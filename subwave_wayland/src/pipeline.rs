use gstreamer::glib;
use gstreamer::{self as gst, prelude::*};
use gstreamer_video::{
    prelude::{VideoOverlayExt, VideoOverlayExtManual},
    VideoOverlay,
};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::gstplayflags::gst_play_flags::GstPlayFlags;

use crate::{
    subtitle_runtime::{
        duration_from_clock_time, ActiveSubtitleSelection, SubtitleProbeEvent,
        WaylandSubtitlePayload,
    },
    Error, Result, WaylandIntegration, WaylandSubsurfaceManager,
};
use subwave_core::video::types::Position;

/// Build a `GstWaylandDisplayHandleContextType` context carrying `display`.
///
/// `display_addr` is the raw `wl_display*` pointer cast to `usize` so it can be
/// captured by GStreamer sync handlers without making the closure `!Send`.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubtitlePadKind {
    Pgs,
    Text,
}

struct PadSubtitleTiming {
    segment: gst::FormattedSegment<gst::ClockTime>,
}

impl PadSubtitleTiming {
    fn new() -> Self {
        Self {
            segment: gst::FormattedSegment::<gst::ClockTime>::new(),
        }
    }

    fn update_segment(&mut self, segment: &gst::FormattedSegment<gst::ClockTime>) {
        self.segment.set_flags(segment.flags());
        self.segment.set_rate(segment.rate());
        self.segment.set_applied_rate(segment.applied_rate());
        self.segment.set_base(segment.base());
        self.segment.set_offset(segment.offset());
        self.segment.set_start(segment.start());
        self.segment.set_stop(segment.stop());
        self.segment.set_time(segment.time());
        self.segment.set_position(segment.position());
        self.segment.set_duration(segment.duration());
    }

    fn running_time_for(&self, timestamp: gst::ClockTime) -> Option<Duration> {
        self.segment
            .to_running_time(timestamp)
            .map(duration_from_clock_time)
    }
}

fn pgs_display_set_event(
    stream_id: String,
    generation: u64,
    start: Duration,
    display_set: crate::pgs_decoder::PgsDisplaySet,
    video_width: u16,
    video_height: u16,
) -> crate::subtitle_scheduler::DecodedSubtitleEvent<WaylandSubtitlePayload> {
    match display_set {
        crate::pgs_decoder::PgsDisplaySet::Clear => {
            crate::subtitle_scheduler::DecodedSubtitleEvent::clear(stream_id, generation, start)
        }
        crate::pgs_decoder::PgsDisplaySet::Show(frames) => {
            // PGS display sets are stateful: a non-empty display set remains visible
            // until a later display set replaces it or an empty composition clears it.
            // The GstBuffer duration describes packet timing, not subtitle visibility.
            crate::subtitle_scheduler::DecodedSubtitleEvent::show(
                stream_id,
                generation,
                start,
                None,
                WaylandSubtitlePayload::Pgs {
                    frames,
                    video_width,
                    video_height,
                },
            )
        }
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
        compositor_has_cm: bool,
        active_subtitle_selection: &Arc<parking_lot::Mutex<ActiveSubtitleSelection>>,
        subtitle_tx: mpsc::Sender<SubtitleProbeEvent>,
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
        log::info!("[pipeline] playbin flags={play_flags}");
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
                log::info!("[pipeline] vapostproc hdr-tone-mapping DISABLED (compositor has CM)");
            } else {
                // No compositor CM — vapostproc must tone-map HDR→SDR itself.
                vapostproc.set_property("hdr-tone-mapping", true);
                log::info!("[pipeline] vapostproc hdr-tone-mapping ENABLED (no compositor CM)");
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

        // Install the Wayland sink sync handler only after the subsurface has
        // valid initial geometry. This follows GStreamer's waylandsink embedding
        // pattern: answer NEED_CONTEXT and prepare-window-handle synchronously,
        // just-in-time during the state transition, instead of eagerly touching
        // waylandsink while iced/winit may be committing the parent surface.
        let display_addr = integration.display as usize;
        let surface_handle = subsurface.surface_handle();
        let init_bounds = (bounds.0, bounds.1, init_w, init_h);
        if let Some(bus) = pipeline.bus() {
            bus.set_sync_handler(move |_bus, msg| {
                match msg.view() {
                    gst::MessageView::NeedContext(need_context) => {
                        let context_type = need_context.context_type();
                        if context_type == "GstWaylandDisplayHandleContextType"
                            || context_type == "GstWlDisplayHandleContextType"
                        {
                            log::info!(
                                "[sync] Providing Wayland display context (type={context_type})"
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
                    gst::MessageView::Element(element) => {
                        let is_prepare_window = element
                            .structure()
                            .is_some_and(|s| s.name().as_str() == "prepare-window-handle");
                        if is_prepare_window {
                            log::info!(
                                "[sync] Providing window handle 0x{surface_handle:x} and render rect {init_bounds:?}"
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
                gst::BusSyncReply::Pass
            });
        }

        Self::install_subtitle_probes(&pipeline, active_subtitle_selection, subtitle_tx);

        log::debug!(
            "Pipeline ready (Wayland sync handler installed, scheduled subtitle probes armed)"
        );

        Ok(Self {
            speed: 1.0,
            pipeline: Arc::new(pipeline),
        })
    }

    // ── Scheduled subtitle interception (PGS + text/x-raw) ────────────
    //
    // Subtitle buffers are intercepted on demuxer source pads and decoded into
    // timestamped cue events.  Pad probes never attach to the Wayland subtitle
    // surface directly; the UI tick consumes these events through the
    // SubtitleScheduler and presents only when media time reaches the cue start.

    fn pad_stream_id(pad: &gst::Pad) -> Option<String> {
        pad.sticky_event::<gst::event::StreamStart>(0)
            .and_then(|event| {
                let stream_id = event.stream_id();
                (!stream_id.is_empty()).then(|| stream_id.to_string())
            })
    }

    fn stream_id_matches(pad_stream_id: &str, selected_stream_id: &str) -> bool {
        pad_stream_id == selected_stream_id
            || selected_stream_id.ends_with(pad_stream_id)
            || pad_stream_id.ends_with(selected_stream_id)
    }

    fn classify_pad(pad: &gst::Pad) -> Option<SubtitlePadKind> {
        if pad.direction() != gst::PadDirection::Src {
            return None;
        }

        let caps = pad.current_caps().or_else(|| {
            pad.pad_template()
                .map(|template| template.caps().to_owned())
        })?;

        let mut text_match = false;
        for structure in caps.iter() {
            let name = structure.name();
            if name == "subpicture/x-pgs" || name == "subpicture/x-dvd" {
                return Some(SubtitlePadKind::Pgs);
            }
            if name.as_str().starts_with("text/") {
                text_match = true;
            }
        }

        text_match.then_some(SubtitlePadKind::Text)
    }

    fn install_subtitle_probes(
        pipeline: &gst::Pipeline,
        active_selection: &Arc<parking_lot::Mutex<ActiveSubtitleSelection>>,
        subtitle_tx: mpsc::Sender<SubtitleProbeEvent>,
    ) {
        let active = Arc::clone(active_selection);

        pipeline.connect_deep_element_added(move |_pipeline, _bin, element| {
            let factory_name = element
                .factory()
                .map(|factory| factory.name().to_string())
                .unwrap_or_default();

            if !factory_name.contains("demux") {
                return;
            }

            let element_name = element.name().to_string();
            log::info!("[subs] Watching demuxer {element_name} for subtitle pads");

            let active_existing = Arc::clone(&active);
            let tx_existing = subtitle_tx.clone();
            for pad in element.src_pads() {
                Self::maybe_attach_subtitle_probe(
                    &pad,
                    &active_existing,
                    tx_existing.clone(),
                    &element_name,
                );
            }

            let active_dynamic = Arc::clone(&active);
            let tx_dynamic = subtitle_tx.clone();
            element.connect_pad_added(move |element, pad| {
                if pad.direction() != gst::PadDirection::Src {
                    return;
                }
                Self::maybe_attach_subtitle_probe(
                    pad,
                    &active_dynamic,
                    tx_dynamic.clone(),
                    element.name().as_ref(),
                );
            });
        });
    }

    fn maybe_attach_subtitle_probe(
        pad: &gst::Pad,
        active: &Arc<parking_lot::Mutex<ActiveSubtitleSelection>>,
        subtitle_tx: mpsc::Sender<SubtitleProbeEvent>,
        element_name: &str,
    ) {
        match Self::classify_pad(pad) {
            Some(SubtitlePadKind::Pgs) => {
                log::info!(
                    "[subs] Attaching scheduled PGS probe on {element_name}:{}",
                    pad.name()
                );
                Self::attach_pgs_probe(pad, active, subtitle_tx);
            }
            Some(SubtitlePadKind::Text) => {
                log::info!(
                    "[subs] Attaching scheduled text probe on {element_name}:{}",
                    pad.name()
                );
                Self::attach_text_probe(pad, active, subtitle_tx);
            }
            None => {}
        }
    }

    fn active_stream_snapshot(
        probe_pad: &gst::Pad,
        pad_stream_id: &Mutex<Option<String>>,
        active: &Arc<parking_lot::Mutex<ActiveSubtitleSelection>>,
        log_target: &str,
    ) -> Option<(String, u64)> {
        // Fast path for startup / subtitles-disabled: do not touch sticky pad
        // events or per-pad mutexes until a subtitle stream is actually active.
        let (selected_stream_id, generation) = {
            let active = active.lock();
            (active.stream_id.clone()?, active.generation)
        };

        let mut stream_id_guard = pad_stream_id.lock().ok()?;
        if stream_id_guard.is_none() {
            if let Some(stream_id) = Self::pad_stream_id(probe_pad) {
                log::info!("[{log_target}] Resolved pad stream-id: {stream_id}");
                *stream_id_guard = Some(stream_id);
            }
        }
        let pad_stream_id = stream_id_guard.clone()?;
        drop(stream_id_guard);

        if Self::stream_id_matches(&pad_stream_id, &selected_stream_id) {
            Some((selected_stream_id, generation))
        } else {
            None
        }
    }

    fn update_pad_stream_id_from_event(
        pad_stream_id: &Mutex<Option<String>>,
        stream_start: &gst::event::StreamStart,
        log_target: &str,
    ) {
        let stream_id = stream_start.stream_id();
        if stream_id.is_empty() {
            return;
        }

        if let Ok(mut guard) = pad_stream_id.lock() {
            log::debug!("[{log_target}] STREAM_START stream-id: {stream_id}");
            *guard = Some(stream_id.to_string());
        }
    }

    fn update_timing_from_segment(
        timing: &Mutex<PadSubtitleTiming>,
        segment: &gst::event::Segment,
        log_target: &str,
    ) {
        let Some(time_segment) = segment.segment().downcast_ref::<gst::ClockTime>() else {
            log::debug!("[{log_target}] Ignoring non-time subtitle segment");
            return;
        };

        if let Ok(mut timing) = timing.lock() {
            timing.update_segment(time_segment);
            log::debug!("[{log_target}] Updated subtitle segment: {time_segment:?}");
        }
    }

    fn buffer_window(
        buffer: &gst::BufferRef,
        timing: &Mutex<PadSubtitleTiming>,
    ) -> Option<(Duration, Option<Duration>)> {
        let timestamp = buffer.pts().or_else(|| buffer.dts())?;
        let start = timing
            .lock()
            .ok()
            .and_then(|timing| timing.running_time_for(timestamp))
            .unwrap_or_else(|| duration_from_clock_time(timestamp));
        let end = buffer
            .duration()
            .and_then(|duration| start.checked_add(duration_from_clock_time(duration)));
        Some((start, end))
    }

    fn running_time_for_duration_timestamp(
        timestamp: Duration,
        timing: &Mutex<PadSubtitleTiming>,
    ) -> Option<Duration> {
        let nanos = u64::try_from(timestamp.as_nanos()).ok()?;
        let timestamp = gst::ClockTime::from_nseconds(nanos);
        timing
            .lock()
            .ok()
            .and_then(|timing| timing.running_time_for(timestamp))
    }

    fn gap_running_time(gap: &gst::event::Gap, timing: &Mutex<PadSubtitleTiming>) -> Duration {
        let (timestamp, _) = gap.get();
        timing
            .lock()
            .ok()
            .and_then(|timing| timing.running_time_for(timestamp))
            .unwrap_or_else(|| duration_from_clock_time(timestamp))
    }

    fn send_invalidate_for_active_stream(
        probe_pad: &gst::Pad,
        pad_stream_id: &Mutex<Option<String>>,
        active: &Arc<parking_lot::Mutex<ActiveSubtitleSelection>>,
        subtitle_tx: &mpsc::Sender<SubtitleProbeEvent>,
        log_target: &str,
    ) {
        if let Some((stream_id, generation)) =
            Self::active_stream_snapshot(probe_pad, pad_stream_id, active, log_target)
        {
            let _ = subtitle_tx.send(SubtitleProbeEvent::Invalidate {
                stream_id,
                generation,
            });
        }
    }

    fn attach_pgs_probe(
        pad: &gst::Pad,
        active: &Arc<parking_lot::Mutex<ActiveSubtitleSelection>>,
        subtitle_tx: mpsc::Sender<SubtitleProbeEvent>,
    ) {
        let active = Arc::clone(active);
        let decoder = Mutex::new(crate::pgs_decoder::PgsDecoder::new());
        let pad_stream_id = Mutex::new(Self::pad_stream_id(pad));
        let timing = Mutex::new(PadSubtitleTiming::new());

        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |probe_pad, info| {
                match &info.data {
                    Some(gst::PadProbeData::Buffer(buffer)) => {
                        let Some((stream_id, generation)) = Self::active_stream_snapshot(
                            probe_pad,
                            &pad_stream_id,
                            &active,
                            "pgs-probe",
                        ) else {
                            return gst::PadProbeReturn::Ok;
                        };
                        let Some((start, _buffer_end)) = Self::buffer_window(buffer, &timing)
                        else {
                            log::debug!("[pgs-probe] Buffer without timestamp; skipping");
                            return gst::PadProbeReturn::Ok;
                        };
                        let Ok(map) = buffer.map_readable() else {
                            return gst::PadProbeReturn::Ok;
                        };

                        let mut decoder = match decoder.lock() {
                            Ok(decoder) => decoder,
                            Err(_) => return gst::PadProbeReturn::Ok,
                        };
                        let display_sets = decoder.feed(map.as_slice());
                        let video_width = decoder.video_width;
                        let video_height = decoder.video_height;
                        for display_set in display_sets {
                            let display_start = display_set
                                .pts
                                .and_then(|pts| Self::running_time_for_duration_timestamp(pts, &timing))
                                .unwrap_or(start);
                            log::debug!(
                                "[pgs-probe] scheduling display set: buffer_start={start:?}, raw_pts={:?}, display_start={display_start:?}",
                                display_set.pts
                            );
                            let event = pgs_display_set_event(
                                stream_id.clone(),
                                generation,
                                display_start,
                                display_set.action,
                                video_width,
                                video_height,
                            );
                            let _ = subtitle_tx.send(SubtitleProbeEvent::Decoded(event));
                        }
                    }
                    Some(gst::PadProbeData::Event(event)) => match event.view() {
                        gst::EventView::StreamStart(stream_start) => {
                            Self::update_pad_stream_id_from_event(
                                &pad_stream_id,
                                stream_start,
                                "pgs-probe",
                            );
                        }
                        gst::EventView::Segment(segment) => {
                            if Self::active_stream_snapshot(
                                probe_pad,
                                &pad_stream_id,
                                &active,
                                "pgs-probe",
                            )
                            .is_some()
                            {
                                Self::update_timing_from_segment(&timing, segment, "pgs-probe");
                            }
                        }
                        gst::EventView::Gap(_) => {
                            // PGS visibility is encoded by display sets: non-empty PCS
                            // compositions show bitmaps and zero-object normal PCS display
                            // sets clear them. GAP events on sparse PGS pads describe packet
                            // absence/buffering, not the subtitle's presentation end, and can
                            // arrive immediately after a bitmap display set.
                            log::debug!(
                                "[pgs-probe] Ignoring GAP event; PGS display sets carry clear timing"
                            );
                        }
                        gst::EventView::FlushStart(_) | gst::EventView::FlushStop(_) => {
                            Self::send_invalidate_for_active_stream(
                                probe_pad,
                                &pad_stream_id,
                                &active,
                                &subtitle_tx,
                                "pgs-probe",
                            );
                        }
                        gst::EventView::Eos(_) => {
                            Self::send_invalidate_for_active_stream(
                                probe_pad,
                                &pad_stream_id,
                                &active,
                                &subtitle_tx,
                                "pgs-probe",
                            );
                        }
                        _ => {}
                    },
                    _ => {}
                }

                gst::PadProbeReturn::Ok
            },
        );
    }

    fn attach_text_probe(
        pad: &gst::Pad,
        active: &Arc<parking_lot::Mutex<ActiveSubtitleSelection>>,
        subtitle_tx: mpsc::Sender<SubtitleProbeEvent>,
    ) {
        let active = Arc::clone(active);
        let pad_stream_id = Mutex::new(Self::pad_stream_id(pad));
        let timing = Mutex::new(PadSubtitleTiming::new());

        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |probe_pad, info| {
                match &info.data {
                    Some(gst::PadProbeData::Buffer(buffer)) => {
                        let Some((stream_id, generation)) = Self::active_stream_snapshot(
                            probe_pad,
                            &pad_stream_id,
                            &active,
                            "text-probe",
                        ) else {
                            return gst::PadProbeReturn::Ok;
                        };
                        let Some((start, end)) = Self::buffer_window(buffer, &timing) else {
                            log::debug!("[text-probe] Buffer without timestamp; skipping");
                            return gst::PadProbeReturn::Ok;
                        };
                        let Ok(map) = buffer.map_readable() else {
                            return gst::PadProbeReturn::Ok;
                        };
                        let text = String::from_utf8_lossy(map.as_slice()).into_owned();

                        let event = if text.trim().is_empty() {
                            crate::subtitle_scheduler::DecodedSubtitleEvent::clear(
                                stream_id, generation, start,
                            )
                        } else {
                            let preview: String = text.chars().take(80).collect();
                            log::debug!("[text-probe] queued subtitle: {preview}...");
                            crate::subtitle_scheduler::DecodedSubtitleEvent::show(
                                stream_id,
                                generation,
                                start,
                                end,
                                WaylandSubtitlePayload::Text(text),
                            )
                        };
                        let _ = subtitle_tx.send(SubtitleProbeEvent::Decoded(event));
                    }
                    Some(gst::PadProbeData::Event(event)) => match event.view() {
                        gst::EventView::StreamStart(stream_start) => {
                            Self::update_pad_stream_id_from_event(
                                &pad_stream_id,
                                stream_start,
                                "text-probe",
                            );
                        }
                        gst::EventView::Segment(segment) => {
                            if Self::active_stream_snapshot(
                                probe_pad,
                                &pad_stream_id,
                                &active,
                                "text-probe",
                            )
                            .is_some()
                            {
                                Self::update_timing_from_segment(&timing, segment, "text-probe");
                            }
                        }
                        gst::EventView::Gap(gap) => {
                            if let Some((stream_id, generation)) = Self::active_stream_snapshot(
                                probe_pad,
                                &pad_stream_id,
                                &active,
                                "text-probe",
                            ) {
                                let at = Self::gap_running_time(gap, &timing);
                                let event = crate::subtitle_scheduler::DecodedSubtitleEvent::clear(
                                    stream_id, generation, at,
                                );
                                let _ = subtitle_tx.send(SubtitleProbeEvent::Decoded(event));
                            }
                        }
                        gst::EventView::FlushStart(_) | gst::EventView::FlushStop(_) => {
                            Self::send_invalidate_for_active_stream(
                                probe_pad,
                                &pad_stream_id,
                                &active,
                                &subtitle_tx,
                                "text-probe",
                            );
                        }
                        gst::EventView::Eos(_) => {
                            Self::send_invalidate_for_active_stream(
                                probe_pad,
                                &pad_stream_id,
                                &active,
                                &subtitle_tx,
                                "text-probe",
                            );
                        }
                        _ => {}
                    },
                    _ => {}
                }

                gst::PadProbeReturn::Ok
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
        // Clear the sync handler before state teardown so waylandsink cannot
        // call back into stale Wayland handles while the pipeline is stopping.
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
                // Do not call `handle_events(true)` here. iced/winit owns the
                // Wayland input/event loop; handing events back to waylandsink
                // can make the embedded player consume or block UI dispatch.
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

        // Clear the sync handler first to prevent callbacks during teardown.
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }

        // First, stop the pipeline
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            log::error!("Error: Failed to set state to Null during cleanup: {:?}", e);
        }

        // Wait for state change to complete
        let _ = self.pipeline.state(gst::ClockTime::from_seconds(1));

        log::debug!("Cleanup completed");
    }
}

#[cfg(test)]
mod tests {
    use super::{pgs_display_set_event, WaylandSubtitlePayload};
    use crate::{
        pgs_decoder::{PgsDisplaySet, PgsFrame},
        subtitle_scheduler::{DecodedSubtitleEvent, SubtitleAction, SubtitleScheduler},
    };
    use std::time::Duration;

    const STREAM: &str = "pgs/en";

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn non_empty_pgs_display_sets_have_no_scheduled_end() {
        let frames = vec![PgsFrame {
            argb: vec![0, 0, 0, 255],
            width: 1,
            height: 1,
            x: 10,
            y: 20,
        }];

        assert_eq!(
            pgs_display_set_event(
                STREAM.to_string(),
                3,
                ms(1_000),
                PgsDisplaySet::Show(frames.clone()),
                1920,
                1080
            ),
            DecodedSubtitleEvent::show(
                STREAM,
                3,
                ms(1_000),
                None,
                WaylandSubtitlePayload::Pgs {
                    frames,
                    video_width: 1920,
                    video_height: 1080,
                },
            )
        );
    }

    #[test]
    fn empty_pgs_display_sets_clear_at_their_timestamp() {
        assert_eq!(
            pgs_display_set_event(
                STREAM.to_string(),
                3,
                ms(2_500),
                PgsDisplaySet::Clear,
                1920,
                1080
            ),
            DecodedSubtitleEvent::clear(STREAM, 3, ms(2_500))
        );
    }

    #[test]
    fn open_ended_pgs_cue_persists_until_empty_display_set_clear() {
        let mut scheduler = SubtitleScheduler::new(STREAM, 3);
        let payload = WaylandSubtitlePayload::Pgs {
            frames: vec![PgsFrame {
                argb: vec![0, 0, 0, 255],
                width: 1,
                height: 1,
                x: 0,
                y: 0,
            }],
            video_width: 1920,
            video_height: 1080,
        };

        assert!(scheduler.push_event(DecodedSubtitleEvent::show(
            STREAM,
            3,
            ms(1_000),
            None,
            payload.clone(),
        )));
        assert_eq!(
            scheduler.drain_due(ms(1_000)),
            vec![SubtitleAction::Attach(
                crate::subtitle_scheduler::SubtitleAttach {
                    stream_id: STREAM.to_string(),
                    generation: 3,
                    start: ms(1_000),
                    end: None,
                    payload,
                }
            )]
        );
        assert!(scheduler.drain_due(ms(2_499)).is_empty());

        assert!(scheduler.push_event(DecodedSubtitleEvent::clear(STREAM, 3, ms(2_500))));
        assert_eq!(
            scheduler.drain_due(ms(2_500)),
            vec![SubtitleAction::Clear(
                crate::subtitle_scheduler::SubtitleClearAction {
                    stream_id: STREAM.to_string(),
                    generation: 3,
                },
            )]
        );
    }
}
