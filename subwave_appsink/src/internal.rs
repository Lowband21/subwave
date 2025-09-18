use std::{
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, Instant},
};

use gstreamer::{
    self as gst,
    glib::object::{Cast, ObjectExt},
    prelude::{ElementExt, ElementExtManual, GstBinExt},
};
use subwave_core::{
    Error,
    video::types::{AudioTrack, Position, SubtitleTrack, VideoProperties},
};

#[derive(Debug)]
pub(crate) struct Internal {
    pub(crate) id: u64,

    pub(crate) bus: gst::Bus,
    pub(crate) source: gst::Pipeline,
    pub(crate) alive: Arc<AtomicBool>,
    pub(crate) worker: Option<std::thread::JoinHandle<()>>,

    pub(crate) video_props: Arc<Mutex<VideoProperties>>,
    pub(crate) duration: Duration,
    pub(crate) speed: f64,
    pub(crate) sync_av: bool,

    pub(crate) frame: Arc<Mutex<Vec<u8>>>,
    pub(crate) upload_frame: Arc<AtomicBool>,
    pub(crate) last_frame_time: Arc<Mutex<Instant>>,
    pub(crate) looping: bool,
    pub(crate) is_eos: bool,
    pub(crate) restart_stream: bool,
    pub(crate) sync_av_avg: u64,
    pub(crate) sync_av_counter: u64,

    // Cache seek position to return during seeks
    pub(crate) seek_position: Option<Duration>,
    pub(crate) last_valid_position: Duration,

    // Buffering state
    pub(crate) is_buffering: bool,
    pub(crate) buffering_percent: i32,
    pub(crate) user_paused: bool, // Track if user manually paused

    // Connection monitoring
    pub(crate) current_bitrate: u64, // bits per second
    pub(crate) avg_in_rate: i64,     // average input rate from queue2

    // Error recovery
    pub(crate) last_error_time: Option<Instant>,
    pub(crate) error_count: u32,
    pub(crate) is_reconnecting: bool,

    // Subtitle tracking
    pub(crate) available_subtitles: Vec<SubtitleTrack>,
    pub(crate) current_subtitle_track: Option<i32>,
    pub(crate) subtitles_enabled: bool,

    // Audio track tracking
    pub(crate) available_audio_tracks: Vec<AudioTrack>,
    pub(crate) current_audio_track: i32,

    // Stream collection for playbin3
    pub(crate) stream_collection: Option<gst::StreamCollection>,
    pub(crate) selected_stream_ids: Vec<String>,
    // HDR metadata
    //pub(crate) hdr_metadata: Option<HdrMetadata>,
}

impl Internal {
    pub(crate) fn seek(
        &mut self,
        position: impl Into<Position>,
        accurate: bool,
    ) -> Result<(), Error> {
        let position = position.into();

        // Check if this is a network stream
        // For now, assume we're dealing with network streams when seeking issues arise
        // This avoids potential property access issues
        let is_network_stream = true; // Conservative approach for debugging

        // Clear any previous seek position
        self.seek_position = None;

        let state = self.source.state(gst::ClockTime::ZERO);
        log::debug!(
            "Seeking to {:?}, accurate={}, network={}, state={:?}",
            position,
            accurate,
            is_network_stream,
            state
        );

        // Check if we're in a seekable state
        if state.1 == gst::State::Null {
            log::error!("Cannot seek: pipeline is in NULL state");
            return Err(Error::InvalidState);
        }

        // For network streams, check if we can seek
        /*
        if is_network_stream {
            // Query if seeking is possible
            let mut query = gst::query::Seeking::new(gst::Format::Time);
            if self.source.query(&mut query) {
                let (seekable, start, end) = query.result();
                log::debug!(
                    "Seeking query result: seekable={}, start={:?}, end={:?}",
                    seekable,
                    start,
                    end
                );
                if !seekable {
                    log::error!("Stream is not seekable");
                    return Err(Error::InvalidState);
                }
            } else {
                log::warn!("Failed to query seeking capabilities");
            }
        }
        */

        // Build seek flags
        let mut flags = gst::SeekFlags::FLUSH;

        if accurate {
            flags |= gst::SeekFlags::ACCURATE;
        } else {
            // Use keyframe seeking for faster seeks when accuracy not required
            flags |= gst::SeekFlags::KEY_UNIT;
        }

        // Perform the seek
        let result = match &position {
            Position::Time(time) => self
                .source
                .seek_simple(flags, gst::ClockTime::from_nseconds(time.as_nanos() as u64)),
            Position::Frame(_) => {
                // Frame seeking is more complex, use full seek
                self.source.seek(
                    self.speed,
                    flags,
                    gst::SeekType::Set,
                    gst::GenericFormattedValue::from(position),
                    gst::SeekType::None,
                    gst::format::Default::NONE,
                )
            }
        };

        if let Err(e) = result {
            log::error!("Seek failed: {:?}", e);
            return Err(Error::InvalidState);
        }

        log::debug!("Seek initiated successfully");
        Ok(())
    }

    pub(crate) fn set_speed(&mut self, speed: f64) -> Result<(), Error> {
        let Some(position) = self.source.query_position::<gst::ClockTime>() else {
            return Err(Error::Caps);
        };
        if speed > 0.0 {
            self.source.seek(
                speed,
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::SeekType::Set,
                position,
                gst::SeekType::End,
                gst::ClockTime::from_seconds(0),
            )?;
        } else {
            self.source.seek(
                speed,
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::SeekType::Set,
                gst::ClockTime::from_seconds(0),
                gst::SeekType::Set,
                position,
            )?;
        }
        self.speed = speed;
        Ok(())
    }

    pub(crate) fn restart_stream(&mut self) -> Result<(), Error> {
        self.is_eos = false;
        self.set_paused(false);
        self.seek(0, false)?;
        Ok(())
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        // Track user-initiated pause state
        self.user_paused = paused;

        // Only change state if not buffering, or if explicitly pausing
        if !self.is_buffering || paused {
            self.source
                .set_state(if paused {
                    gst::State::Paused
                } else {
                    gst::State::Playing
                })
                .unwrap(/* state was changed in ctor; state errors caught there */);
        }

        // Set restart_stream flag to make the stream restart on the next Message::NextFrame
        if self.is_eos && !paused {
            self.restart_stream = true;
        }
    }

    pub(crate) fn paused(&self) -> bool {
        self.source.state(gst::ClockTime::ZERO).1 == gst::State::Paused
    }

    pub(crate) fn update_position_cache(&mut self) {
        // Try to get current position
        if let Some(pos) = self.source.query_position::<gst::ClockTime>() {
            let duration = Duration::from_nanos(pos.nseconds());
            self.last_valid_position = duration;
            // Clear seek position if we have a valid position
            if self.seek_position.is_some() {
                log::debug!("Clearing seek position, got valid position: {:?}", duration);
                self.seek_position = None;
            }
        }
    }

    /// Syncs audio with video when there is (inevitably) latency presenting the frame.
    pub(crate) fn set_av_offset(&mut self, offset: Duration) {
        if self.sync_av {
            self.sync_av_counter += 1;
            self.sync_av_avg = self.sync_av_avg * (self.sync_av_counter - 1) / self.sync_av_counter
                + offset.as_nanos() as u64 / self.sync_av_counter;
            if self.sync_av_counter.is_multiple_of(128) {
                self.source
                    .set_property("av-offset", -(self.sync_av_avg as i64));
            }
        }
    }

    /// Monitor connection speed from queue2 buffer statistics
    pub(crate) fn update_connection_stats(&mut self) {
        // Try to find the queue2 element in our video sink
        let Some(video_sink) = self.source.property::<Option<gst::Element>>("video-sink") else {
            return;
        };
        if let Ok(video_sink_bin) = video_sink.dynamic_cast::<gst::Bin>()
<<<<<<< HEAD
            && let Some(buffer) = video_sink_bin.by_name("video-buffer") {
                // Check if this is actually a queue2 element that has the properties we need
                if buffer.has_property("avg-in-rate") {
                    // Get average input rate
                    let avg_in: u64 = buffer.property("avg-in-rate");
                    if avg_in > 0 {
                        self.avg_in_rate = avg_in;
                        log::trace!("Queue2 average input rate: {} bytes/sec", avg_in);
                    }
||||||| parent of 80f6bfb (feat: zerocopy video but no subtitles)
            && let Some(buffer) = video_sink_bin.by_name("video-buffer")
        {
            // Check if this is actually a queue2 element that has the properties we need
            if buffer.has_property("avg-in-rate") {
                // Get average input rate
                let avg_in: u64 = buffer.property("avg-in-rate");
                if avg_in > 0 {
                    self.avg_in_rate = avg_in;
                    log::trace!("Queue2 average input rate: {} bytes/sec", avg_in);
                }
=======
            && let Some(buffer) = video_sink_bin.by_name("video-buffer")
        {
            // Check if this is actually a queue2 element that has the properties we need
            if buffer.has_property("avg-in-rate") {
                // Get average input rate (queue2 exposes it as gint64)
                let avg_in_signed: i64 = buffer.property("avg-in-rate");
                if avg_in_signed > 0 {
                    let avg_in = avg_in_signed;
                    self.avg_in_rate = avg_in;
                    log::trace!("Queue2 average input rate: {} bytes/sec", avg_in);
                }
>>>>>>> 80f6bfb (feat: zerocopy video but no subtitles)

<<<<<<< HEAD
                    // Get current level bytes for monitoring
                    if buffer.has_property("current-level-bytes") {
                        let current_level: u64 = buffer.property("current-level-bytes");
                        log::trace!("Queue2 current buffer level: {} bytes", current_level);
                    }
||||||| parent of 80f6bfb (feat: zerocopy video but no subtitles)
                // Get current level bytes for monitoring
                if buffer.has_property("current-level-bytes") {
                    let current_level: u64 = buffer.property("current-level-bytes");
                    log::trace!("Queue2 current buffer level: {} bytes", current_level);
                }
=======
                // Get current level bytes for monitoring
                if buffer.has_property("current-level-bytes") {
                    let current_level: u32 = buffer.property("current-level-bytes");
                    log::trace!("Queue2 current buffer level: {} bytes", current_level);
                }
>>>>>>> 80f6bfb (feat: zerocopy video but no subtitles)

<<<<<<< HEAD
                    // Update connection speed on playbin based on measured rate
                    if self.avg_in_rate > 0 {
                        // Convert bytes/sec to bits/sec
                        let bits_per_sec = self.avg_in_rate * 8;
                        self.source.set_property("connection-speed", bits_per_sec);
                        self.current_bitrate = bits_per_sec;
                    }
                } else {
                    log::trace!("Buffer element is not queue2, skipping stats update");
||||||| parent of 80f6bfb (feat: zerocopy video but no subtitles)
                // Update connection speed on playbin based on measured rate
                if self.avg_in_rate > 0 {
                    // Convert bytes/sec to bits/sec
                    let bits_per_sec = self.avg_in_rate * 8;
                    self.source.set_property("connection-speed", bits_per_sec);
                    self.current_bitrate = bits_per_sec;
=======
                // Update connection speed on playbin based on measured rate
                if self.avg_in_rate > 0 {
                    // Convert bytes/sec to bits/sec
                    let bits_per_sec: u64 = self.avg_in_rate.saturating_mul(8) as u64;
                    self.source.set_property("connection-speed", bits_per_sec);
                    self.current_bitrate = bits_per_sec;
>>>>>>> 80f6bfb (feat: zerocopy video but no subtitles)
                }
            }
    }

    /// Check if error should trigger reconnection attempt
    pub(crate) fn should_retry_on_error(&mut self, error: &gst::glib::Error) -> bool {
        // Check if this is a network-related error
        let is_network_error = error.to_string().to_lowercase().contains("http")
            || error.to_string().to_lowercase().contains("connection")
            || error.to_string().to_lowercase().contains("timeout")
            || error.to_string().to_lowercase().contains("network");

        if !is_network_error {
            return false;
        }

        // Implement exponential backoff
        let now = Instant::now();
        if let Some(last_error) = self.last_error_time {
            let time_since_error = now.duration_since(last_error);
            let backoff_duration = Duration::from_secs(2u64.pow(self.error_count.min(5)));

            if time_since_error < backoff_duration {
                log::debug!(
                    "Skipping retry, backoff time not elapsed: {:?} remaining",
                    backoff_duration - time_since_error
                );
                return false;
            }
        }

        self.last_error_time = Some(now);
        self.error_count += 1;

        // Give up after 5 attempts
        if self.error_count > 5 {
            log::error!("Max retry attempts reached, giving up");
            return false;
        }

        true
    }

    /// Attempt to reconnect after network error
    pub(crate) fn attempt_reconnect(&mut self) -> Result<(), Error> {
        if self.is_reconnecting {
            return Ok(()); // Already reconnecting
        }

        self.is_reconnecting = true;
        log::info!("Attempting to reconnect, attempt #{}", self.error_count);

        // Get current position before reconnecting
        let current_position = self.last_valid_position;

        // Set pipeline to READY state to reset connection
        self.source.set_state(gst::State::Ready)?;

        // Small delay to let the pipeline settle
        std::thread::sleep(Duration::from_millis(100));

        // Set back to playing state
        self.source.set_state(gst::State::Playing)?;

        // Seek to last known position
        if current_position > Duration::ZERO {
            self.seek(current_position, false)?;
        }

        self.is_reconnecting = false;
        log::info!("Reconnection attempt completed");

        Ok(())
    }

    /// Reset error state after successful playback
    pub(crate) fn reset_error_state(&mut self) {
        if self.error_count > 0 {
            log::debug!("Resetting error state after successful playback");
            self.error_count = 0;
            self.last_error_time = None;
        }
    }

    // TODO: Add fallback stream collection query?
    /// Return available subtitles
    pub(crate) fn query_subtitle_tracks(&mut self) -> Vec<SubtitleTrack> {
        if !self.available_subtitles.is_empty() {
            log::info!(
                "Returning {} subtitle tracks from stream collection",
                self.available_subtitles.len()
            );
            return self.available_subtitles.clone();
        }

        log::warn!("No subtitle tracks in stream collection, returning empty");
        Vec::new()
    }

    /// Select a specific subtitle track
    pub(crate) fn select_subtitle_track(&mut self, track_index: Option<i32>) -> Result<(), Error> {
        // Make sure we have a stream collection
        let collection = match &self.stream_collection {
            Some(c) => c,
            None => {
                log::error!("No stream collection available");
                return Err(Error::InvalidState);
            }
        };

        // Build new stream selection list
        let mut new_selection = Vec::new();

        // Find and add video stream(s)
        for i in 0..collection.len() {
            if let Some(stream) = collection.stream(i as u32)
                && stream.stream_type() == gst::StreamType::VIDEO {
                    // Check if this stream was previously selected
                    if let Some(stream_id) = stream.stream_id() {
                        let stream_id_str = stream_id.to_string();
                        if self.selected_stream_ids.contains(&stream_id_str) {
                            new_selection.push(stream_id_str);
                        }
                    }
                }
        }

        // Find and add audio stream(s)
        let mut audio_index = 0;
        for i in 0..collection.len() {
            if let Some(stream) = collection.stream(i as u32)
                && stream.stream_type() == gst::StreamType::AUDIO {
                    if audio_index == self.current_audio_track {
                        new_selection.push(
                            stream
                                .stream_id()
                                .map(|id| id.to_string())
                                .unwrap_or_else(|| String::from("unknown")),
                        );
                    }
                    audio_index += 1;
                }
        }

        // Handle subtitle selection
        match track_index {
            Some(index) => {
                // Validate index
                if index < 0 || index >= self.available_subtitles.len() as i32 {
                    log::error!(
                        "Invalid subtitle track index: {} (available: 0-{})",
                        index,
                        self.available_subtitles.len() - 1
                    );
                    return Err(Error::InvalidState);
                }

                // Find and add the subtitle stream
                let mut subtitle_index = 0;
                for i in 0..collection.len() {
                    if let Some(stream) = collection.stream(i as u32)
                        && stream.stream_type() == gst::StreamType::TEXT {
                            if subtitle_index == index {
                                new_selection.push(
                                    stream
                                        .stream_id()
                                        .map(|id| id.to_string())
                                        .unwrap_or_else(|| String::from("unknown")),
                                );
                                break;
                            }
                            subtitle_index += 1;
                        }
                }

                self.current_subtitle_track = Some(index);
                self.subtitles_enabled = true;

                log::info!("Selected subtitle track {}", index);
            }
            None => {
                // Don't add any subtitle streams to disable subtitles
                self.current_subtitle_track = None;
                self.subtitles_enabled = false;

                log::info!("Disabled subtitles");
            }
        }

        // Update the selected stream IDs and send the event
        self.selected_stream_ids = new_selection;
        self.send_stream_selection()
    }

    /// Enable or disable subtitles
    pub(crate) fn set_subtitles_enabled(&mut self, enabled: bool) {
        let prev_state = self.subtitles_enabled;
        self.subtitles_enabled = enabled;

        log::info!("set_subtitles_enabled: {} -> {}", prev_state, enabled);

        if enabled {
            // Re-enable the previously selected track
            if let Some(track) = self.current_subtitle_track {
                // Use select_subtitle_track which will handle the stream selection
                if let Err(e) = self.select_subtitle_track(Some(track)) {
                    log::error!("Failed to re-enable subtitle track: {:?}", e);
                }
            } else {
                log::warn!("Subtitles enabled but no track previously selected");
            }
        } else {
            // Disable subtitles by selecting None
            if let Err(e) = self.select_subtitle_track(None) {
                log::error!("Failed to disable subtitles: {:?}", e);
            }
        }
    }

    /// Query available audio tracks
    pub(crate) fn query_audio_tracks(&mut self) -> Vec<AudioTrack> {
        // For playbin3, tracks are already populated via stream collection
        if !self.available_audio_tracks.is_empty() {
            log::info!(
                "Returning {} audio tracks from stream collection",
                self.available_audio_tracks.len()
            );
            return self.available_audio_tracks.clone();
        }

        // Fallback to old method for compatibility (shouldn't happen with playbin3)
        log::warn!("No audio tracks in stream collection, falling back to old method");
        self.available_audio_tracks.clone()
    }

    /// Select a specific audio track
    pub(crate) fn select_audio_track(&mut self, track_index: i32) -> Result<(), Error> {
        // Make sure we have a stream collection
        let collection = match &self.stream_collection {
            Some(c) => c,
            None => {
                log::error!("No stream collection available");
                return Err(Error::InvalidState);
            }
        };

        // Validate index
        if track_index < 0 || track_index >= self.available_audio_tracks.len() as i32 {
            log::error!(
                "Invalid audio track index: {} (available: 0-{})",
                track_index,
                self.available_audio_tracks.len() - 1
            );
            return Err(Error::InvalidState);
        }

        // Build new stream selection list
        let mut new_selection = Vec::new();

        // Find and add video stream(s)
        for i in 0..collection.len() {
            if let Some(stream) = collection.stream(i as u32)
                && stream.stream_type() == gst::StreamType::VIDEO {
                    // Check if this stream was previously selected
                    if let Some(stream_id) = stream.stream_id() {
                        let stream_id_str = stream_id.to_string();
                        if self.selected_stream_ids.contains(&stream_id_str) {
                            new_selection.push(stream_id_str);
                        }
                    }
                }
        }

        // Find and add the selected audio stream
        let mut audio_index = 0;
        for i in 0..collection.len() {
            if let Some(stream) = collection.stream(i as u32)
                && stream.stream_type() == gst::StreamType::AUDIO {
                    if audio_index == track_index {
                        new_selection.push(
                            stream
                                .stream_id()
                                .map(|id| id.to_string())
                                .unwrap_or_else(|| String::from("unknown")),
                        );
                    }
                    audio_index += 1;
                }
        }

        // Add current subtitle stream if enabled
        if self.subtitles_enabled
            && let Some(subtitle_track) = self.current_subtitle_track {
                let mut subtitle_index = 0;
                for i in 0..collection.len() {
                    if let Some(stream) = collection.stream(i as u32)
                        && stream.stream_type() == gst::StreamType::TEXT {
                            if subtitle_index == subtitle_track {
                                new_selection.push(
                                    stream
                                        .stream_id()
                                        .map(|id| id.to_string())
                                        .unwrap_or_else(|| String::from("unknown")),
                                );
                                break;
                            }
                            subtitle_index += 1;
                        }
                }
            }

        self.current_audio_track = track_index;

        log::info!("Selected audio track {}", track_index);

        // Update the selected stream IDs and send the event
        self.selected_stream_ids = new_selection;
        self.send_stream_selection()
    }

    /// Process stream collection message for playbin3
    pub(crate) fn update_stream_collection(&mut self, collection: gst::StreamCollection) {
        log::info!(
            "Received stream collection with {} streams",
            collection.len()
        );

        // Store the collection
        self.stream_collection = Some(collection.clone());

        // Clear existing track lists
        self.available_audio_tracks.clear();
        self.available_subtitles.clear();
        self.selected_stream_ids.clear();

        // Process each stream in the collection
        for i in 0..collection.len() {
            if let Some(stream) = collection.stream(i as u32) {
                let stream_id = stream.stream_id();
                let stream_type = stream.stream_type();

                log::debug!(
                    "Stream {}: id={:?}, type={:?}, flags={:?}",
                    i,
                    stream_id,
                    stream_type,
                    stream.stream_flags()
                );

                // Get stream caps and tags
                let caps = stream.caps();
                let tags = stream.tags();

                match stream_type {
                    gst::StreamType::AUDIO => {
                        let mut audio_track = AudioTrack {
                            index: self.available_audio_tracks.len() as i32,
                            language: None,
                            title: None,
                            codec: None,
                            channels: None,
                            sample_rate: None,
                        };

                        // Extract metadata from tags if available
                        if let Some(tags) = tags {
                            if let Some(lang) = tags.get::<gst::tags::LanguageCode>() {
                                audio_track.language = Some(lang.get().to_string());
                            }
                            if let Some(title) = tags.get::<gst::tags::Title>() {
                                audio_track.title = Some(title.get().to_string());
                            }
                            if let Some(codec) = tags.get::<gst::tags::AudioCodec>() {
                                audio_track.codec = Some(codec.get().to_string());
                            }
                        }

                        // Extract info from caps if available
                        if let Some(caps) = caps
                            && let Some(s) = caps.structure(0) {
                                if let Ok(rate) = s.get::<i32>("rate") {
                                    audio_track.sample_rate = Some(rate);
                                }
                                if let Ok(channels) = s.get::<i32>("channels") {
                                    audio_track.channels = Some(channels);
                                }
                            }

                        // If stream is selected by default, track it
                        if stream.stream_flags().contains(gst::StreamFlags::SELECT)
                            && let Some(id) = stream_id {
                                self.selected_stream_ids.push(id.to_string());
                                self.current_audio_track = audio_track.index;
                            }

                        self.available_audio_tracks.push(audio_track);
                    }
                    gst::StreamType::TEXT => {
                        let mut subtitle_track = SubtitleTrack {
                            index: self.available_subtitles.len() as i32,
                            language: None,
                            title: None,
                            codec: None,
                        };

                        // Extract metadata from tags if available
                        if let Some(tags) = tags {
                            if let Some(lang) = tags.get::<gst::tags::LanguageCode>() {
                                subtitle_track.language = Some(lang.get().to_string());
                            }
                            if let Some(title) = tags.get::<gst::tags::Title>() {
                                subtitle_track.title = Some(title.get().to_string());
                            }
                            if let Some(codec) = tags.get::<gst::tags::VideoCodec>() {
                                subtitle_track.codec = Some(codec.get().to_string());
                            } else if let Some(codec) = tags.get::<gst::tags::Codec>() {
                                subtitle_track.codec = Some(codec.get().to_string());
                            }
                        }

                        // If stream is selected by default, track it
                        if stream.stream_flags().contains(gst::StreamFlags::SELECT)
                            && let Some(id) = stream_id {
                                self.selected_stream_ids.push(id.to_string());
                                self.current_subtitle_track = Some(subtitle_track.index);
                                self.subtitles_enabled = true;
                            }

                        self.available_subtitles.push(subtitle_track);
                    }
                    gst::StreamType::VIDEO => {
                        // Track selected video streams
                        if stream.stream_flags().contains(gst::StreamFlags::SELECT)
                            && let Some(id) = stream_id {
                                self.selected_stream_ids.push(id.to_string());
                            }
                    }
                    _ => {
                        log::debug!("Ignoring stream of type {:?}", stream_type);
                    }
                }
            }
        }

        log::info!(
            "Found {} audio tracks, {} subtitle tracks",
            self.available_audio_tracks.len(),
            self.available_subtitles.len()
        );
        log::info!("Selected streams: {:?}", self.selected_stream_ids);
    }

    /// Send stream selection event for playbin3
    pub(crate) fn send_stream_selection(&mut self) -> Result<(), Error> {
        if self.selected_stream_ids.is_empty() {
            log::warn!("No streams selected, skipping stream selection event");
            return Ok(());
        }

        log::info!("Sending stream selection: {:?}", self.selected_stream_ids);

        // Create SELECT_STREAMS event
        let stream_refs: Vec<&str> = self
            .selected_stream_ids
            .iter()
            .map(|s| s.as_str())
            .collect();
        let event = gst::event::SelectStreams::new(stream_refs);

        // Send event to the pipeline
        if !self.source.send_event(event) {
            log::error!("Failed to send SELECT_STREAMS event");
            return Err(Error::InvalidState);
        }

        Ok(())
    }
}
