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

/// Build a `GstWaylandDisplayHandleContextType` context carrying `display`.
///
/// `display_addr` is the raw `wl_display*` pointer cast to `usize`.
fn wayland_display_context(display_addr: usize) -> gst::Context {
    const CTX_TYPE: &str = "GstWaylandDisplayHandleContextType";

    let mut context = gst::Context::new(CTX_TYPE, true);
    {
        let context = context.get_mut().unwrap();
        let structure = context.structure_mut();
        unsafe {
            use glib::translate::{ToGlibPtr, ToGlibPtrMut};
            use gstreamer::ffi as gst_ffi;

            let mut value = glib::Value::from_type(glib::Type::POINTER);
            glib::gobject_ffi::g_value_set_pointer(
                value.to_glib_none_mut().0,
                display_addr as *mut std::ffi::c_void,
            );

            gst_ffi::gst_structure_set_value(
                structure.as_ptr() as *mut gst_ffi::GstStructure,
                c"display".as_ptr(),
                value.to_glib_none().0,
            );
        }
    }
    context
}

fn env_flag_enabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            match v.as_str() {
                "" | "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                _ => true,
            }
        }
        Err(_) => false,
    }
}

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
            .property("buffer-duration", 6_000_000_000i64)
            .property("ring-buffer-max-size", 536870912u64)
            .build()
            .map_err(|_| Error::Pipeline("Failed to create playbin3 element".to_string()))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| {
                Error::Pipeline("Failed to downcast to pipeline from playbin3".to_string())
            })?;

        pipeline.set_property("uri", uri.as_str());

        let play_flags = GstPlayFlags::wayland_native();
        let disable_text = env_flag_enabled("SUBWAVE_DISABLE_TEXT");
        log::info!(
            "[pipeline] playbin flags={} (SUBWAVE_DISABLE_TEXT={})",
            play_flags,
            disable_text
        );
        pipeline.set_property("flags", play_flags);

        // Belt-and-suspenders for debug mode: explicitly disable current text selection.
        if disable_text && pipeline.has_property("current-text") {
            pipeline.set_property("current-text", -1i32);
        }

        // ── Build waylandsink ──────────────────────────────────────────
        // Do NOT set display context or window handle here.
        // GStreamer 1.28's waylandsink starts a background Wayland
        // event-dispatch thread inside gst_wl_display_new_existing()
        // the moment set_context() is called. Doing this while iced's
        // event loop is running (we are inside draw()) races with the
        // main Wayland event loop and segfaults.
        //
        // Instead we install a bus *sync handler* (below) that provides
        // the display context and window handle exactly when waylandsink
        // asks for them during the state transition.  This matches the
        // official GStreamer waylandsink integration example.
        let video_sink = gst::ElementFactory::make("waylandsink")
            .name("vsink")
            .property("async", true)
            .property("sync", true)
            .build()
            .map_err(|err| {
                log::error!("Failed to build waylandsink: {}", err);
                Error::Pipeline("Failed to build waylandsink".to_string())
            })?;

        video_sink.set_property("fullscreen", false);

        if video_sink.has_property("force-aspect-ratio") {
            video_sink.set_property("force-aspect-ratio", false);
        }

        // ── Build vapostproc ───────────────────────────────────────────
        let vapostproc = gst::ElementFactory::make("vapostproc")
            .name("vapostproc")
            .property("add-borders", false)
            .property("disable-passthrough", true)
            .build()
            .map_err(|err| {
                log::error!("Failed to build vapostproc: {}", err);
                Error::Pipeline("Failed to build vapostproc".to_string())
            })?;

        if vapostproc.has_property("hdr-tone-mapping") {
            vapostproc.set_property("hdr-tone-mapping", true);
        }

        // ── Assemble video-sink bin ────────────────────────────────────
        let vsink_bin = gst::Bin::with_name("waylandsink-bin");

        vsink_bin
            .add_many([&vapostproc, &video_sink])
            .map_err(|e| {
                Error::Pipeline(format!("Failed to add elements to video-sink bin: {}", e))
            })?;
        gst::Element::link_many([&vapostproc, &video_sink])
            .map_err(|e| Error::Pipeline(format!("Failed to link video-sink chain: {}", e)))?;

        let ghost_pad = gst::GhostPad::with_target(&vapostproc.static_pad("sink").unwrap())
            .map_err(|e| {
                Error::Pipeline(format!("Failed to create ghost pad for video-sink: {}", e))
            })?;

        vsink_bin.add_pad(&ghost_pad).map_err(|e| {
            Error::Pipeline(format!("Failed to add ghost pad to video-sink: {}", e))
        })?;

        vsink_bin.set_property("message_forward", true);
        vsink_bin.set_property("async-handling", false);

        pipeline.set_property("video-sink", vsink_bin);

        // ── Prepare subsurface geometry ────────────────────────────────
        log::debug!("Setting initial subsurface size (will be updated by widget)");
        subsurface.set_position(0, 0);
        let init_w = bounds.2.max(1);
        let init_h = bounds.3.max(1);
        log::info!("[subs] Initial size from bounds: {}x{}", init_w, init_h);
        subsurface.set_size(init_w, init_h);

        subsurface.force_damage_and_commit();
        subsurface.flush()?;
        log::debug!("Forced damage and committed subsurface");

        // ── Bus sync handler ───────────────────────────────────────────
        // Runs synchronously on the GStreamer streaming thread whenever a
        // message is posted.  We intercept the two Wayland-specific
        // messages and provide the handles just-in-time, matching the
        // pattern used in GStreamer's own waylandsink GTK example.
        // Store pointer addresses as usize so the closure is Send+Sync.
        // The Wayland objects behind these addresses live as long as
        // WaylandSubsurfaceManager, which outlives the pipeline.
        let display_addr: usize = integration.display as usize;
        let surface_handle: usize = subsurface.surface_handle();
        let init_bounds = bounds;

        if let Some(bus) = pipeline.bus() {
            bus.set_sync_handler(move |_bus, msg| {
                match msg.view() {
                    // waylandsink posts NEED_CONTEXT during NULL→READY to
                    // ask for an external wl_display handle.
                    gst::MessageView::NeedContext(nc) => {
                        let ctx_type = nc.context_type();
                        if ctx_type == "GstWaylandDisplayHandleContextType"
                            || ctx_type == "GstWlDisplayHandleContextType"
                        {
                            log::info!(
                                "[sync] Providing Wayland display context (type={ctx_type})"
                            );
                            let context = wayland_display_context(display_addr);
                            if let Some(src) = msg.src() {
                                if let Some(element) = src.downcast_ref::<gst::Element>() {
                                    element.set_context(&context);
                                }
                            }
                            return gst::BusSyncReply::Drop;
                        }
                    }

                    // waylandsink posts an Element message named
                    // "prepare-window-handle" just before rendering the
                    // first frame.  We respond by supplying the wl_surface
                    // handle and the initial render rectangle.
                    gst::MessageView::Element(el) => {
                        let is_prepare = el
                            .structure()
                            .is_some_and(|s| s.name().as_str() == "prepare-window-handle");
                        if is_prepare {
                            log::info!(
                                "[sync] Providing window handle 0x{:x} and render rect {:?}",
                                surface_handle,
                                init_bounds
                            );
                            if let Some(src) = msg.src() {
                                if let Some(overlay) = src.dynamic_cast_ref::<VideoOverlay>() {
                                    unsafe {
                                        overlay.set_window_handle(surface_handle);
                                        let _ = overlay.set_render_rectangle(
                                            init_bounds.0,
                                            init_bounds.1,
                                            init_bounds.2,
                                            init_bounds.3,
                                        );
                                    }
                                }
                            }
                            return gst::BusSyncReply::Drop;
                        }
                    }

                    _ => {}
                }
                // All other messages pass through to the async bus
                // (where the bus thread in video.rs picks them up).
                gst::BusSyncReply::Pass
            });
        }

        log::debug!("Pipeline ready (sync handler installed, awaiting state change)");

        Ok(Self {
            speed: 1.0,
            pipeline: Arc::new(pipeline),
        })
    }

    /// Start playback
    pub fn play(&self) -> Result<()> {
        let current_state = self.pipeline.current_state();
        log::debug!("play() called, current state: {:?}", current_state);

        // Non-blocking: request PAUSED to trigger preroll if needed, do not wait
        if current_state != gst::State::Paused && current_state != gst::State::Playing {
            if let Err(e) = self.pipeline.set_state(gst::State::Paused) {
                log::debug!("Failed to request PAUSED state: {:?}", e);
                return Err(Error::Pipeline(format!("Failed to pause: {:?}", e)));
            }
        }

        // Immediately request PLAYING; bus thread will observe readiness/AsyncDone
        log::debug!("Requesting PLAYING state (non-blocking)...");
        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| Error::Pipeline(format!("Failed to play: {:?}", e)))?;

        Ok(())
    }

    /// Pause playback
    pub fn pause(&self) -> Result<()> {
        let current_state = self.pipeline.current_state();
        log::debug!("pause() called from state: {:?}", current_state);

        // Non-blocking: request PAUSED and return
        self.pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| Error::Pipeline(format!("Failed to pause: {:?}", e)))?;
        Ok(())
    }

    /// Stop playback
    pub fn stop(&self) -> Result<()> {
        // Stop the pipeline and clear the sync handler to break ref-cycles.
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|e| Error::Pipeline(format!("Failed to stop: {:?}", e)))?;
        Ok(())
    }

    /// Seek to a specific position
    pub fn seek(&self, position: impl Into<Position>, accurate: bool) -> Result<()> {
        let position = position.into();

        let mut flags = gst::SeekFlags::FLUSH;
        if accurate {
            flags |= gst::SeekFlags::ACCURATE;
        } else {
            flags |= gst::SeekFlags::KEY_UNIT;
        }

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

        // Clear the sync handler first to prevent callbacks during teardown
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }

        // Stop the pipeline
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            log::error!("Error: Failed to set state to Null during cleanup: {:?}", e);
        }

        // Wait for state change to complete
        let _ = self.pipeline.state(gst::ClockTime::from_seconds(1));

        log::debug!("Cleanup completed");
    }
}
