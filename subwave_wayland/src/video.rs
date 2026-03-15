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
            startup_async_done: false,
            pending_state: None,
            pgs_decoder: crate::pgs_decoder::PgsDecoder::new(),
            pgs_stream_ids: Vec::new(),
            pgs_active: Arc::new(AtomicBool::new(false)),
            pending_http_headers: None,
            pending_play_after_seek: false,
            pending_start_position: None,
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
            startup_async_done: false,
            pending_state: None,
            pgs_decoder: crate::pgs_decoder::PgsDecoder::new(),
            pgs_stream_ids: Vec::new(),
            pgs_active: Arc::new(AtomicBool::new(false)),
            pending_http_headers: None,
            pending_play_after_seek: false,
            pending_start_position: None,
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
        let compositor_has_cm = subsurface.has_color_management();
        let pgs_active = self.0.read().pgs_active.clone();
        let pipeline = Arc::new(SubsurfacePipeline::new(
            &self.0.read().uri,
            &subsurface,
            &integration,
            bounds,
            compositor_has_cm,
            &pgs_active,
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

                    let disable_text = env_flag_enabled("SUBWAVE_DISABLE_TEXT");
                    let mut forced_text_off = false;

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
                                    let mut pgs_ids: Vec<String> = Vec::new();

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
                                                let mut is_text_sub = false;
                                                let mut is_pgs_sub = false;
                                                if let Some(c) = caps.as_ref().and_then(|c| c.structure(0)) {
                                                    if codec.is_none() { codec = Some(c.name().to_string()); }
                                                    let cap_name = c.name().as_str();
                                                    is_text_sub = cap_name.starts_with("text/");
                                                    is_pgs_sub = cap_name == "subpicture/x-pgs"
                                                        || cap_name == "subpicture/x-dvd";
                                                    if is_text_sub && best_text_id.is_none() {
                                                        best_text_id = Some(sid.to_string());
                                                    }
                                                }

                                                if is_pgs_sub {
                                                    // PGS/DVD bitmap subs — track separately.
                                                    // They appear in the UI track list but are NOT
                                                    // routed through playbin3's SelectStreams (which
                                                    // would activate subtitleoverlay and cause green
                                                    // artifacts on HDR).  Instead, when selected, the
                                                    // PGS decoder intercepts raw buffers via pad probe.
                                                    log::info!(
                                                        "[streams] PGS/bitmap subtitle stream {sid}: codec={codec:?}"
                                                    );
                                                    pgs_ids.push(sid.to_string());
                                                    let idx = subtitle_tracks.len() as i32;
                                                    subtitle_tracks.push(SubtitleTrack { index: idx, language, title, codec });
                                                    subtitle_ids.push(sid.to_string());
                                                } else if is_text_sub {
                                                    if any_text_id.is_none() { any_text_id = Some(sid.to_string()); }
                                                    let idx = subtitle_tracks.len() as i32;
                                                    subtitle_tracks.push(SubtitleTrack { index: idx, language, title, codec });
                                                    subtitle_ids.push(sid.to_string());
                                                } else {
                                                    log::info!(
                                                        "[streams] Skipping unsupported subtitle format {sid}: codec={codec:?}"
                                                    );
                                                    continue;
                                                }
                                            }
                                        }
                                    }

                                    // Track current playbin selection without forcing an immediate
                                    // startup SelectStreams event (which can cause reconfigure churn).
                                    let mut current_audio_prop = if gst_pipeline.has_property("current-audio") {
                                        gst_pipeline.property::<i32>("current-audio")
                                    } else {
                                        -1
                                    };
                                    let mut current_text_prop = if disable_text {
                                        -1
                                    } else if gst_pipeline.has_property("current-text") {
                                        gst_pipeline.property::<i32>("current-text")
                                    } else {
                                        -1
                                    };

                                    // Stabilize startup defaults without sending SelectStreams.
                                    if current_audio_prop < 0 && !audio_ids.is_empty() && gst_pipeline.has_property("current-audio") {
                                        gst_pipeline.set_property("current-audio", 0i32);
                                        current_audio_prop = 0;
                                    }
                                    if disable_text && current_text_prop >= 0 && gst_pipeline.has_property("current-text") {
                                        gst_pipeline.set_property("current-text", -1i32);
                                        current_text_prop = -1;
                                    }

                                    let mut selected_ids: Vec<String> = Vec::new();
                                    if let Some(v) = first_video_id.clone() {
                                        selected_ids.push(v);
                                    }

                                    let mut current_audio_index = -1;
                                    if current_audio_prop >= 0
                                        && (current_audio_prop as usize) < audio_ids.len()
                                    {
                                        current_audio_index = current_audio_prop;
                                        selected_ids.push(audio_ids[current_audio_prop as usize].clone());
                                    } else if let Some(aid) = audio_ids.first() {
                                        current_audio_index = 0;
                                        selected_ids.push(aid.clone());
                                    }

                                    let mut subtitles_enabled = false;
                                    let mut current_sub_index: Option<i32> = None;
                                    if current_text_prop >= 0
                                        && (current_text_prop as usize) < subtitle_ids.len()
                                    {
                                        subtitles_enabled = true;
                                        current_sub_index = Some(current_text_prop);
                                        selected_ids.push(subtitle_ids[current_text_prop as usize].clone());
                                    } else if !disable_text {
                                        // Keep legacy hint for UI when current-text is not exposed yet.
                                        let chosen_text = best_text_id.or(any_text_id);
                                        if env_flag_enabled("SUBWAVE_AUTO_ENABLE_SUBS") {
                                            if let Some(tid) = chosen_text {
                                                subtitles_enabled = true;
                                                current_sub_index = Some(0);
                                                selected_ids.push(tid);
                                            }
                                        }
                                    }

                                    dedup_in_place(&mut selected_ids);

                                    // Send SelectStreams to playbin3 so audio is
                                    // included in the active selection.  Without
                                    // this, GStreamer 1.28's internal re-selection
                                    // (triggered by a text pad appearing) can drop
                                    // the audio stream, leaving playback silent.
                                    if !selected_ids.is_empty() {
                                        let evt = gst::event::SelectStreams::new(
                                            selected_ids.iter().map(|s| s.as_str()),
                                        );
                                        if gst_pipeline.send_event(evt) {
                                            log::info!(
                                                "[streams] Sent SelectStreams with {} ids",
                                                selected_ids.len()
                                            );
                                        } else {
                                            log::warn!(
                                                "[streams] Failed to send SelectStreams event"
                                            );
                                        }
                                    }

                                    // Update internal state immediately to expose available tracks
                                    let coll_clone = collection.clone();
                                    let tx_tracks = tx.clone();
                                    if tx_tracks
                                        .send(Box::new(move |s: &mut Internal| {
                                            s.stream_collection = Some(coll_clone);
                                            s.available_audio_tracks = audio_tracks;
                                            s.available_subtitles = subtitle_tracks;
                                            s.audio_index_to_stream_id = audio_ids;
                                            s.subtitle_index_to_stream_id = subtitle_ids;
                                            s.pgs_stream_ids = pgs_ids;
                                            s.selected_stream_ids = selected_ids;
                                            s.current_audio_track = current_audio_index;
                                            s.current_subtitle_track = current_sub_index;
                                            s.subtitles_enabled = subtitles_enabled;
                                        }))
                                        .is_err()
                                    {
                                        log::debug!("[bus] receiver dropped; exiting bus thread");
                                        break;
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
                                            if disable_text
                                                && !forced_text_off
                                                && (cur == gst::State::Paused
                                                    || cur == gst::State::Playing)
                                            {
                                                if gst_pipeline.has_property("current-text") {
                                                    gst_pipeline.set_property("current-text", -1i32);
                                                }
                                                forced_text_off = true;
                                            }
                                        }
                                    }
                                }
                                MessageView::AsyncDone(_) => {
                                    // ── Detect HDR and update color management ──
                                    // After a state transition completes (PAUSED→PLAYING,
                                    // or after a seek) the caps are settled.  Query vsink
                                    // for colorimetry and notify the subsurface manager.
                                    if let Some(vsink) = gst_pipeline.by_name("vsink") {
                                        if let Some(pad) = vsink.static_pad("sink") {
                                            if let Some(caps) = pad.current_caps() {
                                                if let Some(s) = caps.structure(0) {
                                                    let colorimetry = s
                                                        .get::<String>("colorimetry")
                                                        .unwrap_or_default();
                                                    let pixel_format = s
                                                        .get::<String>("format")
                                                        .or_else(|_| s.get::<String>("drm-format"))
                                                        .unwrap_or_default();
                                                    if !colorimetry.is_empty() {
                                                        // Extract mastering display and content light level
                                                        let mastering = s
                                                            .get::<String>("mastering-display-info")
                                                            .ok();
                                                        let cll = s
                                                            .get::<String>("content-light-level")
                                                            .ok();

                                                        let mut meta = crate::color_management::HdrMetadata {
                                                            mastering_primaries: None,
                                                            mastering_luminance_min: None,
                                                            mastering_luminance_max: None,
                                                            max_cll: None,
                                                            max_fall: None,
                                                        };
                                                        if let Some(ref m) = mastering {
                                                            if let Some((prims, max_lum, min_lum)) =
                                                                crate::color_management::HdrMetadata::parse_mastering_display(m)
                                                            {
                                                                meta.mastering_primaries = Some(prims);
                                                                meta.mastering_luminance_max = Some(max_lum);
                                                                meta.mastering_luminance_min = Some(min_lum);
                                                            }
                                                        }
                                                        if let Some(ref c) = cll {
                                                            if let Some((max_cll, max_fall)) =
                                                                crate::color_management::HdrMetadata::parse_content_light_level(c)
                                                            {
                                                                meta.max_cll = Some(max_cll);
                                                                meta.max_fall = Some(max_fall);
                                                            }
                                                        }

                                                        // Only tag as HDR if the pixel format
                                                        // can actually carry HDR data.  If
                                                        // vapostproc already tone-mapped to
                                                        // BGRx/8-bit, the pixels are SDR even
                                                        // if colorimetry metadata says PQ.
                                                        let format_ok = crate::color_management::HdrMetadata::is_hdr_capable_format(&pixel_format);

                                                        let tx_cm = tx.clone();
                                                        let colorimetry_owned = colorimetry.clone();
                                                        let pixel_fmt_owned = pixel_format.clone();
                                                        let _ = tx_cm.send(Box::new(move |state: &mut Internal| {
                                                            if let Some(ref subs) = state.subsurface {
                                                                if format_ok {
                                                                    subs.notify_video_colorimetry(
                                                                        &colorimetry_owned,
                                                                        Some(&meta),
                                                                    );
                                                                } else {
                                                                    // 8-bit SDR pixel format — make sure
                                                                    // the surface is NOT tagged as HDR
                                                                    log::info!(
                                                                        "[color-mgmt] Pixel format {pixel_fmt_owned} is SDR; \
                                                                         NOT tagging surface as HDR despite PQ colorimetry"
                                                                    );
                                                                    subs.notify_video_colorimetry(
                                                                        "sdr-override",
                                                                        None,
                                                                    );
                                                                }
                                                            }
                                                        }));

                                                        log::info!(
                                                            "[color-mgmt] Detected colorimetry={colorimetry} format={pixel_format} hdr_capable={format_ok} mastering={mastering:?} cll={cll:?}"
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // If we are gating autoplay until seek completes, start playback now
                                    let tx_play = tx.clone();
                                    let pipeline_clone = gst_pipeline.clone();
                                    let _ = tx_play.send(Box::new(move |state: &mut Internal| {
                                        state.startup_async_done = true;

                                        // Re-send SelectStreams after seek completes.
                                        // A flushing seek (FLUSH_START → FLUSH_STOP)
                                        // discards any queued events, including the
                                        // SelectStreams we sent during StreamCollection.
                                        // Without re-sending, GStreamer 1.28's internal
                                        // re-selection can drop the audio stream,
                                        // leaving playback silent with no PipeWire node.
                                        if !state.selected_stream_ids.is_empty() {
                                            if let Some(p) = state.pipeline.as_ref() {
                                                let ids = state.selected_stream_ids.clone();
                                                if p.send_select_streams(&ids) {
                                                    log::info!(
                                                        "[streams] Re-sent SelectStreams ({} ids) after seek/AsyncDone",
                                                        ids.len()
                                                    );
                                                } else {
                                                    log::warn!(
                                                        "[streams] Failed to re-send SelectStreams after seek"
                                                    );
                                                }
                                            }
                                        }

                                        if state.pending_play_after_seek {
                                            // Optional check: confirm position advanced near target
                                            if let Some(pos) = pipeline_clone
                                                .query_position::<gst::ClockTime>()
                                                .map(|ct| Duration::from_nanos(ct.nseconds()))
                                            {
                                                let _target = state.pending_start_position;
                                                // We trust ACCURATE seek; no strict gating on delta required
                                                log::debug!("[seek] AsyncDone at {:?}", pos);
                                            }
                                            // Only auto-play if user hasn't requested pause
                                            if !state.user_paused {
                                                if let Some(p) = state.pipeline.clone() {
                                                    let _ = p.play();
                                                }
                                            } else {
                                                log::debug!("Autoplay gated by user pause; remaining paused");
                                            }
                                            state.pending_play_after_seek = false;
                                        }
                                    }));
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
            let ready_for_apply = {
                let r = self.0.read();
                r.pipeline.is_some() && r.startup_async_done
            };

            if ready_for_apply {
                // Best-effort apply; if still not ready for a specific operation, requeue.
                let requeue = self.apply_state_now(&st).is_err();
                if requeue {
                    let mut w = self.0.write();
                    w.pending_state = Some(st);
                }
            } else {
                // Wait for initial AsyncDone before first resume-state application.
                let mut w = self.0.write();
                w.pending_state = Some(st);
            }
        }

        // 3) (Optional) subtitle draining could happen here
    }

    // Control
    pub fn play(&self) -> Result<(), Error> {
        // Respect explicit user pause intent: do not auto-start if user paused
        let (user_paused, p) = {
            let r = self.0.read();
            (r.user_paused, r.pipeline.clone())
        };
        if user_paused {
            // Silently succeed; caller wanted to play but user has paused explicitly
            return Ok(());
        }
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

        // Only apply explicit track selections when present to avoid startup churn.
        if st.audio_track >= 0 {
            let _ = self.select_audio_track(st.audio_track);
        }

        if st.subtitles_enabled {
            if st.subtitle_track.is_some() {
                let _ = self.select_subtitle_track(st.subtitle_track);
            } else {
                let _ = self.set_subtitles_enabled(true);
            }
        } else if self.subtitles_enabled() {
            let _ = self.select_subtitle_track(None);
        }

        if let Some(url) = &st.subtitle_url {
            let _ = self.set_subtitle_url(url);
        }
        if self.seek(st.position, true).is_err() {
            return Err(());
        }
        self.set_volume(st.volume);
        self.set_muted(st.muted);
        let _ = self.set_playback_rate(st.speed);

        // Always gate play() behind AsyncDone when we just did a flushing
        // seek.  The seek flushes queued SelectStreams events, and calling
        // play() immediately triggers a PAUSED→PLAYING transition whose
        // text-pad activity can cause GStreamer 1.28 to re-select streams
        // without audio.  By deferring play() to AsyncDone, we re-send
        // SelectStreams *before* the PLAYING transition (see AsyncDone
        // handler), ensuring audio stays selected.
        if !st.paused {
            let mut w = self.0.write();
            w.pending_play_after_seek = true;
            w.user_paused = false;
        }
        let _ = self.pause();
        Ok(())
    }

    pub fn queue_pending_state(&self, st: PendingState) {
        let mut w = self.0.write();
        w.pending_state = Some(st);
    }

    /// Enable autoplay after the next seek completes (AsyncDone), starting exactly at position.
    pub fn enable_autoplay_after_seek(&mut self, position: Duration) {
        let mut w = self.0.write();
        w.pending_play_after_seek = true;
        w.pending_start_position = Some(position);
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
        let (p, mut new_ids, old_ids, audio_ids) = {
            let r = self.0.read();
            let p = r.pipeline.clone();
            if index < 0 || (index as usize) >= r.audio_index_to_stream_id.len() {
                return Err(Error::Pipeline(format!(
                    "Invalid audio track index: {}",
                    index
                )));
            }
            let mut ids = r.selected_stream_ids.clone();
            let old_ids = ids.clone();
            // Remove any existing audio IDs
            if !r.audio_index_to_stream_id.is_empty() {
                ids.retain(|id| !r.audio_index_to_stream_id.iter().any(|aid| aid == id));
            }
            (p, ids, old_ids, r.audio_index_to_stream_id.clone())
        };

        let Some(p) = p else {
            return Err(Error::Pipeline("Video not initialized".into()));
        };
        let target_id = audio_ids[index as usize].clone();
        // Append new audio id
        new_ids.push(target_id);
        // Dedup while preserving order
        dedup_in_place(&mut new_ids);

        // No-op: desired selection already active.
        if new_ids == old_ids {
            let mut w = self.0.write();
            w.current_audio_track = index;
            return Ok(());
        }

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
        let (p, mut new_ids, old_ids, sub_ids, pgs_ids, subsurface) = {
            let r = self.0.read();
            let p = r.pipeline.clone();
            let mut ids = r.selected_stream_ids.clone();
            let old_ids = ids.clone();
            if !r.subtitle_index_to_stream_id.is_empty() {
                ids.retain(|id| !r.subtitle_index_to_stream_id.iter().any(|sid| sid == id));
            }
            (
                p,
                ids,
                old_ids,
                r.subtitle_index_to_stream_id.clone(),
                r.pgs_stream_ids.clone(),
                r.subsurface.clone(),
            )
        };

        let Some(p) = p else {
            return Err(Error::Pipeline("Video not initialized".into()));
        };

        let mut new_current: Option<i32> = None;
        let mut enabled = false;
        let mut is_pgs = false;

        if let Some(i) = index {
            if i < 0 || (i as usize) >= sub_ids.len() {
                return Err(Error::Pipeline(format!(
                    "Invalid subtitle track index: {}",
                    i
                )));
            }
            let sid = sub_ids[i as usize].clone();

            // Check if this is a PGS/bitmap track
            is_pgs = pgs_ids.contains(&sid);

            if is_pgs {
                // PGS: add to SelectStreams so the demuxer actually pushes
                // data (our pad probe on the demuxer intercepts it for
                // decoding). The text-sink is a fakesink so PGS caps
                // negotiate fine — no green artifacts since subtitleoverlay
                // is bypassed (text-sink replaces it).
                log::info!("[subs] PGS track selected (index={i}, sid={sid}) — activating PGS decoder");
                self.0.read().pgs_active.store(true, Ordering::Relaxed);
                new_ids.push(sid);
            } else {
                // Text track — route through playbin3's text-sink
                // Deactivate PGS decoder if it was active
                self.0.read().pgs_active.store(false, Ordering::Relaxed);
                new_ids.push(sid);
            }
            new_current = Some(i);
            enabled = true;
        } else {
            // Subtitles disabled — clear the subtitle subsurface and deactivate PGS
            self.0.read().pgs_active.store(false, Ordering::Relaxed);
            if let Some(ref subs) = subsurface {
                let _ = subs.clear_subtitle();
            }
        }
        dedup_in_place(&mut new_ids);

        // For PGS tracks, we don't send SelectStreams (the PGS stream
        // stays unselected in playbin3; we intercept it separately).
        // Just update state.
        if is_pgs {
            let mut w = self.0.write();
            w.current_subtitle_track = new_current;
            w.subtitles_enabled = enabled;
            return Ok(());
        }

        // No-op for text tracks: desired selection already active.
        if new_ids == old_ids {
            let mut w = self.0.write();
            w.current_subtitle_track = new_current;
            w.subtitles_enabled = enabled;
            return Ok(());
        }

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
