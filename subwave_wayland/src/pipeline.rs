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

/// A Send+Sync handle for pushing subtitle frames to the subsurface from
/// GStreamer streaming threads.
///
/// `WaylandSubsurfaceManager` is `!Send` because `WaylandIntegration`
/// contains raw `*mut c_void` pointers.  The subtitle methods only touch
/// thread-safe fields (Mutex-guarded buffers, Wayland proxies).
/// We use a raw pointer to erase the non-Send inner type.
/// Wrapper around a raw pointer that is Send+Sync.
/// Used to pass WaylandSubsurfaceManager references into GStreamer
/// callbacks that require Send+Sync closures.
#[derive(Clone, Copy)]
struct SendPtr(*const WaylandSubsurfaceManager);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

struct SubtitleWriter(SendPtr);

impl SubtitleWriter {
    fn new(mgr: &Arc<WaylandSubsurfaceManager>) -> Self {
        Self(SendPtr(Arc::as_ptr(mgr)))
    }

    fn ptr(&self) -> SendPtr {
        self.0
    }

    fn get(&self) -> &WaylandSubsurfaceManager {
        // SAFETY: The WaylandSubsurfaceManager Arc is alive for the
        // lifetime of the pipeline (enforced by video.rs ownership).
        unsafe { &*self.0.0 }
    }
}

enum SubtitlePadKind {
    Pgs,
    Text,
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
        compositor_has_cm: bool,
        active_sub_stream_id: &Arc<parking_lot::Mutex<Option<String>>>,
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
        log::info!("[pipeline] playbin flags={}", play_flags);
        pipeline.set_property("flags", play_flags);

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
            if compositor_has_cm {
                // Compositor supports color management — let HDR pixels pass
                // through to waylandsink untouched.  The compositor will do
                // the tone-mapping using the image description we set on the
                // surface via wp-color-management-v1.
                vapostproc.set_property("hdr-tone-mapping", false);
                log::info!(
                    "[pipeline] vapostproc hdr-tone-mapping DISABLED (compositor has CM)"
                );
            } else {
                // No compositor CM — vapostproc must tone-map HDR→SDR itself.
                vapostproc.set_property("hdr-tone-mapping", true);
                log::info!(
                    "[pipeline] vapostproc hdr-tone-mapping ENABLED (no compositor CM)"
                );
            }
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

        // ── Subtitle interception (PGS + text/x-raw) ──────────────────
        // All subtitle rendering is handled out-of-band via demuxer pad
        // probes.  Each probe reads its pad's stream-id and only decodes
        // when it matches `active_sub_stream_id`, so switching languages
        // activates exactly one probe.  playbin3 never sees TEXT in its
        // flags, so subtitleoverlay is never involved and HDR passthrough
        // is preserved.
        Self::install_subtitle_probes(&pipeline, subsurface, active_sub_stream_id);

        log::debug!("Pipeline ready (sync handler installed, subtitle probes armed, awaiting state change)");

        Ok(Self {
            speed: 1.0,
            pipeline: Arc::new(pipeline),
        })
    }

    // ── Unified subtitle interception (PGS + text/x-raw) ────────────
    //
    // All subtitle rendering bypasses playbin3's subtitleoverlay to
    // preserve HDR video passthrough.  We install pad probes on the
    // demuxer's source pads and intercept subtitle buffers directly.
    //
    // Each probe reads its pad's stream-id (from the STREAM_START sticky
    // event) and compares it against `active_sub_stream_id`.  Only the
    // probe whose stream-id matches will decode/render — all others
    // return Ok immediately, so switching languages is instantaneous.

    /// Read the stream-id from a pad's sticky STREAM_START event.
    fn pad_stream_id(pad: &gst::Pad) -> Option<String> {
        pad.sticky_event::<gst::event::StreamStart>(0)
            .and_then(|ev| {
                let sid = ev.stream_id();
                if sid.is_empty() { None } else { Some(sid.to_string()) }
            })
    }

    /// Classify a demuxer source pad by its caps.
    fn classify_pad(pad: &gst::Pad) -> Option<SubtitlePadKind> {
        if pad.direction() != gst::PadDirection::Src {
            return None;
        }
        let caps = pad.current_caps().or_else(|| {
            pad.pad_template().map(|t| t.caps().to_owned())
        });
        let name = caps.as_ref()
            .and_then(|c| c.structure(0))
            .map(|s| s.name().as_str().to_string())?;

        if name == "subpicture/x-pgs" || name == "subpicture/x-dvd" {
            Some(SubtitlePadKind::Pgs)
        } else if name.starts_with("text/") {
            Some(SubtitlePadKind::Text)
        } else {
            None
        }
    }

    /// Install probes on every demuxer subtitle pad (PGS and text).
    fn install_subtitle_probes(
        pipeline: &gst::Pipeline,
        subsurface: &Arc<WaylandSubsurfaceManager>,
        active_id: &Arc<parking_lot::Mutex<Option<String>>>,
    ) {
        let sw_ptr = SubtitleWriter::new(subsurface).ptr();
        let active = Arc::clone(active_id);

        pipeline.connect_deep_element_added(move |_pipeline, _bin, element| {
            let factory_name = element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default();

            // Only watch demuxer elements for subtitle pads.
            if !factory_name.contains("demux") {
                return;
            }

            let element_name = element.name().to_string();
            log::info!("[subs] Watching demuxer {element_name} for subtitle pads");

            let sw = sw_ptr;
            let active_inner = Arc::clone(&active);

            // Probe existing source pads
            for pad in element.src_pads() {
                Self::maybe_attach_subtitle_probe(&pad, sw, &active_inner, &element_name);
            }

            // Watch for dynamic pads
            let active_dyn = Arc::clone(&active_inner);
            element.connect_pad_added(move |el, pad| {
                if pad.direction() != gst::PadDirection::Src {
                    return;
                }
                Self::maybe_attach_subtitle_probe(pad, sw, &active_dyn, &el.name().to_string());
            });
        });
    }

    fn maybe_attach_subtitle_probe(
        pad: &gst::Pad,
        sw: SendPtr,
        active: &Arc<parking_lot::Mutex<Option<String>>>,
        element_name: &str,
    ) {
        match Self::classify_pad(pad) {
            Some(SubtitlePadKind::Pgs) => {
                log::info!("[subs] Attaching PGS probe on {element_name}:{}", pad.name());
                Self::attach_pgs_probe(pad, sw, active);
            }
            Some(SubtitlePadKind::Text) => {
                log::info!("[subs] Attaching text probe on {element_name}:{}", pad.name());
                Self::attach_text_probe(pad, sw, active);
            }
            None => {}
        }
    }

    /// PGS/bitmap subtitle probe — decodes PGS segments to ARGB and
    /// pushes to the subtitle subsurface.
    fn attach_pgs_probe(
        pad: &gst::Pad,
        sw: SendPtr,
        active: &Arc<parking_lot::Mutex<Option<String>>>,
    ) {
        let writer = SubtitleWriter(sw);
        let decoder = std::sync::Mutex::new(crate::pgs_decoder::PgsDecoder::new());
        let active_id = Arc::clone(active);
        // Snapshot this pad's stream-id once caps are known.  For
        // dynamic pads the STREAM_START event may not have arrived yet,
        // so we lazily resolve it on the first buffer.
        let pad_sid: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(Self::pad_stream_id(pad));

        pad.add_probe(gst::PadProbeType::BUFFER, move |probe_pad, info| {
            // Lazily resolve stream-id if we didn't get it at install time.
            {
                let mut sid_guard = pad_sid.lock().unwrap();
                if sid_guard.is_none() {
                    *sid_guard = Self::pad_stream_id(probe_pad);
                }
                // Check whether this pad is the active subtitle track.
                let selected = active_id.lock();
                match (sid_guard.as_deref(), selected.as_deref()) {
                    (Some(mine), Some(want)) if mine == want => {} // active — continue
                    _ => return gst::PadProbeReturn::Ok,           // not selected — skip
                }
            }

            let Some(buffer) = info.buffer() else {
                return gst::PadProbeReturn::Ok;
            };
            let Ok(map) = buffer.map_readable() else {
                return gst::PadProbeReturn::Ok;
            };

            log::debug!("[pgs-probe] Buffer: {} bytes", map.len());

            let mut dec = decoder.lock().unwrap();
            if let Some(frames) = dec.feed(map.as_slice()) {
                let mgr = writer.get();
                if frames.is_empty() {
                    let _ = mgr.clear_subtitle();
                } else {
                    let (surf_w, surf_h) = mgr.get_size();
                    let surf_w = surf_w.max(1) as usize;
                    let surf_h = surf_h.max(1) as usize;

                    let pgs_w = dec.video_width.max(1) as f64;
                    let pgs_h = dec.video_height.max(1) as f64;
                    let scale_x = surf_w as f64 / pgs_w;
                    let scale_y = surf_h as f64 / pgs_h;

                    let stride = surf_w * 4;
                    let mut canvas = vec![0u8; stride * surf_h];

                    for frame in &frames {
                        let fx = (frame.x as f64 * scale_x) as usize;
                        let fy = (frame.y as f64 * scale_y) as usize;
                        let fw = frame.width as usize;
                        let fh = frame.height as usize;
                        let scaled_fw = ((frame.width as f64) * scale_x) as usize;
                        let scaled_fh = ((frame.height as f64) * scale_y) as usize;
                        let src_stride = fw * 4;

                        for dy in 0..scaled_fh {
                            let canvas_y = fy + dy;
                            if canvas_y >= surf_h { break; }
                            let src_row = (dy as f64 / scale_y) as usize;
                            if src_row >= fh { break; }
                            let s_off = src_row * src_stride;

                            for dx in 0..scaled_fw {
                                let canvas_x = fx + dx;
                                if canvas_x >= surf_w { break; }
                                let src_col = (dx as f64 / scale_x) as usize;
                                if src_col >= fw { break; }

                                let s_px = s_off + src_col * 4;
                                let d_px = canvas_y * stride + canvas_x * 4;
                                if s_px + 4 <= frame.argb.len() && d_px + 4 <= canvas.len() {
                                    canvas[d_px..d_px + 4]
                                        .copy_from_slice(&frame.argb[s_px..s_px + 4]);
                                }
                            }
                        }
                    }

                    let _ = mgr.attach_subtitle_frame(
                        &canvas,
                        surf_w as i32,
                        surf_h as i32,
                        stride as i32,
                    );
                }
            }

            gst::PadProbeReturn::Ok
        });
    }

    /// Text subtitle probe — renders text/x-raw to ARGB and pushes
    /// to the subtitle subsurface.
    fn attach_text_probe(
        pad: &gst::Pad,
        sw: SendPtr,
        active: &Arc<parking_lot::Mutex<Option<String>>>,
    ) {
        let writer = SubtitleWriter(sw);
        let active_id = Arc::clone(active);
        let pad_sid: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(Self::pad_stream_id(pad));
        let renderer: std::sync::Mutex<Option<crate::text_renderer::TextRenderer>> =
            std::sync::Mutex::new(crate::text_renderer::TextRenderer::new());

        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |probe_pad, info| {
                // Lazily resolve stream-id.
                {
                    let mut sid_guard = pad_sid.lock().unwrap();
                    if sid_guard.is_none() {
                        *sid_guard = Self::pad_stream_id(probe_pad);
                    }
                    let selected = active_id.lock();
                    match (sid_guard.as_deref(), selected.as_deref()) {
                        (Some(mine), Some(want)) if mine == want => {}
                        _ => return gst::PadProbeReturn::Ok,
                    }
                }

                match &info.data {
                    Some(gst::PadProbeData::Buffer(buffer)) => {
                        let Ok(map) = buffer.map_readable() else {
                            return gst::PadProbeReturn::Ok;
                        };
                        let text = String::from_utf8_lossy(map.as_slice());
                        let mgr = writer.get();

                        if text.trim().is_empty() {
                            let _ = mgr.clear_subtitle();
                            return gst::PadProbeReturn::Ok;
                        }

                        log::debug!(
                            "[text-probe] subtitle: {}...",
                            &text[..text.len().min(80)]
                        );

                        let rend = renderer.lock().unwrap();
                        if let Some(ref r) = *rend {
                            let (w, h) = mgr.get_size();
                            let w = w.max(1) as usize;
                            let h = h.max(1) as usize;
                            if let Some(argb) = r.render(&text, w, h) {
                                let _ = mgr.attach_subtitle_frame(
                                    &argb,
                                    w as i32,
                                    h as i32,
                                    (w * 4) as i32,
                                );
                            }
                        } else {
                            log::warn!("[text-probe] No font available — cannot render text subtitles");
                        }

                        gst::PadProbeReturn::Ok
                    }
                    Some(gst::PadProbeData::Event(ev)) => {
                        if matches!(
                            ev.type_(),
                            gst::EventType::Gap | gst::EventType::FlushStart
                        ) {
                            let _ = writer.get().clear_subtitle();
                        }
                        gst::PadProbeReturn::Ok
                    }
                    _ => gst::PadProbeReturn::Ok,
                }
            },
        );
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
