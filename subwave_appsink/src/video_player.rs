use crate::{render_pipeline::VideoPrimitive, video::AppsinkVideo};
use gstreamer::{self as gst, glib};
use iced::{
    Element,
    advanced::{self, Widget, layout, widget},
    wgpu::TextureFormat,
};
use iced_wgpu::primitive::Renderer as PrimitiveRenderer;
use log::error;
use std::sync::Arc;
use std::{marker::PhantomData, sync::atomic::Ordering, time::Instant};
use subwave_core::video::video_trait::Video;

/// Video player widget which displays the current frame of a [`Video`](crate::Video).
pub struct VideoPlayer<'a, Message, Theme = iced::Theme, Renderer = iced::Renderer>
where
    Renderer: PrimitiveRenderer,
{
    video: &'a AppsinkVideo,
    content_fit: iced::ContentFit,
    width: iced::Length,
    height: iced::Length,
    on_end_of_stream: Option<Message>,
    on_new_frame: Option<Message>,
    on_error: Option<Box<dyn Fn(&glib::Error) -> Message + 'a>>,
    on_buffering: Option<Box<dyn Fn(i32) -> Message + 'a>>,
    _phantom: PhantomData<(Theme, Renderer)>,
}

impl<'a, Message, Theme, Renderer> VideoPlayer<'a, Message, Theme, Renderer>
where
    Renderer: PrimitiveRenderer,
{
    /// Creates a new video player widget for a given video.
    pub fn new(video: &'a AppsinkVideo) -> Self {
        VideoPlayer {
            video,
            content_fit: iced::ContentFit::default(),
            width: iced::Length::Shrink,
            height: iced::Length::Shrink,
            on_end_of_stream: None,
            on_new_frame: None,
            on_error: None,
            on_buffering: None,
            _phantom: Default::default(),
        }
    }

    /// Sets the width of the `VideoPlayer` boundaries.
    pub fn width(self, width: impl Into<iced::Length>) -> Self {
        VideoPlayer {
            width: width.into(),
            ..self
        }
    }

    /// Sets the height of the `VideoPlayer` boundaries.
    pub fn height(self, height: impl Into<iced::Length>) -> Self {
        VideoPlayer {
            height: height.into(),
            ..self
        }
    }

    /// Sets the `ContentFit` of the `VideoPlayer`.
    pub fn content_fit(self, content_fit: iced::ContentFit) -> Self {
        VideoPlayer {
            content_fit,
            ..self
        }
    }

    /// Message to send when the video reaches the end of stream (i.e., the video ends).
    pub fn on_end_of_stream(self, on_end_of_stream: Message) -> Self {
        VideoPlayer {
            on_end_of_stream: Some(on_end_of_stream),
            ..self
        }
    }

    /// Message to send when the video receives a new frame.
    pub fn on_new_frame(self, on_new_frame: Message) -> Self {
        VideoPlayer {
            on_new_frame: Some(on_new_frame),
            ..self
        }
    }

    /// Message to send when the video playback encounters an error.
    pub fn on_error<F>(self, on_error: F) -> Self
    where
        F: 'a + Fn(&glib::Error) -> Message,
    {
        VideoPlayer {
            on_error: Some(Box::new(on_error)),
            ..self
        }
    }

    /// Message to send when the video is buffering.
    /// The callback receives the buffering percentage (0-100).
    pub fn on_buffering<F>(self, on_buffering: F) -> Self
    where
        F: 'a + Fn(i32) -> Message,
    {
        VideoPlayer {
            on_buffering: Some(Box::new(on_buffering)),
            ..self
        }
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for VideoPlayer<'_, Message, Theme, Renderer>
where
    Message: Clone,
    Renderer: PrimitiveRenderer,
{
    fn size(&self) -> iced::Size<iced::Length> {
        iced::Size {
            width: iced::Length::Shrink,
            height: iced::Length::Shrink,
        }
    }

    fn layout(
        &self,
        _tree: &mut widget::Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let (video_width, video_height) = self.video.size();

        // based on `Image::layout`
        let image_size = iced::Size::new(video_width as f32, video_height as f32);
        let raw_size = limits.resolve(self.width, self.height, image_size);
        let full_size = self.content_fit.fit(image_size, raw_size);
        let final_size = iced::Size {
            width: match self.width {
                iced::Length::Shrink => f32::min(raw_size.width, full_size.width),
                _ => raw_size.width,
            },
            height: match self.height {
                iced::Length::Shrink => f32::min(raw_size.height, full_size.height),
                _ => raw_size.height,
            },
        };

        layout::Node::new(final_size)
    }

    fn draw(
        &self,
        _tree: &widget::Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &advanced::renderer::Style,
        layout: advanced::Layout<'_>,
        _cursor: advanced::mouse::Cursor,
        _viewport: &iced::Rectangle,
    ) {
        let mut inner = self.video.write();

        // bounds based on `Image::draw`
        let props = inner.video_props.lock().expect("lock video props");
        let image_size = iced::Size::new(props.width as f32, props.height as f32);
        drop(props);
        let bounds = layout.bounds();
        let adjusted_fit = self.content_fit.fit(image_size, bounds.size());
        let scale = iced::Vector::new(
            adjusted_fit.width / image_size.width,
            adjusted_fit.height / image_size.height,
        );
        let final_size = image_size * scale;

        let position = match self.content_fit {
            iced::ContentFit::None => iced::Point::new(
                bounds.x + (image_size.width - adjusted_fit.width) / 2.0,
                bounds.y + (image_size.height - adjusted_fit.height) / 2.0,
            ),
            _ => iced::Point::new(
                bounds.center_x() - final_size.width / 2.0,
                bounds.center_y() - final_size.height / 2.0,
            ),
        };

        let drawing_bounds = iced::Rectangle::new(position, final_size);

        let upload_frame = inner.upload_frame.swap(false, Ordering::SeqCst);

        if upload_frame {
            let last_frame_time = inner
                .last_frame_time
                .lock()
                .map(|time| *time)
                .unwrap_or_else(|_| Instant::now());
            inner.set_av_offset(Instant::now() - last_frame_time);
        }

        let render = |renderer: &mut Renderer| {
            let props = inner.video_props.lock().expect("lock video props");
            let dims = (props.width as _, props.height as _);
            drop(props);

            renderer.draw_primitive(
                drawing_bounds,
                VideoPrimitive::new(
                    inner.id,
                    Arc::clone(&inner.alive),
                    Arc::clone(&inner.frame),
                    dims,
                    upload_frame,
                    // Use the same format as the surface; iced will pass it to our prepare()
                    // This argument is ignored by our pipeline creation and replaced with actual surface format
                    TextureFormat::Bgra8UnormSrgb,
                ),
            );
        };

        if adjusted_fit.width > bounds.width || adjusted_fit.height > bounds.height {
            renderer.with_layer(bounds, render);
        } else {
            render(renderer);
        }
    }

    fn update(
        &mut self,
        _state: &mut widget::Tree,
        event: &iced::Event,
        _layout: advanced::Layout<'_>,
        _cursor: advanced::mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn advanced::Clipboard,
        shell: &mut advanced::Shell<'_, Message>,
        _viewport: &iced::Rectangle,
    ) {
        let mut inner = self.video.write();

        if let iced::Event::Window(iced::window::Event::RedrawRequested(_)) = &event {
            if inner.restart_stream || (!inner.is_eos && !inner.paused()) {
                let mut restart_stream = false;
                if inner.restart_stream {
                    restart_stream = true;
                    // Set flag to false to avoid potentially multiple seeks
                    inner.restart_stream = false;
                }
                let mut eos_pause = false;

                while let Some(msg) = inner.bus.pop_filtered(&[
                    gst::MessageType::Error,
                    gst::MessageType::Eos,
                    gst::MessageType::AsyncDone,
                    gst::MessageType::StateChanged,
                    gst::MessageType::Buffering,
                    gst::MessageType::StreamCollection,
                ]) {
                    match msg.view() {
                        gst::MessageView::Error(err) => {
                            error!("bus returned an error: {err}");
                            let gst_error = err.error();

                            // Check if we should retry on this error
                            if inner.should_retry_on_error(&gst_error) {
                                log::info!(
                                    "Network error detected, scheduling reconnection attempt"
                                );

                                // Schedule reconnection on next frame
                                // We can't reconnect immediately in the message handler
                                inner.is_reconnecting = true;
                            } else {
                                // Non-recoverable error, notify the application
                                if let Some(ref on_error) = self.on_error {
                                    shell.publish(on_error(&gst_error));
                                }
                            }
                        }
                        gst::MessageView::Eos(_eos) => {
                            if let Some(on_end_of_stream) = self.on_end_of_stream.clone() {
                                shell.publish(on_end_of_stream);
                            }
                            if inner.looping {
                                restart_stream = true;
                            } else {
                                eos_pause = true;
                            }
                        }
                        gst::MessageView::AsyncDone(_) => {
                            log::debug!("GStreamer AsyncDone message received - seek completed");
                            // Clear the cached seek position
                            inner.seek_position = None;
                        }
                        gst::MessageView::StateChanged(state_changed) => {
                            if state_changed
                                .src()
                                .map(|s| s == &inner.source)
                                .unwrap_or(false)
                            {
                                log::debug!(
                                    "Pipeline state changed: {:?} -> {:?}",
                                    state_changed.old(),
                                    state_changed.current()
                                );
                            }
                        }
                        gst::MessageView::Buffering(_buffering) => {
                            /*
                            let percent = buffering.percent();
                            log::debug!("Buffering: {}%", percent);

                            // Update buffering state
                            inner.buffering_percent = percent;

                            // Send buffering message to UI
                            if let Some(ref on_buffering) = self.on_buffering {
                                shell.publish(on_buffering(percent));
                            }

                            if percent < 100 {
                                // Start buffering
                                if !inner.is_buffering {
                                    inner.is_buffering = true;
                                    // Pause playback if not already paused by user
                                    if !inner.user_paused
                                        && inner.source.current_state() == gst::State::Playing
                                    {
                                        inner.source.set_state(gst::State::Paused).ok();
                                        log::info!("Pausing for buffering at {}%", percent);
                                    }
                                }
                            } else {
                                // Buffering complete
                                if inner.is_buffering {
                                    inner.is_buffering = false;
                                    // Resume playback if not paused by user
                                    if !inner.user_paused {
                                        inner.source.set_state(gst::State::Playing).ok();
                                        log::info!("Resuming after buffering complete");
                                    }
                                }
                            } */
                        }
                        gst::MessageView::StreamCollection(stream_collection) => {
                            log::info!("Received StreamCollection message");

                            let collection = stream_collection.stream_collection();
                            // Update the stream collection in our video state
                            inner.update_stream_collection(collection);

                            // Send stream selection event to select default streams
                            if let Err(e) = inner.send_stream_selection() {
                                log::error!("Failed to send stream selection: {:?}", e);
                            }
                        }
                        _ => {}
                    }
                }

                // Don't run eos_pause if restart_stream is true; fixes "pausing" after restarting a stream
                if restart_stream {
                    if let Err(err) = inner.restart_stream() {
                        error!("cannot restart stream (can't seek): {err:#?}");
                    }
                } else if eos_pause {
                    inner.is_eos = true;
                    inner.set_paused(true);
                }

                // Handle reconnection attempts after network errors
                if inner.is_reconnecting {
                    inner.is_reconnecting = false;
                    if let Err(e) = inner.attempt_reconnect() {
                        log::error!("Reconnection attempt failed: {:?}", e);
                        // Notify the application about the failure
                        if let Some(ref on_error) = self.on_error {
                            shell.publish(on_error(&glib::Error::new(
                                gst::CoreError::Failed,
                                &format!("Failed to reconnect: {:?}", e),
                            )));
                        }
                    }
                }

                if inner.upload_frame.load(Ordering::SeqCst) {
                    // Reset error state on successful frame
                    inner.reset_error_state();
                    if let Some(on_new_frame) = self.on_new_frame.clone() {
                        shell.publish(on_new_frame);
                    }
                    // Update position cache when we get a new frame
                    inner.update_position_cache();

                    // Periodically update connection stats for network streams
                    static mut STATS_COUNTER: u64 = 0;
                    unsafe {
                        STATS_COUNTER += 1;
                        if STATS_COUNTER.is_multiple_of(60) {
                            // Every ~60 frames (roughly 1-2 seconds)
                            inner.update_connection_stats();
                        }
                    }
                }

                shell.request_redraw();
            } else {
                shell.request_redraw();
            }
        }
    }
}

impl<'a, Message, Theme, Renderer> From<VideoPlayer<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a + Clone,
    Theme: 'a,
    Renderer: 'a + PrimitiveRenderer,
{
    fn from(video_player: VideoPlayer<'a, Message, Theme, Renderer>) -> Self {
        Self::new(video_player)
    }
}
