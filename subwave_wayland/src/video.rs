use crate::internal::Internal;
use crate::position::Position;
use crate::{
    pipeline::SubsurfacePipeline,
    subsurface_manager::WaylandSubsurfaceManager,
    Error,
    Result,
    WaylandIntegration,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer::State;
use parking_lot::RwLock;
use subwave_core::types::{AudioTrack, SubtitleTrack};
use subwave_core::video_trait::Video;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// Video is an exterior-facing newtype with a single interior RwLock
pub struct SubsurfaceVideo(pub(crate) RwLock<Internal>);

// Bus commands are closures applied on Internal on the UI thread
pub type Cmd = Box<dyn FnOnce(&mut Internal) + Send + 'static>;

impl Video for SubsurfaceVideo {
    pub fn new(uri: &url::Url) -> Result<Self> {
        let inner = Internal {
            uri: uri.clone(),
            pipeline: None,
            subsurface: None,
            video_props: None,
            duration: None,
            speed: 1.0,
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
            last_position_update: Instant::now(),
        };
        Ok(SubsurfaceVideo(RwLock::new(inner)))
    }

    // Initialize Wayland and the playback pipeline. Spawns a bus thread that translates
    // GStreamer messages into small commands (closures) that are applied on the UI thread.
    pub fn init_wayland(
        &self,
        integration: WaylandIntegration,
        bounds: (i32, i32, i32, i32),
    ) -> Result<()> {
        // Construct subsurface and pipeline (no lock held during external calls)
        let subsurface = WaylandSubsurfaceManager::new(integration.clone())?;
        let pipeline = Arc::new(SubsurfacePipeline::new(
            &self.0.read().uri,
            &subsurface,
            &integration,
            bounds,
        )?);

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
                        // Fallbacks if needed
                        //if let Some(vsink) = pipe.by_name("vsink") {
                        //    let evt2 = gst::event::SelectStreams::new(ids.iter().map(|s| s.as_str()));
                        //    if vsink.send_event(evt2) {
                        //        return true;
                        //    }
                        //}
                        //if let Some(asink) = pipe.by_name("audiosink") {
                        //    let evt3 = gst::event::SelectStreams::new(ids.iter().map(|s| s.as_str()));
                        //    if asink.send_event(evt3) {
                        //        return true;
                        //    }
                        //}
                        false
                    }

                    while !stop.load(Ordering::SeqCst) {
                        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                            match msg.view() {
                                MessageView::Eos(_) => {
                                    //if tx.send(Box::new(|s: &mut Internal| s.state //= State::Stopped)).is_err() {
                                    //    log::debug!("[bus] receiver dropped; //exiting bus thread");
                                    //    break;
                                    //}
                                    break;
                                }
                                MessageView::Error(err) => {
                                    log::error!("Pipeline error: {:?}", err);
                                    //if tx.send(Box::new(|s: &mut Internal| //s.pipeline = State::Paused)).is_err() {
                                    //    log::debug!("[bus] receiver dropped; //exiting bus thread");
                                    //    break;
                                    //}
                                    break;
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
                                    if let Some(aid) = audio_ids.get(0) { initial_ids.push(aid.clone()); }
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
                                    let mut n_audio = 0;
                                    let mut n_subtitle = 0;
                                    for i in 0..collection.len() {
                                        if let Some(stream) = collection.stream(i as u32) {
                                            let st = stream.stream_type();
                                            if st.contains(gst::StreamType::AUDIO) { n_audio += 1; }
                                            if st.contains(gst::StreamType::TEXT) { n_subtitle += 1; }
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
    pub fn tick(&self) {
        // 1) Apply pending commands with a short write lock
        {
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
        }
        // 2) Drain subtitle frames without holding the lock
        //if let Some(p) = self.0.read().pipeline.clone() {
        //    p.drain_subtitles();
        //}
    }

    // Control
    pub fn play(&self) -> Result<()> {
        let p = self.0.read().pipeline.clone();
        if let Some(p) = p {
            p.play()?;
            self.0.write().state = PlaybackState::Playing;
            Ok(())
        } else {
            Err(Error::Pipeline("Video not initialized".into()))
        }
    }

    pub fn pause(&self) -> Result<()> {
        let p = self.0.read().pipeline.clone();
        if let Some(p) = p {
            p.pause()?;
            self.0.write().state = PlaybackState::Paused;
            Ok(())
        } else {
            Err(Error::Pipeline("Video not initialized".into()))
        }
    }

    pub fn stop(&self) -> Result<()> {
        // Signal thread and join
        let handle = {
            let mut w = self.0.write();
            w.bus_stop.store(true, Ordering::SeqCst);
            w.bus_thread.take()
        };
        if let Some(h) = handle {
            let _ = h.join();
        }
        // Stop pipeline
        if let Some(p) = self.0.read().pipeline.clone() {
            p.stop()?;
            self.0.write().state = PlaybackState::Stopped;
        }
        Ok(())
    }

    pub fn toggle_play(&self) -> Result<()> {
        if self.is_playing() {
            self.pause()
        } else {
            self.play()
        }
    }

    // Queries
    pub fn is_playing(&self) -> bool {
        self.0.read().state == PlaybackState::Playing
    }
    pub fn is_paused(&self) -> bool {
        self.0.read().state == PlaybackState::Paused
    }

    pub fn position(&self) -> Option<Duration> {
        self.0
            .read()
            .pipeline
            .as_ref()
            .and_then(|p| p.pipeline.query_position::<gst::ClockTime>())
            .map(|ct| Duration::from_nanos(ct.nseconds()))
    }

    pub fn duration(&self) -> Option<Duration> {
        self.0
            .read()
            .pipeline
            .as_ref()
            .and_then(|p| p.pipeline.query_duration::<gst::ClockTime>())
            .map(|ct| Duration::from_nanos(ct.nseconds()))
    }

    pub fn seek(&self, position: impl Into<Position>, accurate: bool) -> Result<()> {
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
        if let Some(p) = &self.0.read().pipeline {
            p.set_render_rectangle(x_offset, y_offset, width, height);
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
    pub fn set_volume(&self, volume: f64) -> Result<()> {
        if let Some(p) = self.0.read().pipeline.clone() {
            p.set_volume(volume)
        } else {
            Ok(())
        }
    }

    pub fn set_playback_rate(&self, rate: f64) -> Result<()> {
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
            w.metadata.current_audio_track
        }
    }

    pub fn current_subtitle_track(&self) -> Option<i32> {
        self.0.read().current_subtitle_track
    }

    pub fn audio_tracks_info(&self) -> Vec<WaylandAudioTrack> {
        self.0.read().available_audio_tracks.clone()
    }

    pub fn subtitle_tracks_info(&self) -> Vec<WaylandSubtitleTrack> {
        self.0.read().available_subtitle_tracks.clone()
    }

    pub fn select_audio_track(&self, index: i32) -> Result<()> {
        // Gather required info without holding the lock during GStreamer calls
        let (p, mut new_ids, audio_ids) = {
            let r = self.0.read();
            let p = r.pipeline.clone();
            if index < 0 || (index as usize) >= r.audio_index_to_stream_id.len() {
                return Err(Error::Pipeline(format!("Invalid audio track index: {}", index)));
            }
            let mut ids = r.selected_stream_ids.clone();
            // Remove any existing audio IDs
            if !r.audio_index_to_stream_id.is_empty() {
                ids.retain(|id| !r.audio_index_to_stream_id.iter().any(|aid| aid == id));
            }
            (p, ids, r.audio_index_to_stream_id.clone())
        };

        let Some(p) = p else { return Err(Error::Pipeline("Video not initialized".into())); };
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
            Err(Error::Pipeline("Failed to send SelectStreams for audio".into()))
        }
    }

    pub fn select_subtitle_track(&self, index: Option<i32>) -> Result<()> {
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

        let Some(p) = p else { return Err(Error::Pipeline("Video not initialized".into())); };

        let mut new_current: Option<i32> = None;
        let mut enabled = false;
        if let Some(i) = index {
            if i < 0 || (i as usize) >= sub_ids.len() {
                return Err(Error::Pipeline(format!("Invalid subtitle track index: {}", i)));
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
            Err(Error::Pipeline("Failed to send SelectStreams for subtitles".into()))
        }
    }

    pub fn subtitles_enabled(&self) -> bool {
        self.0.read().subtitles_enabled
    }

    pub fn set_subtitles_enabled(&self, enabled: bool) -> Result<()> {
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
            if let Some(i) = default_idx { self.select_subtitle_track(Some(i)) } else { Ok(()) }
        } else {
            // Disable
            self.select_subtitle_track(None)
        }
    }

    // Metadata accessors
    pub fn metadata(&self) -> VideoMetadata {
        self.0.read().metadata.clone()
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

impl Drop for Video {
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
