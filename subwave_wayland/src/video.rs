use crate::internal::Internal;
use crate::{
    pipeline::SubsurfacePipeline, subsurface_manager::WaylandSubsurfaceManager, Error,
    WaylandIntegration,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use parking_lot::RwLock;
use std::result::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use subwave_core::types::PendingState;
use subwave_core::video::types::{AudioTrack, Position, SubtitleTrack};
use subwave_core::video_trait::Video;

// Video is an exterior-facing newtype with a single interior RwLock
pub struct SubsurfaceVideo(pub(crate) RwLock<Internal>);

// Bus commands are closures applied on Internal on the UI thread
pub type Cmd = Box<dyn FnOnce(&mut Internal) + Send + 'static>;

// Implement the core Video trait for Wayland-backed SubsurfaceVideo
impl Video for SubsurfaceVideo {
    type Video = SubsurfaceVideo;

    fn new(uri: &url::Url) -> Result<Self::Video, subwave_core::Error> {
        // Creating the video object itself can't fail here
        Ok(SubsurfaceVideo(RwLock::new(Internal {
            uri: uri.clone(),
            pipeline: None,
            subsurface: None,
            duration: None,
            speed: 1.0,
            looping: false,
            is_eos: false,
            restart_stream: false,
            bus_thread: None,
            bus_stop: Arc::new(AtomicBool::new(false)),
            cmd_rx: None,
            stream_collection: None,
            available_subtitles: Vec::new(),
            current_subtitle_track: None,
            subtitles_enabled: false,
            available_audio_tracks: Vec::new(),
            current_audio_track: -1,
            audio_index_to_stream_id: Vec::new(),
            subtitle_index_to_stream_id: Vec::new(),
            selected_stream_ids: Vec::new(),
            is_buffering: false,
            buffering_percent: 100,
            user_paused: false,
            pending_state: None,
            pending_http_headers: None,
            last_position_update: Instant::now(),
        })))
    }

    fn size(&self) -> (i32, i32) {
        self.resolution().unwrap_or((0, 0))
    }

    fn framerate(&self) -> f64 {
        // Query from current caps if available
        if let Some(p) = self.0.read().pipeline.as_ref() {
            if let Some(pad) = p
                .pipeline
                .by_name("vsink")
                .and_then(|s| s.static_pad("sink"))
            {
                if let Some(caps) = pad.current_caps() {
                    if let Some(s) = caps.structure(0) {
                        if let Ok(fr) = s.get::<gst::Fraction>("framerate") {
                            return fr.numer() as f64 / fr.denom() as f64;
                        }
                    }
                }
            }
        }
        0.0
    }

    fn volume(&self) -> f64 {
        self.0
            .read()
            .pipeline
            .as_ref()
            .map(|p| p.pipeline.property::<f64>("volume"))
            .unwrap_or(0.0)
    }

    fn set_volume(&mut self, volume: f64) {
        if let Some(p) = self.0.read().pipeline.as_ref() {
            p.pipeline.set_property("volume", volume);
        }
        // Preserve mute state
        self.set_muted(self.muted());
    }

    fn muted(&self) -> bool {
        self.0
            .read()
            .pipeline
            .as_ref()
            .map(|p| p.pipeline.property::<bool>("mute"))
            .unwrap_or(false)
    }

    fn set_muted(&mut self, muted: bool) {
        if let Some(p) = self.0.read().pipeline.as_ref() {
            p.pipeline.set_property("mute", muted);
        }
    }

    fn eos(&self) -> bool {
        self.0.read().is_eos
    }

    fn looping(&self) -> bool {
        self.0.read().looping
    }

    fn set_looping(&mut self, looping: bool) {
        self.0.write().looping = looping;
    }

    fn restart_stream(&mut self) -> std::result::Result<(), subwave_core::Error> {
        // Attempt immediate restart if pipeline exists
        let p = self.0.read().pipeline.clone();
        if let Some(p) = p {
            p.seek(Position::Time(Duration::ZERO), true)
                .map_err(|_| subwave_core::Error::InvalidState)?;
            p.play().map_err(|_| subwave_core::Error::InvalidState)?;
            let mut w = self.0.write();
            w.is_eos = false;
            w.restart_stream = false;
            Ok(())
        } else {
            // Otherwise, schedule restart on next tick
            self.0.write().restart_stream = true;
            Ok(())
        }
    }

    fn paused(&self) -> bool {
        self.0
            .read()
            .pipeline
            .as_ref()
            .map(|p| p.pipeline.current_state() == gst::State::Paused)
            .unwrap_or(true)
    }

    fn set_paused(&mut self, paused: bool) {
        let pipeline = {
            let mut state = self.0.write();
            state.user_paused = paused;
            state.pipeline.clone()
        };

        if let Some(p) = pipeline {
            let _ = if paused { p.pause() } else { p.play() };
        }
    }

    fn speed(&self) -> f64 {
        self.0.read().speed
    }

    fn set_speed(&mut self, speed: f64) -> Result<(), subwave_core::Error> {
        // Update and apply via seek-rate
        {
            let mut w = self.0.write();
            w.speed = speed;
        }
        if let Some(p) = self.0.read().pipeline.clone() {
            p.set_playback_rate(speed)
                .map_err(|_| subwave_core::Error::InvalidState)
        } else {
            Ok(())
        }
    }

    fn position(&self) -> Duration {
        self.0
            .read()
            .pipeline
            .as_ref()
            .and_then(|p| p.pipeline.query_position::<gst::ClockTime>())
            .map(|ct| Duration::from_nanos(ct.nseconds()))
            .unwrap_or(Duration::ZERO)
    }

    fn seek(
        &mut self,
        position: impl Into<Position>,
        accurate: bool,
    ) -> Result<(), subwave_core::Error> {
        if let Some(p) = self.0.read().pipeline.clone() {
            p.seek(position, accurate)
                .map_err(|_| subwave_core::Error::InvalidState)
        } else {
            Err(subwave_core::Error::InvalidState)
        }
    }

    fn duration(&self) -> Duration {
        if let Some(d) = self.0.read().duration {
            d
        } else {
            self.0
                .read()
                .pipeline
                .as_ref()
                .and_then(|p| p.pipeline.query_duration::<gst::ClockTime>())
                .map(|ct| Duration::from_nanos(ct.nseconds()))
                .unwrap_or(Duration::ZERO)
        }
    }

    fn subtitle_url(&self) -> Option<url::Url> {
        self.0
            .read()
            .pipeline
            .as_ref()
            .and_then(|p| p.pipeline.property::<Option<String>>("suburi"))
            .and_then(|s| url::Url::parse(&s).ok())
    }

    fn set_subtitle_url(&mut self, url: &url::Url) -> Result<(), subwave_core::Error> {
        if let Some(p) = self.0.read().pipeline.as_ref() {
            // Safest to set while PAUSED/READY, similar to appsink impl
            let _ = p.pipeline.set_state(gst::State::Ready);
            p.pipeline.set_property("suburi", url.as_str());
            let _ = p.pipeline.set_state(gst::State::Playing);
            Ok(())
        } else {
            Ok(())
        }
    }

    fn subtitles_enabled(&self) -> bool {
        self.0.read().subtitles_enabled
    }

    fn set_subtitles_enabled(&mut self, enabled: bool) {
        // Best-effort: select default or disable
        if enabled {
            let idx = {
                let r = self.0.read();
                if r.current_subtitle_track.is_some() {
                    r.current_subtitle_track
                } else if !r.subtitle_index_to_stream_id.is_empty() {
                    Some(0)
                } else {
                    None
                }
            };
            if let Some(i) = idx {
                let _ = SubsurfaceVideo::select_subtitle_track(self, Some(i));
            }
        } else {
            let _ = SubsurfaceVideo::select_subtitle_track(self, None);
        }
    }

    fn subtitle_tracks(&mut self) -> Vec<SubtitleTrack> {
        self.0.read().available_subtitles.clone()
    }

    fn current_subtitle_track(&self) -> Option<i32> {
        self.0.read().current_subtitle_track
    }

    fn select_subtitle_track(
        &mut self,
        track_index: Option<i32>,
    ) -> Result<(), subwave_core::Error> {
        SubsurfaceVideo::select_subtitle_track(self, track_index)
            .map_err(|_| subwave_core::Error::InvalidState)
    }

    fn audio_tracks(&mut self) -> Vec<AudioTrack> {
        self.0.read().available_audio_tracks.clone()
    }

    fn current_audio_track(&self) -> i32 {
        self.current_audio_track()
    }

    fn select_audio_track(&mut self, track_index: i32) -> Result<(), subwave_core::Error> {
        SubsurfaceVideo::select_audio_track(self, track_index)
            .map_err(|_| subwave_core::Error::InvalidState)
    }

    fn has_video(&self) -> bool {
        self.resolution()
            .map(|(w, h)| w > 0 && h > 0)
            .unwrap_or(false)
    }

    fn pipeline(&self) -> gst::Pipeline {
        self.0
            .read()
            .pipeline
            .as_ref()
            .map(|p| p.pipeline.as_ref().clone())
            .unwrap_or_default()
    }
}

impl SubsurfaceVideo {
    pub fn new(uri: &url::Url) -> Result<Self, Error> {
        let inner = Internal {
            uri: uri.clone(),
            pipeline: None,
            subsurface: None,
            duration: None,
            speed: 1.0,
            looping: false,
            is_eos: false,
            restart_stream: false,
            bus_thread: None,
            bus_stop: Arc::new(AtomicBool::new(false)),
            cmd_rx: None,
            stream_collection: None,
            // Subtitle tracking
            available_subtitles: Vec::new(),
            current_subtitle_track: None,
            subtitles_enabled: false,
            // Audio track tracking
            available_audio_tracks: Vec::new(),
            current_audio_track: -1,
            // Indices
            audio_index_to_stream_id: Vec::new(),
            subtitle_index_to_stream_id: Vec::new(),
            selected_stream_ids: Vec::new(),
            is_buffering: false,
            buffering_percent: 100,
            user_paused: false,
            pending_state: None,
            pending_http_headers: None,
            last_position_update: Instant::now(),
        };
        Ok(SubsurfaceVideo(RwLock::new(inner)))
    }

    /// Set HTTP headers for HTTP-based sources via GStreamer "http-headers" context.
    /// If the pipeline is not yet initialized, headers are stored and applied during init.
    pub fn set_http_headers(&mut self, headers: &[(impl AsRef<str>, impl AsRef<str>)]) {
        // Stash a copy for later application
        {
            let mut w = self.0.write();
            w.pending_http_headers = Some(
                headers
                    .iter()
                    .map(|(k, v)| (k.as_ref().to_string(), v.as_ref().to_string()))
                    .collect(),
            );
        }

        // Apply immediately if we already have a pipeline
        if let Some(p) = self.0.read().pipeline.clone() {
            if let Some(h) = self.0.read().pending_http_headers.as_ref() {
                subwave_core::http::set_http_headers_on_pipeline(&p.pipeline, h);
            }
        }
    }

    // Initialize Wayland and the playback pipeline. Spawns a bus thread that translates
    // GStreamer messages into small commands (closures) that are applied on the UI thread.
    pub fn init_wayland(
        &self,
        integration: WaylandIntegration,
        bounds: (i32, i32, i32, i32),
    ) -> Result<(), Error> {
        // Construct subsurface and pipeline (no lock held during external calls)
        let subsurface = WaylandSubsurfaceManager::new(integration.clone())?;
        let pipeline = Arc::new(SubsurfacePipeline::new(
            &self.0.read().uri,
            &subsurface,
            &integration,
            bounds,
        )?);

        // Apply any pending HTTP headers context before starting message processing
        if let Some(h) = self.0.read().pending_http_headers.clone() {
            subwave_core::http::set_http_headers_on_pipeline(&pipeline.pipeline, h.as_slice());
        }

        // Create command channel for bus -> UI updates
        let (tx, rx) = mpsc::channel::<Cmd>();

        // Spawn bus thread translating messages into closures
        let stop = self.0.read().bus_stop.clone();
        if let Some(bus) = pipeline.bus() {
            let gst_pipeline = pipeline.pipeline.clone();
            let handle = std::thread::Builder::new()
                .name(format!("gst-bus-{}", self.0.read().uri))
                .spawn(move || {
                    use gst::MessageView;
                    // Track desired selection and readiness
                    let mut desired_select_ids: Option<Vec<String>> = None;
                    let mut did_send_select = false;
                    let mut pipeline_ready = false;

                    // Helper to send SelectStreams preferring pipeline
                    fn send_select_streams_preferring_pipeline(
                        pipe: &gst::Pipeline,
                        ids: &[String],
                    ) -> bool {
                        let evt = gst::event::SelectStreams::new(ids.iter().map(|s| s.as_str()));
                        if pipe.send_event(evt) {
                            return true;
                        }
                        false
                    }

                    while !stop.load(Ordering::SeqCst) {
                        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) {
                            match msg.view() {
                                MessageView::Eos(_) => {
                                    // Mark EOS and schedule restart on UI thread if looping
                                    let _ = tx.send(Box::new(|s: &mut Internal| {
                                        s.is_eos = true;
                                        if s.looping {
                                            s.restart_stream = true;
                                        }
                                    }));
                                }
                                MessageView::Error(err) => {
                                    log::error!("Pipeline error: {:?}", err);
                                    // Keep the bus thread alive to allow recovery strategies if needed
                                }
                                MessageView::DurationChanged(_) => {
                                    let dur = gst_pipeline
                                        .query_duration::<gst::ClockTime>()
                                        .map(|d| Duration::from_nanos(d.nseconds()));
                                    if tx.send(Box::new(move |s: &mut Internal| s.duration = dur)).is_err() {
                                        log::debug!("[bus] receiver dropped; exiting bus thread");
                                        break;
                                    }
                                }
                                MessageView::Buffering(buffering) => {
                                    let percent = buffering.percent();
                                    log::debug!("[buffering] {}%", percent);
                                    let tx_buffer = tx.clone();
                                    if tx_buffer
                                        .send(Box::new(move |state: &mut Internal| {
                                            let was_buffering = state.is_buffering;
                                            let buffering_now = percent < 100;
                                            state.is_buffering = buffering_now;
                                            state.buffering_percent = percent;

                                            if let Some(pipeline) = state.pipeline.clone() {
                                                if buffering_now && !was_buffering && !state.user_paused {
                                                    if let Err(err) = pipeline.pause() {
                                                        log::warn!(
                                                            "Failed to pause pipeline during buffering: {err:?}"
                                                        );
                                                    }
                                                } else if !buffering_now
                                                    && was_buffering
                                                    && !state.user_paused
                                                {
                                                    if let Err(err) = pipeline.play() {
                                                        log::warn!(
                                                            "Failed to resume pipeline after buffering: {err:?}"
                                                        );
                                                    }
                                                }
                                            }
                                        }))
                                        .is_err()
                                    {
                                        log::debug!("[bus] receiver dropped; exiting bus thread");
                                        break;
                                    }
                                }
                                MessageView::StreamCollection(msg) => {
                                    let collection = msg.stream_collection();
                                    let n = collection.len();
                                    log::info!("[streams] StreamCollection received: {} streams", n);

                                    // Track lists and id mappings
                                    let mut audio_tracks: Vec<AudioTrack> = Vec::new();
                                    let mut subtitle_tracks: Vec<SubtitleTrack> = Vec::new();
                                    let mut audio_ids: Vec<String> = Vec::new();
                                    let mut subtitle_ids: Vec<String> = Vec::new();
                                    let mut first_video_id: Option<String> = None;
                                    let mut best_text_id: Option<String> = None; // text/x-raw preferred
                                    let mut any_text_id: Option<String> = None;

                                    for i in 0..n {
                                        if let Some(stream) = collection.stream(i as u32) {
                                            let stype = stream.stream_type();
                                            let sid = stream
                                                .stream_id()
                                                .unwrap_or_else(|| "<no-id>".into());
                                            let caps = stream.caps();

                                            if stype.contains(gst::StreamType::VIDEO) {
                                                if first_video_id.is_none() {
                                                    first_video_id = Some(sid.to_string());
                                                }
                                            } else if stype.contains(gst::StreamType::AUDIO) {
                                                // Extract audio info
                                                let mut language: Option<String> = None;
                                                let mut title: Option<String> = None;
                                                let mut codec: Option<String> = None;
                                                let mut channels: Option<i32> = None;
                                                let mut sample_rate: Option<i32> = None;

                                                if let Some(tags) = stream.tags() {
                                                    if let Some(v) = tags.get::<gst::tags::LanguageCode>() {
                                                        language = Some(v.get().to_string());
                                                    } else if let Some(v) = tags.get::<gst::tags::LanguageName>() {
                                                        language = Some(v.get().to_string());
                                                    }
                                                    if let Some(v) = tags.get::<gst::tags::Title>() {
                                                        title = Some(v.get().to_string());
                                                    }
                                                    if let Some(v) = tags.get::<gst::tags::Codec>() {
                                                        codec = Some(v.get().to_string());
                                                    }
                                                }
                                                if let Some(c) = caps.as_ref().and_then(|c| c.structure(0)) {
                                                    if let Ok(ch) = c.get::<i32>("channels") { channels = Some(ch); }
                                                    if let Ok(sr) = c.get::<i32>("rate") { sample_rate = Some(sr); }
                                                    if codec.is_none() { codec = Some(c.name().to_string()); }
                                                }

                                                let idx = audio_tracks.len() as i32;
                                                audio_tracks.push(AudioTrack { index: idx, language, title, codec, channels, sample_rate });
                                                audio_ids.push(sid.to_string());
                                            } else if stype.contains(gst::StreamType::TEXT) {
                                                // Extract subtitle info
                                                let mut language: Option<String> = None;
                                                let mut title: Option<String> = None;
                                                let mut codec: Option<String> = None;
                                                if let Some(tags) = stream.tags() {
                                                    if let Some(v) = tags.get::<gst::tags::LanguageCode>() {
                                                        language = Some(v.get().to_string());
                                                    } else if let Some(v) = tags.get::<gst::tags::LanguageName>() {
                                                        language = Some(v.get().to_string());
                                                    }
                                                    if let Some(v) = tags.get::<gst::tags::Title>() {
                                                        title = Some(v.get().to_string());
                                                    }
                                                    if let Some(v) = tags.get::<gst::tags::Codec>() {
                                                        codec = Some(v.get().to_string());
                                                    }
                                                }
                                                if let Some(c) = caps.as_ref().and_then(|c| c.structure(0)) {
                                                    if codec.is_none() { codec = Some(c.name().to_string()); }
                                                    // Remember best raw text
                                                    if c.name().starts_with("text/x-raw") && best_text_id.is_none() {
                                                        best_text_id = Some(sid.to_string());
                                                    }
                                                }
                                                if any_text_id.is_none() { any_text_id = Some(sid.to_string()); }

                                                let idx = subtitle_tracks.len() as i32;
                                                subtitle_tracks.push(SubtitleTrack { index: idx, language, title, codec });
                                                subtitle_ids.push(sid.to_string());
                                            }
                                        }
                                    }

                                    // Compute initial selection
                                    let mut initial_ids: Vec<String> = Vec::new();
                                    if let Some(v) = first_video_id.clone() { initial_ids.push(v); }
                                    if let Some(aid) = audio_ids.first() { initial_ids.push(aid.clone()); }
                                    let chosen_text = best_text_id.or(any_text_id);
                                    if let Some(tid) = chosen_text.clone() { initial_ids.push(tid); }

                                    let subtitles_enabled = chosen_text.is_some();
                                    let current_audio_index = if audio_ids.is_empty() { -1 } else { 0 };
                                    let current_sub_index = if subtitles_enabled { Some(0) } else { None };

                                    // Update internal state immediately to expose available tracks
                                    let coll_clone = collection.clone();
                                    let tx_tracks = tx.clone();
                                    let ids_for_state = initial_ids.clone();
                                    if tx_tracks
                                        .send(Box::new(move |s: &mut Internal| {
                                            s.stream_collection = Some(coll_clone);
                                            s.available_audio_tracks = audio_tracks;
                                            s.available_subtitles = subtitle_tracks;
                                            s.audio_index_to_stream_id = audio_ids;
                                            s.subtitle_index_to_stream_id = subtitle_ids;
                                            s.selected_stream_ids = ids_for_state;
                                            s.current_audio_track = current_audio_index;
                                            s.current_subtitle_track = current_sub_index;
                                            s.subtitles_enabled = subtitles_enabled;
                                        }))
                                        .is_err()
                                    {
                                        log::debug!("[bus] receiver dropped; exiting bus thread");
                                        break;
                                    }

                                    // Stage selection; send when ready
                                    desired_select_ids = Some(initial_ids);
                                    if pipeline_ready && !did_send_select {
                                        if let Some(ref ids) = desired_select_ids {
                                            if send_select_streams_preferring_pipeline(&gst_pipeline, ids) {
                                                log::info!("[streams] Sent SelectStreams after collection");
                                                did_send_select = true;
                                            } else {
                                                log::warn!("[streams] Failed to send SelectStreams on collection; will retry later");
                                            }
                                        }
                                    }
                                }
                                MessageView::StreamsSelected(sel) => {
                                    let collection = sel.stream_collection();
                                    let mut _n_audio = 0;
                                    let mut _n_subtitle = 0;
                                for i in 0..collection.len() {
                                    if let Some(stream) = collection.stream(i as u32) {
                                        let st = stream.stream_type();
                                        if st.contains(gst::StreamType::AUDIO) { _n_audio += 1; }
                                        if st.contains(gst::StreamType::TEXT) { _n_subtitle += 1; }
                                    }
                                }
                                }
                                MessageView::StateChanged(state_changed) => {
                                    if let Some(src) = msg.src() {
                                        if src.name() == gst_pipeline.name() {
                                            let cur = state_changed.current();
                                            if cur == gst::State::Paused || cur == gst::State::Playing {
                                                pipeline_ready = true;
                                                if !did_send_select {
                                                    if let Some(ref ids) = desired_select_ids {
                                                        if send_select_streams_preferring_pipeline(&gst_pipeline, ids) {
                                                            log::info!("[streams] Sent SelectStreams on state ready");
                                                            did_send_select = true;
                                                        } else {
                                                            log::warn!("[streams] SelectStreams send failed on state change; will retry");
                                                        }
                                                    }
                                                }
                                            }
                                            /*
                                            // Update seekable
                                            let seekable = {
                                                let mut q = gst::query::Seeking::new(gst::Format::Time);
                                                if gst_pipeline.query(q.query_mut()) {
                                                    let (seekable, _, _) = q.result();
                                                    Some(seekable)
                                                } else { None }
                                            };
                                            if let Some(seek) = seekable {
                                                if tx.send(Box::new(move |s: &mut Internal| s.video_props.seekable = seek)).is_err() {
                                                    log::debug!("[bus] receiver dropped; exiting bus thread");
                                                    break;
                                                }
                                            }*/
                                        }
                                    }
                                }
                                MessageView::AsyncDone(_) => {
                                    if pipeline_ready && !did_send_select {
                                        if let Some(ref ids) = desired_select_ids {
                                            if send_select_streams_preferring_pipeline(&gst_pipeline, ids) {
                                                log::info!("[streams] Sent SelectStreams after AsyncDone");
                                                did_send_select = true;
                                            }
                                        }
                                    }
                                    //// Refresh seekable on AsyncDone as well
                                    //let seekable = {
                                    //    let mut q = gst::query::Seeking::new//(gst::Format::Time);
                                    //    if gst_pipeline.query(q.query_mut()) {
                                    //        let (seekable, _, _) = q.result();
                                    //        Some(seekable)
                                    //    } else { None }
                                    //};
                                    //if let Some(seek) = seekable {
                                    //    if tx.send(Box::new(move |s: &mut Internal//| s.video_props.seekable = seek)).is_err() //{
                                    //        log::debug!("[bus] receiver dropped; //exiting bus thread");
                                    //        break;
                                    //    }
                                    //}
                                }
                                _ => {}
                            }
                        }
                    }
                })
                .expect("Failed to spawn bus thread");

            let mut w = self.0.write();
            w.bus_thread = Some(handle);
        }

        // Commit subsurface, pipeline, and receiver into Internal
        {
            let mut w = self.0.write();
            w.subsurface = Some(subsurface);
            w.pipeline = Some(pipeline);
            w.cmd_rx = Some(rx);
        }

        Ok(())
    }

    // Drain pending bus commands and pump subtitles. Intended to be called on UI/redraw ticks.
    pub fn tick(&mut self) {
        // 1) Apply pending commands with a short write lock
        let pending = {
            let mut w = self.0.write();
            loop {
                let cmd_opt = {
                    if let Some(rx) = &w.cmd_rx {
                        rx.try_recv().ok()
                    } else {
                        None
                    }
                };
                match cmd_opt {
                    Some(cmd) => cmd(&mut w),
                    None => break,
                }
            }
            // Handle scheduled restart on UI thread
            if w.restart_stream {
                if let Some(p) = w.pipeline.clone() {
                    if p.seek(Position::Time(Duration::ZERO), true).is_ok() {
                        let _ = p.play();
                        w.is_eos = false;
                        w.restart_stream = false;
                    }
                }
            }
            // Take any pending state to apply outside the lock
            w.pending_state.take()
        };

        // 2) Apply pending state when pipeline is ready
        if let Some(st) = pending {
            let has_pipeline = self.0.read().pipeline.is_some();
            if has_pipeline {
                // Best-effort apply; if not ready, requeue
                let requeue = self.apply_state_now(&st).is_err();
                if requeue {
                    let mut w = self.0.write();
                    w.pending_state = Some(st);
                }
            } else {
                let mut w = self.0.write();
                w.pending_state = Some(st);
            }
        }

        // 3) (Optional) subtitle draining could happen here
    }

    // Control
    pub fn play(&self) -> Result<(), Error> {
        let p = self.0.read().pipeline.clone();
        if let Some(p) = p {
            p.play()?;
            Ok(())
        } else {
            Err(subwave_core::Error::Pipeline(
                "Video not initialized".into(),
            ))
        }
    }

    pub fn pause(&self) -> Result<(), Error> {
        let p = self.0.read().pipeline.clone();
        if let Some(p) = p {
            p.pause()?;
            Ok(())
        } else {
            Err(Error::Pipeline("Video not initialized".into()))
        }
    }

    pub fn stop(&self) -> Result<(), Error> {
        // Signal thread and join
        let handle = {
            let mut w = self.0.write();
            w.bus_stop.store(true, Ordering::SeqCst);
            w.bus_thread.take()
        };
        if let Some(h) = handle {
            let _ = h.join();
        }
        let subsurface = self.0.read().subsurface.clone();
        if let Some(s) = subsurface {
            let _ = s.clear_subtitle();
        }

        // Stop pipeline
        if let Some(p) = self.0.read().pipeline.clone() {
            p.stop()?;
        }
        Ok(())
    }

    pub fn toggle_play(&self) -> Result<(), Error> {
        if self.is_playing() {
            self.pause()
        } else {
            self.play()
        }
    }

    // Queries
    fn apply_state_now(&mut self, st: &PendingState) -> Result<(), ()> {
        // Pause first, ignore errors
        let _ = self.pause();
        let _ = self.select_audio_track(st.audio_track);
        let _ = self.select_subtitle_track(st.subtitle_track);
        self.set_subtitles_enabled(st.subtitles_enabled);
        if let Some(url) = &st.subtitle_url {
            let _ = self.set_subtitle_url(url);
        }
        if self.seek(st.position, true).is_err() {
            return Err(());
        }
        self.set_volume(st.volume);
        self.set_muted(st.muted);
        let _ = self.set_playback_rate(st.speed);
        if st.paused {
            let _ = self.pause();
        } else {
            let _ = self.play();
        }
        Ok(())
    }

    pub fn queue_pending_state(&self, st: PendingState) {
        let mut w = self.0.write();
        w.pending_state = Some(st);
    }

    pub fn is_playing(&self) -> bool {
        self.0
            .read()
            .pipeline
            .as_ref()
            .map(|p| p.pipeline.current_state() == gst::State::Playing)
            .unwrap_or(false)
    }
    pub fn is_paused(&self) -> bool {
        self.0
            .read()
            .pipeline
            .as_ref()
            .map(|p| p.pipeline.current_state() == gst::State::Paused)
            .unwrap_or(false)
    }

    pub fn seek(&self, position: impl Into<Position>, accurate: bool) -> Result<(), Error> {
        if let Some(p) = self.0.read().pipeline.clone() {
            p.seek(position, accurate)
        } else {
            Err(Error::Pipeline("Video not initialized".into()))
        }
    }

    // Wayland surface positioning and viewport
    pub fn set_subsurface_position(&self, x: i32, y: i32) {
        if let Some(s) = self.0.read().subsurface.clone() {
            s.set_position(x, y);
        }
    }

    pub fn set_buffer_offset(&self, x: i32, y: i32) {
        if let Some(s) = self.0.read().subsurface.clone() {
            s.set_buffer_offset(x, y);
        }
    }

    pub fn set_subsurface_viewport(
        &self,
        source: Option<(i32, i32, i32, i32)>,
        dest: Option<(i32, i32)>,
    ) {
        if let Some(s) = self.0.read().subsurface.clone() {
            s.set_video_viewport(source, dest);
        }
    }

    pub fn set_video_size_position(&self, x_offset: i32, y_offset: i32, width: i32, height: i32) {
        let (pipeline, subsurface) = {
            let guard = self.0.read();
            (guard.pipeline.clone(), guard.subsurface.clone())
        };

        if let Some(p) = pipeline {
            p.set_render_rectangle(x_offset, y_offset, width, height);
        }

        if let Some(s) = subsurface {
            s.set_size(width, height);
        }
    }

    // Resolution helpers: query directly from vsink caps for current stream
    pub fn resolution(&self) -> Option<(i32, i32)> {
        let p = self.0.read().pipeline.clone()?;
        let video_pad = p
            .pipeline
            .by_name("vsink")
            .and_then(|sink| sink.static_pad("sink"))?;
        let caps = video_pad.current_caps()?;
        let s = caps.structure(0)?;
        let w = s.get::<i32>("width").ok()?;
        let h = s.get::<i32>("height").ok()?;
        Some((w, h))
    }

    pub fn width(&self) -> Option<i32> {
        self.resolution().map(|(w, _)| w)
    }
    pub fn height(&self) -> Option<i32> {
        self.resolution().map(|(_, h)| h)
    }

    // Audio/volume/rate
    pub fn set_volume(&self, volume: f64) -> Result<(), Error> {
        if let Some(p) = self.0.read().pipeline.clone() {
            p.set_volume(volume)
        } else {
            Ok(())
        }
    }

    pub fn set_playback_rate(&self, rate: f64) -> Result<(), Error> {
        if let Some(p) = self.0.read().pipeline.clone() {
            p.set_playback_rate(rate)
        } else {
            Ok(())
        }
    }

    pub fn current_audio_track(&self) -> i32 {
        let w = self.0.read();
        if w.current_audio_track >= 0 {
            w.current_audio_track
        } else {
            -1
        }
    }

    pub fn current_subtitle_track(&self) -> Option<i32> {
        self.0.read().current_subtitle_track
    }

    pub fn audio_tracks_info(&self) -> Vec<AudioTrack> {
        self.0.read().available_audio_tracks.clone()
    }

    pub fn subtitle_tracks_info(&self) -> Vec<SubtitleTrack> {
        self.0.read().available_subtitles.clone()
    }

    pub fn select_audio_track(&self, index: i32) -> Result<(), Error> {
        // Gather required info without holding the lock during GStreamer calls
        let (p, mut new_ids, audio_ids) = {
            let r = self.0.read();
            let p = r.pipeline.clone();
            if index < 0 || (index as usize) >= r.audio_index_to_stream_id.len() {
                return Err(Error::Pipeline(format!(
                    "Invalid audio track index: {}",
                    index
                )));
            }
            let mut ids = r.selected_stream_ids.clone();
            // Remove any existing audio IDs
            if !r.audio_index_to_stream_id.is_empty() {
                ids.retain(|id| !r.audio_index_to_stream_id.iter().any(|aid| aid == id));
            }
            (p, ids, r.audio_index_to_stream_id.clone())
        };

        let Some(p) = p else {
            return Err(Error::Pipeline("Video not initialized".into()));
        };
        let target_id = audio_ids[index as usize].clone();
        // Append new audio id
        new_ids.push(target_id);
        // Dedup while preserving order
        dedup_in_place(&mut new_ids);

        let ok = p.send_select_streams(&new_ids);
        if ok {
            let mut w = self.0.write();
            w.selected_stream_ids = new_ids;
            w.current_audio_track = index;
            Ok(())
        } else {
            Err(Error::Pipeline(
                "Failed to send SelectStreams for audio".into(),
            ))
        }
    }

    pub fn select_subtitle_track(&self, index: Option<i32>) -> Result<(), Error> {
        let (p, mut new_ids, sub_ids) = {
            let r = self.0.read();
            let p = r.pipeline.clone();
            let mut ids = r.selected_stream_ids.clone();
            // Remove existing subtitle ids
            if !r.subtitle_index_to_stream_id.is_empty() {
                ids.retain(|id| !r.subtitle_index_to_stream_id.iter().any(|sid| sid == id));
            }
            (p, ids, r.subtitle_index_to_stream_id.clone())
        };

        let Some(p) = p else {
            return Err(Error::Pipeline("Video not initialized".into()));
        };

        let mut new_current: Option<i32> = None;
        let mut enabled = false;
        if let Some(i) = index {
            if i < 0 || (i as usize) >= sub_ids.len() {
                return Err(Error::Pipeline(format!(
                    "Invalid subtitle track index: {}",
                    i
                )));
            }
            let sid = sub_ids[i as usize].clone();
            new_ids.push(sid);
            new_current = Some(i);
            enabled = true;
        }
        dedup_in_place(&mut new_ids);

        let ok = p.send_select_streams(&new_ids);
        if ok {
            let mut w = self.0.write();
            w.selected_stream_ids = new_ids;
            w.current_subtitle_track = new_current;
            w.subtitles_enabled = enabled;
            Ok(())
        } else {
            Err(Error::Pipeline(
                "Failed to send SelectStreams for subtitles".into(),
            ))
        }
    }

    pub fn subtitles_enabled(&self) -> bool {
        self.0.read().subtitles_enabled
    }

    pub fn set_subtitles_enabled(&self, enabled: bool) -> Result<(), Error> {
        if enabled == self.subtitles_enabled() {
            return Ok(());
        }
        if enabled {
            // Enable: choose current or default to 0
            let default_idx = {
                let r = self.0.read();
                if r.current_subtitle_track.is_some() {
                    r.current_subtitle_track
                } else if !r.subtitle_index_to_stream_id.is_empty() {
                    Some(0)
                } else {
                    None
                }
            };
            if let Some(i) = default_idx {
                self.select_subtitle_track(Some(i))
            } else {
                Ok(())
            }
        } else {
            // Disable
            self.select_subtitle_track(None)
        }
    }

    pub fn get_subsurface(&self) -> Option<Arc<WaylandSubsurfaceManager>> {
        self.0.read().subsurface.clone()
    }

    // Widget-friendly helper for throttled frame notifications
    pub fn should_emit_on_new_frame(&self, interval: Duration) -> bool {
        let now = Instant::now();
        let mut w = self.0.write();
        if now.duration_since(w.last_position_update) >= interval {
            w.last_position_update = now;
            true
        } else {
            false
        }
    }
}

fn dedup_in_place(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::<String>::new();
    v.retain(|s| seen.insert(s.clone()));
}

impl Drop for SubsurfaceVideo {
    fn drop(&mut self) {
        // Best-effort cleanup without panicking
        let handle = {
            let mut w = self.0.write();
            w.bus_stop.store(true, Ordering::SeqCst);
            w.bus_thread.take()
        };
        if let Some(h) = handle {
            let _ = h.join();
        }
        if let Some(p) = self.0.read().pipeline.clone() {
            let _ = p.stop();
        }
    }
}
