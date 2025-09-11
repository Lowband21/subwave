use crate::SubsurfaceVideo;
use gstreamer::glib;

type OnError<'a, Message> = Box<dyn Fn(&glib::Error) -> Message + 'a>;
use iced::{
    advanced::{self, layout, widget::Widget},
    ContentFit, Element, Event, Length, Rectangle, Size,
};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

/// A video player widget that reserves space in iced's layout
/// The actual rendering happens through a Wayland subsurface
///
/// Note: This widget requires the wgpu renderer and Wayland platform
pub struct VideoPlayer<'a, Message, Theme = iced::Theme> {
    video: &'a Arc<Mutex<Option<Box<SubsurfaceVideo>>>>,
    _content_fit: ContentFit,
    width: Length,
    height: Length,
    _on_end_of_stream: Option<Message>,
    _on_error: Option<OnError<'a, Message>>,
    on_new_frame: Option<Message>,
    _phantom: PhantomData<Theme>,
}

impl<'a, Message, Theme> VideoPlayer<'a, Message, Theme> {
    /// Create a new video player widget for the given video
    pub fn new(video: &'a Arc<Mutex<Option<Box<SubsurfaceVideo>>>>) -> Self {
        Self {
            video,
            _content_fit: ContentFit::Contain,
            width: Length::Fill,
            height: Length::Fill,
            _on_end_of_stream: None,
            _on_error: None,
            on_new_frame: None,
            _phantom: PhantomData,
        }
    }

    /// Create a new video player widget from a direct video reference
    pub fn from(video: &'a Arc<Mutex<Option<Box<SubsurfaceVideo>>>>) -> Self {
        Self::new(video)
    }

    /// Set the width of the video player
    pub fn width(self, width: impl Into<Length>) -> Self {
        VideoPlayer {
            width: width.into(),
            ..self
        }
    }

    /// Set the height of the video player
    pub fn height(self, height: impl Into<Length>) -> Self {
        VideoPlayer {
            height: height.into(),
            ..self
        }
    }

    /// Set the content fit mode
    pub fn content_fit(self, content_fit: ContentFit) -> Self {
        VideoPlayer {
            _content_fit: content_fit,
            ..self
        }
    }

    /// Set a message to emit when the video reaches end of stream
    pub fn on_end_of_stream(self, on_end_of_stream: Message) -> Self {
        VideoPlayer {
            _on_end_of_stream: Some(on_end_of_stream),
            ..self
        }
    }

    pub fn on_error<F>(self, on_error: F) -> Self
    where
        F: 'a + Fn(&glib::Error) -> Message,
    {
        VideoPlayer {
            _on_error: Some(Box::new(on_error)),
            ..self
        }
    }

    /// Set a message to emit on an interval rather than based on frame rate
    /// due to our video rendering being inherently decoupled from iced logic
    pub fn on_new_frame(self, on_new_frame: Message) -> Self {
        VideoPlayer {
            on_new_frame: Some(on_new_frame),
            ..self
        }
    }
}

impl<'a, Message, Theme> Widget<Message, Theme, iced_wgpu::Renderer>
    for VideoPlayer<'a, Message, Theme>
where
    Message: Clone,
    Theme: 'a,
{
    fn size(&self) -> Size<Length> {
        Size::new(self.width, self.height)
    }

    /// Use the widget's requested size (Fill means use all available space)
    /// Don't use video dimensions for widget sizing - the video will be fitted within the widget
    fn layout(
        &self,
        _tree: &mut advanced::widget::Tree,
        _renderer: &iced_wgpu::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.resolve(self.width, self.height, Size::ZERO);

        layout::Node::new(size)
    }

    fn update(
        &mut self,
        _state: &mut advanced::widget::Tree,
        event: &Event,
        _layout: advanced::Layout<'_>,
        _cursor: advanced::mouse::Cursor,
        _renderer: &iced_wgpu::Renderer,
        _clipboard: &mut dyn advanced::Clipboard,
        shell: &mut advanced::Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        // Handle redraw events to check for position updates
        if let Event::Window(iced::window::Event::RedrawRequested(_)) = event {
            // Check if video is available and process position updates
            if let Ok(guard) = self.video.lock() {
                if let Some(video) = guard.as_ref() {
                    // Only emit new frame message if the video is playing
                    // and enough time has passed since last update (100ms throttling)
                    if video.is_playing() {
                        // Check if 100ms has passed since last position update
                        let should_update = video
                            .should_emit_on_new_frame(std::time::Duration::from_millis(1000));

                        // Emit new frame message if configured and timing is right
                        // This allows the player to update position/duration
                        if should_update {
                            if let Some(on_new_frame) = self.on_new_frame.clone() {
                                shell.publish(on_new_frame);
                            }
                        }

                        // Todo: determine whether this is needed
                        // Request another redraw to keep updates flowing while playing
                        shell.request_redraw();
                    }
                }
            }
        }
    }

    fn draw(
        &self,
        _tree: &advanced::widget::Tree,
        _renderer: &mut iced_wgpu::Renderer,
        _theme: &Theme,
        _style: &advanced::renderer::Style,
        layout: advanced::Layout<'_>,
        _cursor: advanced::mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let video_available = if let Ok(guard) = self.video.lock() {
            guard.is_some()
        } else {
            false
        };

        if !video_available {
            log::debug!("Video not yet available, skipping draw");
            return;
        }

        let initialized = if let Ok(guard) = self.video.lock() {
            if let Some(video) = guard.as_ref() {
                // Check if the video has a subsurface (indicates it's initialized)
                video.get_subsurface().is_some()
            } else {
                false
            }
        } else {
            false
        };

        // Get the layout bounds before the closure
        let window_bounds = layout.bounds();

        // Only initialize if not already initialized
        if !initialized {
            log::debug!("Video not yet initialized with Wayland, initializing...");

            let integration_result = initialize(self, window_bounds);

            if integration_result.is_none() {
                log::debug!("No Wayland integration available from iced_winit!");
                log::debug!("This likely means the thread-local wasn't set properly");
            } else {
                log::debug!("Successfully processed Wayland integration");
            }
        }

        // TODO: Calculate and pass the correct aspect ratio to the video player pipeline seemlessly
        // We should probably add the element to the pipeline on demand if the user changes the default fit mode
        if let Ok(guard) = self.video.lock() {
            if let Some(video) = guard.as_ref() {
                if let Some(resolution) = video.resolution() {
                    // Validate video dimensions - must be reasonable
                    if resolution.0 < 2 || resolution.1 < 2 {
                        log::debug!(
                            "WARNING: Invalid video dimensions detected: {}x{}, skipping render",
                            resolution.0,
                            resolution.1
                        );
                        return; // Skip this draw call until we have valid dimensions
                    }

                    let _video_width = resolution.0;
                    let _video_height = resolution.1;
                    //let video_aspect = video_width / video_height;

                    let widget_width = window_bounds.width;
                    let widget_height = window_bounds.height;
                    //let widget_aspect = widget_width / widget_height;

                    // Apply the calculated viewport
                    if let Some(subsurface) = video.get_subsurface() {
                        let current_size = subsurface.get_size();
                        let new_width = widget_width.round() as i32;
                        let new_height = widget_height.round() as i32;

                        // Only update if size actually changed and dimensions are valid
                        if current_size != (new_width, new_height)
                            && new_width > 0
                            && new_height > 0
                        {
                            log::info!("Setting new size to {}, {}", new_width, new_height);
                            // Update background subsurface to match widget size
                            subsurface.update_background(new_width, new_height);

                            // Update video subsurface to match widget size
                            subsurface.set_size(new_width, new_height);

                            // Update render rectangle to match widget (gstreamer handles scaling and aspect ratio)
                            video.set_video_size_position(0, 0, new_width, new_height);

                            //subsurface.set_video_surface_opaque_region(
                            //    0,
                            //    0,
                            //    video_width,
                            //    video_height,
                            //);

                            // TODO: Implement proper damage and commit handling
                            subsurface.integration.trigger_pre_commit_hooks();

                            subsurface.force_damage_and_commit();

                            // TODO: Determine correct flush behavior
                            match subsurface.flush() {
                                Ok(_) => (),
                                Err(e) => log::debug!("Error: {:#?}", e),
                            }
                        }

                        // Pump updates (bus commands + subtitles) from the UI thread each draw
                        video.tick();
                    }
                }
            }
        }
    }
}

fn initialize<'a, Message, Theme>(
    video_player: &VideoPlayer<'a, Message, Theme>,
    window_bounds: Rectangle,
) -> Option<()> {
    iced_winit::wayland_integration::wayland::with_current_wayland_integration(|integration| {
        log::debug!("Got Wayland integration from iced_winit!");
        log::debug!(
            "Display ptr: {:p}, Surface ptr: {:p}",
            integration.display as *const _,
            integration.surface as *const _
        );

        // Todo: should we just be using the WaylandIntegration type from iced_winit?
        // Create our WaylandIntegration type
        let our_integration =
            crate::WaylandIntegration::new(integration.surface, integration.display);
        log::debug!("[VideoPlayer] Created our WaylandIntegration wrapper");

        let window_width = window_bounds.width.round() as i32;
        let window_height = window_bounds.height.round() as i32;

        // Calculate the position in window coordinates
        let init_bounds = (0, 0, window_width, window_height);

        log::debug!(
            "Initializing with bounds: x={}, y={}, w={}, h={}",
            init_bounds.0,
            init_bounds.1,
            init_bounds.2,
            init_bounds.3
        );

        // Initialize the video with Wayland integration and bounds
        let init_result = if let Ok(mut guard) = video_player.video.lock() {
            if let Some(video) = guard.as_deref_mut() {
                video.init_wayland(our_integration, init_bounds)
            } else {
                Err(crate::Error::Pipeline("Video not initialized".into()))
            }
        } else {
            Err(crate::Error::Pipeline("Failed to lock video".into()))
        };

        match init_result {
            Ok(()) => {
                log::debug!("Wayland integration initialized successfully");

                // Start playback now that we're initialized and visible
                if let Ok(guard) = video_player.video.lock() {
                    if let Some(video) = guard.as_ref() {
                        // Try to start playback
                        if let Err(e) = video.play() {
                            log::debug!("Failed to start playback: {}", e);
                        }

                        // Initialize background to widget size
                        if let Some(subsurface) = video.get_subsurface() {
                            subsurface.update_background(window_width, window_height);
                            log::debug!(
                                "Initialized background to {}x{}",
                                window_width,
                                window_height
                            );
                        }
                    }
                }
                log::debug!("Video initialized, ready for playback");
            }
            Err(e) => {
                log::error!("Failed to initialize Wayland: {}", e);
            }
        }
    })
}

impl<'a, Message, Theme> From<VideoPlayer<'a, Message, Theme>>
    for Element<'a, Message, Theme, iced_wgpu::Renderer>
where
    Message: Clone + 'a,
    Theme: 'a,
{
    fn from(video_player: VideoPlayer<'a, Message, Theme>) -> Self {
        Self::new(video_player)
    }
}
