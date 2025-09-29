use gstreamer::glib;
use gstreamer::{self as gst, prelude::*};
use gstreamer_video::{
    prelude::{VideoOverlayExt, VideoOverlayExtManual},
    VideoOverlay,
};
use std::sync::Arc;

use crate::gstplayflags::gst_play_flags::GstPlayFlags;

use crate::{Error, Result, WaylandIntegration, WaylandSubsurfaceManager};
use subwave_core::video::types::Position;

pub struct SubsurfacePipeline {
    speed: f64,
    pub pipeline: Arc<gst::Pipeline>,
}

impl SubsurfacePipeline {
    pub fn send_select_streams(&self, ids: &[String]) -> bool {
        let evt = gst::event::SelectStreams::new(ids.iter().map(|s| s.as_str()));
        if self.pipeline.send_event(evt) {
            return true;
        }
        false
    }

    pub fn new(
        uri: &url::Url,
        subsurface: &Arc<WaylandSubsurfaceManager>,
        integration: &WaylandIntegration,
        bounds: (i32, i32, i32, i32),
    ) -> Result<Self> {
        gst::init()?;

        let pipeline = gst::ElementFactory::make("playbin3")
            .name("playbin3")
            .property("message-forward", true)
            .property("async-handling", true)
            //.property("connection-speed", 500000u64)
            // CRITICAL NOTE: Must be larger than video-sink-bin's queue2 buffer
            //.property("buffer-duration", 10_000_000_000i64) // 5s
            //.property("buffer-size", 3_000_000i32)
            .property("buffer-duration", 6_000_000_000i64) // 5s
            //.property("buffer-size", 6_000_000i32)
            .property("ring-buffer-max-size", 536870912u64)
            //.property("delay", 500_000_000u64)
            .build()
            .map_err(|_| Error::Pipeline("Failed to create playbin3 element".to_string()))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| {
                Error::Pipeline("Failed to downcast to pipeline from playbin3".to_string())
            })?;

        pipeline.set_property("uri", uri.as_str());

        pipeline.set_property("flags", GstPlayFlags::wayland_native());

        let video_sink = gst::ElementFactory::make("waylandsink")
            .name("vsink")
            .property("async", true)
            .property("sync", true)
            // Setting too high causes stuttering, will have adjust dynamically to optimize
            //.property("blocksize", 500_000u32)
            .build()
            .map_err(|err| {
                println!("Failed to build video sink: {}", err);
                Error::Pipeline("Failed to build video sink".to_string())
            })?;

        // Create and set the Wayland display context
        const WAYLAND_DISPLAY_HANDLE_CONTEXT_TYPE: &str = "GstWaylandDisplayHandleContextType";

        let mut context = gst::Context::new(WAYLAND_DISPLAY_HANDLE_CONTEXT_TYPE, true);
        {
            let context = context.get_mut().unwrap();
            let structure = context.structure_mut();

            log::debug!(
                "Setting display pointer in context: {:p}",
                integration.display as *const _
            );

            unsafe {
                use glib::translate::{ToGlibPtr, ToGlibPtrMut};
                use gstreamer::ffi as gst_ffi;

                let mut value = glib::Value::from_type(glib::Type::POINTER);
                glib::gobject_ffi::g_value_set_pointer(
                    value.to_glib_none_mut().0,
                    integration.display,
                );

                gst_ffi::gst_structure_set_value(
                    structure.as_ptr() as *mut gst_ffi::GstStructure,
                    c"display".as_ptr(),
                    value.to_glib_none().0,
                );
            }
        }

        video_sink.set_context(&context);
        log::debug!("Wayland display context set on pipeline");

        log::debug!("Setting initial subsurface size (will be updated by widget)");
        subsurface.set_position(0, 0);
        let init_w = bounds.2.max(1);
        let init_h = bounds.3.max(1);
        log::info!("[subs] Initial size from bounds: {}x{}", init_w, init_h);
        subsurface.set_size(init_w, init_h); // Use provided bounds or minimum 1x1
                                             // Proactively inform subtitle worker of initial size

        // Now get the surface handle - it should have the correct size
        let surface_handle = subsurface.surface_handle();
        log::debug!(
            "Setting waylandsink to use subsurface handle: 0x{:x}",
            surface_handle
        );

        let video_overlay = video_sink
            .dynamic_cast_ref::<VideoOverlay>()
            .ok_or_else(|| Error::Pipeline("waylandsink doesn't implement VideoOverlay".into()))?;

        unsafe {
            video_overlay.set_window_handle(surface_handle);
            if let Err(err) =
                video_overlay.set_render_rectangle(bounds.0, bounds.1, bounds.2, bounds.3)
            {
                log::debug!("[ERROR] Failed to set initial render rectangle: {}", err);
            }
            video_overlay.expose();
            video_overlay.handle_events(false);
        }

        video_sink.set_property("fullscreen", false); // Fullscreen true causes freeze with subsurface

        if video_sink.has_property("force-aspect-ratio") {
            video_sink.set_property("force-aspect-ratio", false);
        }

        let vsink_bin = gst::Bin::with_name("waylandsink-bin");

        // Insert a buffering queue to decouple upstream reconfiguration when subtitles are enabled
        //let queue2 = gst::ElementFactory::make("queue2")
        //    .name("video-buffer")
        //    .property("use-buffering", true)
        //    //.property("low-watermark", 0.25f64)
        //    //.property("high-watermark", 0.85f64)
        //    .property("max-size-buffers", 20u32)
        //    //.property("max-size-time", 6_000_000_000u64) // 5s
        //    //.property("max-size-bytes", 4_000_000u32)
        //    .property("max-size-bytes", 0u32)
        //    //.property("ring-buffer-max-size", 536870912u64)
        //    //.property("use-bitrate-query", true)
        //    //.property("use-rate-estimate", true)
        //    .build()
        //    .map_err(|err| {
        //        println!("Failed to build video buffer queue2: {}", err);
        //        Error::Pipeline("Failed to build queue2 for video sink".to_string())
        //    })?;

        let vapostproc = gst::ElementFactory::make("vapostproc")
            .name("vapostproc")
            // Causes significant artifacting
            .property("add-borders", false)
            // Fixes washed out hdr
            .property("disable-passthrough", true)
            .build()
            .map_err(|err| {
                println!("Failed to build video sink: {}", err);
                Error::Pipeline("Failed to build video sink".to_string())
            })?;

        // Should enable tone mapping on supported hardware
        if vapostproc.has_property("hdr-tone-mapping") {
            vapostproc.set_property("hdr-tone-mapping", true);
        }

        vsink_bin
            .add_many([(&vapostproc), &video_sink])
            .map_err(|e| {
                Error::Pipeline(format!("Failed to add elements to video-sink bin: {}", e))
            })?;
        gst::Element::link_many([(&vapostproc), &video_sink])
            .map_err(|e| Error::Pipeline(format!("Failed to link video-sink chain: {}", e)))?;

        // Create and add a ghost pad so playbin3 can link video into this bin through the buffer
        let ghost_pad = gst::GhostPad::with_target(&vapostproc.static_pad("sink").unwrap())
            .map_err(|e| {
                Error::Pipeline(format!("Failed to create ghost pad for text-sink: {}", e))
            })?;

        vsink_bin.add_pad(&ghost_pad).map_err(|e| {
            Error::Pipeline(format!("Failed to add ghost pad to video-sink: {}", e))
        })?;

        vsink_bin.set_property("message_forward", true);
        // Should still test this
        vsink_bin.set_property("async-handling", false);

        pipeline.set_property("video-sink", vsink_bin);

        subsurface.force_damage_and_commit();
        subsurface.flush()?;
        log::debug!("Forced damage and committed subsurface");
        log::debug!("Pipeline ready for playback in PAUSED state");

        // Enable debug subtitle overlay if env var is set
        //let debug_subs = std::env::var_os("SUBWAVE_DEBUG_SUBS").is_some();

        Ok(Self {
            speed: 1.0,
            pipeline: Arc::new(pipeline),
        })
    }

    /// Start playback
    pub fn play(&self) -> Result<()> {
        let current_state = self.pipeline.current_state();
        log::debug!("play() called, current state: {:?}", current_state);

        // First, ensure we're in PAUSED state (this triggers dynamic pad creation)
        if current_state != gst::State::Paused && current_state != gst::State::Playing {
            log::debug!("Setting pipeline to PAUSED for preroll...");

            match self.pipeline.set_state(gst::State::Paused) {
                Ok(gst::StateChangeSuccess::Success) => {
                    log::debug!("Synchronous state change to PAUSED");
                }
                Ok(gst::StateChangeSuccess::Async) => {
                    log::debug!("Async state change to PAUSED, waiting for completion...");

                    // Wait for preroll with a timeout
                    let (result, state, pending) =
                        self.pipeline.state(gst::ClockTime::from_seconds(10));
                    log::debug!(
                        "After wait: result={:?}, state={:?}, pending={:?}",
                        result,
                        state,
                        pending
                    );

                    if state != gst::State::Paused {
                        // Check for errors on the bus
                        if let Some(bus) = self.pipeline.bus() {
                            while let Some(msg) = bus.pop() {
                                use gst::MessageView;
                                if let MessageView::Error(err) = msg.view() {
                                    log::debug!("Error during preroll: {:?}", err);
                                    return Err(Error::Pipeline(format!(
                                        "Preroll error: {:?}",
                                        err
                                    )));
                                }
                            }
                        }

                        // If still not in PAUSED after timeout, warn but continue
                        log::debug!(
                            "Warning: Failed to reach PAUSED state, attempting to play anyway"
                        );
                    } else {
                        log::debug!("Successfully prerolled to PAUSED");
                    }
                }
                Ok(gst::StateChangeSuccess::NoPreroll) => {
                    log::debug!("No preroll needed (live source)");
                }
                Err(e) => {
                    log::debug!("Failed to set PAUSED state: {:?}", e);
                    return Err(Error::Pipeline(format!("Failed to pause: {:?}", e)));
                }
            }
        }

        // Now transition to PLAYING
        log::debug!("Setting pipeline to PLAYING...");
        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| Error::Pipeline(format!("Failed to play: {:?}", e)))?;

        log::debug!("Successfully set to Playing state");
        Ok(())
    }

    /// Pause playback
    pub fn pause(&self) -> Result<()> {
        let current_state = self.pipeline.current_state();
        log::debug!(
            "Attempting to pause pipeline from state: {:?}",
            current_state
        );

        // If in Null state, first go to Ready
        if current_state == gst::State::Null {
            log::debug!("Pipeline in Null state, transitioning to Ready first...");
            self.pipeline
                .set_state(gst::State::Ready)
                .map_err(|e| Error::Pipeline(format!("Failed to set Ready state: {:?}", e)))?;
        }

        let result = self.pipeline.set_state(gst::State::Paused);
        log::debug!("set_state(Paused) returned: {:?}", result);

        match result {
            Ok(state_change) => {
                log::debug!("State change success: {:?}", state_change);

                // Wait a bit for state change to complete
                let (res, state, pending) = self.pipeline.state(gst::ClockTime::from_seconds(1));
                log::debug!(
                    "After pause - Result: {:?}, State: {:?}, Pending: {:?}",
                    res,
                    state,
                    pending
                );

                Ok(())
            }
            Err(e) => {
                log::debug!("Failed to pause: {:?}", e);

                // Try to get more debug info
                let (res, state, pending) = self.pipeline.state(gst::ClockTime::from_seconds(0));
                log::debug!(
                    "Current state - Result: {:?}, State: {:?}, Pending: {:?}",
                    res,
                    state,
                    pending
                );

                Err(Error::Pipeline(format!("Failed to pause: {:?}", e)))
            }
        }
    }

    /// Stop playback
    pub fn stop(&self) -> Result<()> {
        // Finally, stop the overall pipeline
        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|e| Error::Pipeline(format!("Failed to stop: {:?}", e)))?;
        Ok(())
    }

    /// Seek to a specific position
    pub fn seek(&self, position: impl Into<Position>, _accurate: bool) -> Result<()> {
        let position = position.into();

        let flags = gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT; //| gst::SeekFlags::TRICKMODE; // | gst::SeekFlags::ACCURATE; // No point accurate seeking for video playback

        // Perform the seek
        match &position {
            Position::Time(time) => {
                let seek_pos = gst::ClockTime::from_nseconds(time.as_nanos() as u64);
                self.pipeline
                    .seek(
                        self.speed,
                        flags,
                        gst::SeekType::Set,
                        seek_pos,
                        gst::SeekType::None,
                        gst::ClockTime::NONE,
                    )
                    .map_err(|err| Error::Pipeline(format!("Failed to seek to time: {}", err)))
            }
            Position::Frame(_) => self
                .pipeline
                .seek(
                    self.speed,
                    flags,
                    gst::SeekType::Set,
                    gst::GenericFormattedValue::from(position),
                    gst::SeekType::None,
                    gst::format::Default::NONE,
                )
                .map_err(|err| Error::Pipeline(format!("Failed to seek to time: {}", err))),
        }
    }

    /// Check if the pipeline is playing
    #[allow(dead_code)]
    pub fn is_playing(&self) -> bool {
        matches!(self.pipeline.current_state(), gst::State::Playing)
    }

    /// Set up bus message handling
    pub fn bus(&self) -> Option<gst::Bus> {
        self.pipeline.bus()
    }

    /// Set the volume of the pipeline (0.0 to 1.0)
    pub fn set_volume(&self, volume_level: f64) -> Result<()> {
        self.pipeline.set_property("volume", volume_level);
        Ok(())
    }

    /// Update the render rectangle for the video output
    pub fn set_render_rectangle(&self, x: i32, y: i32, width: i32, height: i32) {
        if let Some(video_sink) = self.pipeline.by_name("vsink") {
            if let Some(video_overlay) = video_sink.dynamic_cast_ref::<VideoOverlay>() {
                // Safe to call - this updates where waylandsink renders within the surface
                if let Err(e) = video_overlay.set_render_rectangle(x, y, width, height) {
                    log::error!("Failed to update render rectangle: {}", e);
                } else {
                    log::debug!(
                        "Updated render rectangle to x={}, y={}, w={}, h={}",
                        x,
                        y,
                        width,
                        height
                    );
                }
                video_overlay.expose();
                video_overlay.handle_events(true); // Still don't know what this does or if it's necessary
            }
        }
    }

    /// Set the playback rate (speed)
    pub fn set_playback_rate(&self, rate: f64) -> Result<()> {
        // Get current position for the seek
        let position = self
            .pipeline
            .query_position::<gst::ClockTime>()
            .ok_or_else(|| Error::Pipeline("Failed to query position".into()))?;

        // Perform seek with new rate
        let flags = gst::SeekFlags::FLUSH;

        self.pipeline
            .seek(
                rate,
                flags,
                gst::SeekType::Set,
                position,
                gst::SeekType::None,
                gst::ClockTime::NONE,
            )
            .map_err(|e| Error::Pipeline(format!("Failed to set playback rate: {:?}", e)))?;

        Ok(())
    }

    /// Get the current audio track index
    #[allow(dead_code)]
    pub fn current_audio_track(&self) -> i32 {
        // For now, return the currently selected audio track
        // This would need to be tracked properly in a real implementation
        self.pipeline.property::<i32>("current-audio")
    }

    /// Get the number of available audio tracks
    #[allow(dead_code)]
    pub fn n_audio(&self) -> i32 {
        self.pipeline.property::<i32>("n-audio")
    }

    /// Select an audio track by index
    #[allow(dead_code)]
    pub fn select_audio_track(&self, track_index: i32) -> Result<()> {
        self.pipeline.set_property("current-audio", track_index);
        Ok(())
    }
}

impl Drop for SubsurfacePipeline {
    fn drop(&mut self) {
        log::debug!("Beginning cleanup");

        // First, stop the pipeline
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            log::error!("Error: Failed to set state to Null during cleanup: {:?}", e);
        }

        // Wait for state change to complete
        let _ = self.pipeline.state(gst::ClockTime::from_seconds(1));

        log::debug!("Cleanup completed");
    }
}
