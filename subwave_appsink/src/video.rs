use crate::internal::Internal;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use iced::widget::image as img;
use std::num::NonZeroU8;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use subwave_core::Error;
use subwave_core::video::types::{AudioTrack, Position, SubtitleTrack, VideoProperties};
use subwave_core::video::video_trait::Video;

/// A multimedia video loaded from a URI (e.g., a local file path or HTTP stream).
#[derive(Debug)]
pub struct AppsinkVideo(pub(crate) RwLock<Internal>);

impl AppsinkVideo {
    /// Creates a video sink bin with proper buffering for network streams
    fn build_video_sink() -> Result<gst::Element, Error> {
        let bin = gst::Bin::builder().name("video-sink-bin").build();

        // Create the video processing elements
        //let videobalance = gst::ElementFactory::make("videobalance")
        //    .name("video_balance")
        //    .build()
        //    .map_err(|e| {
        //        log::error!("Failed to create videobalance: {:?}", e);
        //        Error::Cast
        //    })?;

        let videoconvertscale = gst::ElementFactory::make("videoconvertscale")
            .property("n-threads", 0u32) // Use multiple threads for conversion
            .build()
            .map_err(|e| {
                log::error!("Failed to create videoconvertscale: {:?}", e);
                Error::Cast
            })?;

        let appsink = gst::ElementFactory::make("appsink")
            .name("subwave_appsink")
            .property("drop", true)
            .property("max-buffers", 3u32)
            .property("sync", true)
            .property("enable-last-sample", false)
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", gst::List::new(["NV12"]))
                    .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                    .build(),
            )
            .build()
            .map_err(|e| {
                log::error!("Failed to create appsink: {:?}", e);
                Error::Cast
            })?;

        // Add elements to bin
        bin.add_many([&videoconvertscale, &appsink]).map_err(|e| {
            log::error!("Failed to add elements to bin: {:?}", e);
            Error::Cast
        })?;

        // Link elements - convert first, then scale, then balance
        gst::Element::link_many([&videoconvertscale, &appsink]).map_err(|e| {
            log::error!("Failed to link elements: {:?}", e);
            Error::Cast
        })?;

        // Create ghost pad
        let sink_pad = videoconvertscale.static_pad("sink").ok_or_else(|| {
            log::error!("Failed to get sink pad from videoconvertscale");
            Error::Cast
        })?;

        let ghost_pad = gst::GhostPad::with_target(&sink_pad).map_err(|e| {
            log::error!("Failed to create ghost pad: {:?}", e);
            Error::Cast
        })?;

        ghost_pad.set_active(true).map_err(|e| {
            log::error!("Failed to activate ghost pad: {:?}", e);
            Error::Cast
        })?;

        bin.add_pad(&ghost_pad).map_err(|e| {
            log::error!("Failed to add ghost pad to bin: {:?}", e);
            Error::Cast
        })?;

        log::debug!("Successfully created video sink bin");
        Ok(bin.upcast())
    }

    /// Creates a new video based on an existing GStreamer pipeline and appsink.
    /// Expects an `appsink` plugin with `caps=video/x-raw,format=NV12`.
    ///
    /// **Note:** Many functions of [`Video`] assume a `playbin` pipeline.
    /// Non-`playbin` pipelines given here may not have full functionality.
    pub fn from_gst_pipeline(
        pipeline: gst::Pipeline,
        video_sink: gst_app::AppSink,
    ) -> Result<Self, Error> {
        gst::init()?;
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);

        // We need to ensure we stop the pipeline if we hit an error,
        // or else there may be audio left playing in the background.
        macro_rules! cleanup {
            ($expr:expr) => {
                $expr.map_err(|e| {
                    let _ = pipeline.set_state(gst::State::Null);
                    e
                })
            };
        }

        let pad = video_sink.pads().first().cloned().unwrap();

        log::debug!("Setting pipeline to PLAYING state");
        match pipeline.set_state(gst::State::Playing) {
            Ok(state_change) => {
                log::debug!("State change result: {:?}", state_change);
            }
            Err(e) => {
                log::error!("Failed to set pipeline to PLAYING: {:?}", e);

                // Get more details about the error
                if let Some(bus) = pipeline.bus() {
                    while let Some(msg) = bus.pop() {
                        log::error!("Bus message: {:?}", msg);
                    }
                }

                cleanup!(Err(e))?;
            }
        }

        // wait for up to 5 seconds until the decoder gets the source capabilities
        log::debug!("Waiting for pipeline to reach PLAYING state");
        let state_result = pipeline.state(gst::ClockTime::from_seconds(5));
        match state_result {
            (Ok(state_change), current, pending) => {
                log::debug!(
                    "Pipeline state: current={:?}, pending={:?}, change={:?}",
                    current,
                    pending,
                    state_change
                );
            }
            (Err(e), current, pending) => {
                log::error!(
                    "Pipeline state error: current={:?}, pending={:?}, error={:?}",
                    current,
                    pending,
                    e
                );
                cleanup!(Err(e))?;
            }
        }

        // For playbin3 with complex pipelines, caps might not be available immediately
        // We'll start with defaults and update them when we get the first sample
        log::info!("Deferring video caps extraction until first sample arrives");
        let (mut width, mut height, mut framerate, has_video) = (1920, 1080, 30.0, true);

        // Try to get initial caps if available
        if let Some(caps) = pad.current_caps() {
            log::debug!("Initial caps available: {:?}", caps);
            if let Some(s) = caps.structure(0)
                && let (Ok(w), Ok(h), Ok(fr)) = (
                    s.get::<i32>("width"),
                    s.get::<i32>("height"),
                    s.get::<gst::Fraction>("framerate"),
                ) {
                    width = ((w + 4 - 1) / 4) * 4;
                    height = h;
                    framerate = fr.numer() as f64 / fr.denom() as f64;
                    log::info!(
                        "Got initial video properties: {}x{} @ {}fps",
                        width,
                        height,
                        framerate
                    );
                }
        } else {
            log::debug!("No initial caps available, will update on first sample");
        }

        if has_video
            && (framerate.is_nan()
                || framerate.is_infinite()
                || framerate < 0.0
                || framerate.abs() < f64::EPSILON)
        {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(Error::Framerate(framerate));
        }

        let duration = Duration::from_nanos(
            pipeline
                .query_duration::<gst::ClockTime>()
                .map(|duration| duration.nseconds())
                .unwrap_or(0),
        );

        // For network streams, duration might not be available immediately
        if duration.as_secs() == 0 {
            log::info!("Duration not available yet, will update later");
        }

        let sync_av = pipeline.has_property("av-offset");

        // NV12 = 12bpp
        let frame = Arc::new(Mutex::new(vec![
            0u8;
            (width as usize * height as usize * 3)
                .div_ceil(2)
        ]));
        let upload_frame = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let last_frame_time = Arc::new(Mutex::new(Instant::now()));

        let video_props = Arc::new(Mutex::new(VideoProperties {
            width,
            height,
            framerate,
            has_video,
        }));

        // For HDR metadata detection
        //let hdr_metadata_shared = Arc::new(Mutex::new(None::<HdrMetadata>));

        let frame_ref = Arc::clone(&frame);
        let upload_frame_ref = Arc::clone(&upload_frame);
        let alive_ref = Arc::clone(&alive);
        let last_frame_time_ref = Arc::clone(&last_frame_time);
        let video_props_ref = Arc::clone(&video_props);

        let pipeline_ref = pipeline.clone();

        let worker = std::thread::spawn(move || {
            let mut caps_checked = false;

            while alive_ref.load(Ordering::Acquire) {
                if let Err(gst::FlowError::Error) = (|| -> Result<(), gst::FlowError> {
                    let sample =
                        if pipeline_ref.state(gst::ClockTime::ZERO).1 != gst::State::Playing {
                            video_sink
                                .try_pull_preroll(gst::ClockTime::from_mseconds(16))
                                .ok_or(gst::FlowError::Eos)?
                        } else {
                            video_sink
                                .try_pull_sample(gst::ClockTime::from_mseconds(16))
                                .ok_or(gst::FlowError::Eos)?
                        };

                    // Update video properties from the first sample with caps
                    if !caps_checked
                        && let Some(caps) = sample.caps() {
                            log::debug!("Got caps from sample: {:?}", caps);

                            if let Some(s) = caps.structure(0)
                                && let (Ok(w), Ok(h), Ok(fr)) = (
                                    s.get::<i32>("width"),
                                    s.get::<i32>("height"),
                                    s.get::<gst::Fraction>("framerate"),
                                ) {
                                    let mut props = video_props_ref
                                        .lock()
                                        .map_err(|_| gst::FlowError::Error)?;
                                    props.width = ((w + 4 - 1) / 4) * 4;
                                    props.height = h;
                                    props.framerate = fr.numer() as f64 / fr.denom() as f64;
                                    props.has_video = true;
                                    log::info!(
                                        "Updated video properties from sample: {}x{} @ {}fps",
                                        props.width,
                                        props.height,
                                        props.framerate
                                    );

                                    // Recreate frame buffer with correct size
                                    let new_size =
                                        (props.width as usize * props.height as usize * 3)
                                            .div_ceil(2);
                                    let mut frame_guard =
                                        frame_ref.lock().map_err(|_| gst::FlowError::Error)?;
                                    frame_guard.resize(new_size, 0);
                                    drop(frame_guard);
                                    drop(props);
                                }
                            caps_checked = true;
                        }

                    *last_frame_time_ref
                        .lock()
                        .map_err(|_| gst::FlowError::Error)? = Instant::now();

                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                    let mut frame = frame_ref.lock().map_err(|_| gst::FlowError::Error)?;
                    let frame_len = frame.len();
                    if map.len() >= frame_len {
                        frame.copy_from_slice(&map.as_slice()[..frame_len]);
                    }

                    upload_frame_ref.swap(true, Ordering::SeqCst);

                    Ok(())
                })() {
                    log::error!("error pulling frame");
                }
            }
        });

        Ok(AppsinkVideo(RwLock::new(Internal {
            id,

            bus: pipeline.bus().unwrap(),
            source: pipeline,
            alive,
            worker: Some(worker),

            video_props,
            duration,
            speed: 1.0,
            sync_av,

            frame,
            upload_frame,
            last_frame_time,
            looping: false,
            is_eos: false,
            restart_stream: false,
            sync_av_avg: 0,
            sync_av_counter: 0,

            seek_position: None,
            last_valid_position: Duration::ZERO,

            is_buffering: false,
            buffering_percent: 100,
            user_paused: false,

            current_bitrate: 0,
            avg_in_rate: 0,

            last_error_time: None,
            error_count: 0,
            is_reconnecting: false,

            available_subtitles: Vec::new(),
            current_subtitle_track: None,
            subtitles_enabled: false,

            available_audio_tracks: Vec::new(),
            current_audio_track: 0,

            stream_collection: None,
            selected_stream_ids: Vec::new(),
            //hdr_metadata: hdr_metadata_shared
            //    .lock()
            //    .ok()
            //    .and_then(|guard| guard.clone()),
        })))
    }

    pub(crate) fn read(&self) -> impl Deref<Target = Internal> + '_ {
        self.0.read().expect("lock")
    }

    pub(crate) fn write(&self) -> impl DerefMut<Target = Internal> + '_ {
        self.0.write().expect("lock")
    }

    pub(crate) fn get_mut(&mut self) -> impl DerefMut<Target = Internal> + '_ {
        self.0.get_mut().expect("lock")
    }

    /// Generates a list of thumbnails based on a set of positions in the media, downscaled by a given factor.
    ///
    /// Slow; only needs to be called once for each instance.
    /// It's best to call this at the very start of playback, otherwise the position may shift.
    fn thumbnails<I>(
        &mut self,
        positions: I,
        downscale: NonZeroU8,
    ) -> Result<Vec<img::Handle>, Error>
    where
        I: IntoIterator<Item = Position>,
    {
        let downscale = u8::from(downscale) as u32;

        let paused = self.paused();
        let muted = self.muted();
        let pos = self.position();

        self.set_paused(false);
        self.set_muted(true);

        let out = {
            let mut inner = self.get_mut();
            let props = inner.video_props.lock().expect("lock video props");
            let width = props.width;
            let height = props.height;
            drop(props);

            positions
                .into_iter()
                .map(|pos| {
                    inner.seek(pos, true)?;
                    inner.upload_frame.store(false, Ordering::SeqCst);
                    while !inner.upload_frame.load(Ordering::SeqCst) {
                        std::hint::spin_loop();
                    }
                    let frame_guard = inner.frame.lock().map_err(|_| Error::Lock)?;

                    Ok(img::Handle::from_rgba(
                        width as u32 / downscale,
                        height as u32 / downscale,
                        yuv_to_rgba(&frame_guard, width as _, height as _, downscale),
                    ))
                })
                .collect()
        };

        self.set_paused(paused);
        self.set_muted(muted);
        self.seek(pos, true)?;

        out
    }
}

impl Video for AppsinkVideo {
    type Video = AppsinkVideo;

    /// Create a new video player from a given video which loads from `uri`.
    /// Note that live sources will report the duration to be zero.
    fn new(uri: &url::Url) -> Result<Self, Error> {
        gst::init()?;

        //let is_network_stream = uri.scheme() == "http" || uri.scheme() == "https";

        // Create video sink bin
        let video_sink_bin = match Self::build_video_sink() {
            Ok(sink) => sink,
            Err(e) => {
                log::error!(
                    "Failed to create buffered sink, falling back to string pipeline builder: {:?}",
                    e
                );
                gst::parse::bin_from_description(
                        "videoconvertscale n-threads=0 ! appsink name=iced_video drop=true caps=\"video/x-raw,format=(string){NV12},pixel-aspect-ratio=1/1\"",
                        true
                    )?.upcast()
            }
        };

        let pipeline = gst::ElementFactory::make("playbin3")
            .property("uri", uri.as_str())
            .property("video-sink", &video_sink_bin)
            .build()?
            .downcast::<gst::Pipeline>()
            .map_err(|_| Error::Cast)?;

        // Add scaletempo for pitch correction during variable playback speed
        if let Ok(scaletempo) = gst::ElementFactory::make("scaletempo")
            .name("pitch-corrector")
            .build()
        {
            pipeline.set_property("audio-filter", &scaletempo);
            log::info!("Enabled pitch correction for variable playback speed");
        } else {
            log::warn!("scaletempo element not available - pitch correction disabled");
        }

        let video_sink_opt: Option<gst::Element> = pipeline.property("video-sink");
        let video_sink = match video_sink_opt {
            Some(e) => e,
            None => {
                log::error!("video-sink property is None on pipeline");
                return Err(Error::Cast);
            }
        };
        let video_sink_bin = video_sink.downcast::<gst::Bin>().map_err(|_| {
            log::error!("Failed to downcast video-sink to Bin");
            Error::Cast
        })?;
        let video_sink = video_sink_bin.by_name("subwave_appsink").ok_or_else(|| {
            log::error!("Failed to find 'iced_video' element in video sink bin");
            Error::Cast
        })?;
        let video_sink = video_sink.downcast::<gst_app::AppSink>().map_err(|_| {
            log::error!("Failed to downcast to AppSink");
            Error::Cast
        })?;

        Self::from_gst_pipeline(pipeline, video_sink)
    }

    /// Get the size/resolution of the video as `(width, height)`.
    fn size(&self) -> (i32, i32) {
        let inner = self.read();
        let props = inner.video_props.lock().expect("lock video props");
        (props.width, props.height)
    }

    /// Get the framerate of the video as frames per second.
    fn framerate(&self) -> f64 {
        let inner = self.read();
        let props = inner.video_props.lock().expect("lock video props");
        props.framerate
    }

    /// Set the volume multiplier of the audio.
    /// `0.0` = 0% volume, `1.0` = 100% volume.
    ///
    /// This uses a linear scale, for example `0.5` is perceived as half as loud.
    fn set_volume(&mut self, volume: f64) {
        self.get_mut().source.set_property("volume", volume);
        self.set_muted(self.muted()); // for some reason gstreamer unmutes when changing volume?
    }

    /// Get the volume multiplier of the audio.
    fn volume(&self) -> f64 {
        self.read().source.property("volume")
    }

    /// Set if the audio is muted or not, without changing the volume.
    fn set_muted(&mut self, muted: bool) {
        self.get_mut().source.set_property("mute", muted);
    }

    /// Get if the audio is muted or not.
    fn muted(&self) -> bool {
        self.read().source.property("mute")
    }

    /// Get if the stream ended or not.
    fn eos(&self) -> bool {
        self.read().is_eos
    }

    /// Get if the media will loop or not.
    fn looping(&self) -> bool {
        self.read().looping
    }

    /// Set if the media will loop or not.
    fn set_looping(&mut self, looping: bool) {
        self.get_mut().looping = looping;
    }

    /// Set if the media is paused or not.
    fn set_paused(&mut self, paused: bool) {
        self.get_mut().set_paused(paused)
    }

    /// Get if the media is paused or not.
    fn paused(&self) -> bool {
        self.read().paused()
    }

    /// Jumps to a specific position in the media.
    /// Passing `true` to the `accurate` parameter will result in more accurate seeking,
    /// however, it is also slower. For most seeks (e.g., scrubbing) this is not needed.
    fn seek(&mut self, position: impl Into<Position>, accurate: bool) -> Result<(), Error> {
        self.get_mut().seek(position, accurate)
    }

    /// Set the playback speed of the media.
    /// The default speed is `1.0`.
    fn set_speed(&mut self, speed: f64) -> Result<(), Error> {
        self.get_mut().set_speed(speed)
    }

    /// Get the current playback speed.
    fn speed(&self) -> f64 {
        self.read().speed
    }

    /// Get the current playback position in time.
    fn position(&self) -> Duration {
        let inner = self.read();

        // Check pipeline state first
        let (state_change, current, _) = inner.source.state(gst::ClockTime::ZERO);

        // During state changes or when pipeline is not ready, use cached position
        if state_change.is_err()
            || matches!(state_change, Ok(gst::StateChangeSuccess::Async))
            || current < gst::State::Paused
        {
            return inner.last_valid_position;
        }

        // Query position when pipeline is stable
        if let Some(pos) = inner.source.query_position::<gst::ClockTime>() {
            Duration::from_nanos(pos.nseconds())
        } else {
            // Return last known position if query fails
            inner.last_valid_position
        }
    }

    /// Get the media duration.
    fn duration(&self) -> Duration {
        self.read().duration
    }

    /// Restarts a stream; seeks to the first frame and unpauses, sets the `eos` flag to false.
    fn restart_stream(&mut self) -> Result<(), Error> {
        self.get_mut().restart_stream()
    }

    /// Set the subtitle URL to display.
    fn set_subtitle_url(&mut self, url: &url::Url) -> Result<(), Error> {
        let paused = self.paused();
        let mut inner = self.get_mut();
        inner.source.set_state(gst::State::Ready)?;
        inner.source.set_property("suburi", url.as_str());
        inner.set_paused(paused);
        Ok(())
    }

    /// Get the current subtitle URL.
    fn subtitle_url(&self) -> Option<url::Url> {
        self.read()
            .source
            .property::<Option<String>>("suburi")
            .and_then(|s| url::Url::parse(&s).ok())
    }

    /// Get the underlying GStreamer pipeline.
    fn pipeline(&self) -> gst::Pipeline {
        self.read().source.clone()
    }

    /// Get the list of available subtitle tracks
    fn subtitle_tracks(&mut self) -> Vec<SubtitleTrack> {
        self.get_mut().query_subtitle_tracks()
    }

    /// Select a specific subtitle track by index, or None to disable subtitles
    fn select_subtitle_track(&mut self, track_index: Option<i32>) -> Result<(), Error> {
        self.get_mut().select_subtitle_track(track_index)
    }

    /// Get the currently selected subtitle track index
    fn current_subtitle_track(&self) -> Option<i32> {
        self.read().current_subtitle_track
    }

    /// Enable or disable subtitle display
    fn set_subtitles_enabled(&mut self, enabled: bool) {
        self.get_mut().set_subtitles_enabled(enabled)
    }

    /// Check if subtitles are enabled
    fn subtitles_enabled(&self) -> bool {
        self.read().subtitles_enabled
    }

    /// Get the list of available audio tracks
    fn audio_tracks(&mut self) -> Vec<AudioTrack> {
        self.get_mut().query_audio_tracks()
    }

    /// Select a specific audio track by index
    fn select_audio_track(&mut self, track_index: i32) -> Result<(), Error> {
        self.get_mut().select_audio_track(track_index)
    }

    /// Get the currently selected audio track index
    fn current_audio_track(&self) -> i32 {
        self.read().current_audio_track
    }

    /// Check if the video has video tracks (not just audio)
    fn has_video(&self) -> bool {
        let inner = self.read();
        let props = inner.video_props.lock().expect("lock video props");
        props.has_video
    }
}

fn yuv_to_rgba(yuv: &[u8], width: u32, height: u32, downscale: u32) -> Vec<u8> {
    let uv_start = width * height;
    let mut rgba = vec![];

    for y in 0..height / downscale {
        for x in 0..width / downscale {
            let x_src = x * downscale;
            let y_src = y * downscale;

            let uv_i = uv_start + width * (y_src / 2) + x_src / 2 * 2;

            let y = yuv[(y_src * width + x_src) as usize] as f32;
            let u = yuv[uv_i as usize] as f32;
            let v = yuv[(uv_i + 1) as usize] as f32;

            let r = 1.164 * (y - 16.0) + 1.596 * (v - 128.0);
            let g = 1.164 * (y - 16.0) - 0.813 * (v - 128.0) - 0.391 * (u - 128.0);
            let b = 1.164 * (y - 16.0) + 2.018 * (u - 128.0);

            rgba.push(r as u8);
            rgba.push(g as u8);
            rgba.push(b as u8);
            rgba.push(0xFF);
        }
    }

    rgba
}

impl Drop for AppsinkVideo {
    fn drop(&mut self) {
        let inner = self.0.get_mut().expect("failed to lock");

        inner
            .source
            .set_state(gst::State::Null)
            .expect("failed to set state");

        inner.alive.store(false, Ordering::SeqCst);
        if let Some(worker) = inner.worker.take() {
            worker.join().expect("failed to stop video thread");
        }
    }
}
