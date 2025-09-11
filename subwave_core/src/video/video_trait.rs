use std::time::Duration;
use gstreamer as gst;

use crate::{video::types::{AudioTrack, Position, SubtitleTrack}, Error};

pub trait Video {
    type Video: Video;
    /// Create a new video instance from a given video which loads from `uri`.
    /// Note that live sources will report the duration to be zero.
    fn new(uri: &url::Url) -> Result<Self::Video, Error>;

    /// Get the size/resolution of the video as `(width, height)`.
    fn size(&self) -> (i32, i32);

    /// Get the framerate of the video as frames per second.
    fn framerate(&self) -> f64;

    /// Get the volume multiplier of the audio.
    fn volume(&self) -> f64;

    /// Set the volume multiplier of the audio.
    /// `0.0` = 0% volume, `1.0` = 100% volume.
    ///
    /// This uses a linear scale, for example `0.5` is perceived as half as loud.
    fn set_volume(&mut self, volume: f64);

    /// Get if the audio is muted or not.
    fn muted(&self) -> bool;

    /// Set if the audio is muted or not, without changing the volume.
    fn set_muted(&mut self, muted: bool);

    /// Get if the stream ended or not.
    fn eos(&self) -> bool;

    /// Get if the media will loop or not.
    fn looping(&self) -> bool;

    /// Set if the media will loop or not.
    fn set_looping(&mut self, looping: bool);

    /// Restarts a stream; seeks to the first frame and unpauses, sets the `eos` flag to false.
    fn restart_stream(&mut self) -> Result<(), Error>;

    /// Get if the media is paused or not.
    fn paused(&self) -> bool;

    /// Set if the media is paused or not.
    fn set_paused(&mut self, paused: bool);

    /// Get the current playback speed.
    fn speed(&self) -> f64;

    /// Set the playback speed of the media.
    /// The default speed is `1.0`.
    fn set_speed(&mut self, speed: f64) -> Result<(), Error>;

    /// Get the current playback position in time.
    fn position(&self) -> Duration;

    /// Jumps to a specific position in the media.
    /// Passing `true` to the `accurate` parameter will result in more accurate seeking,
    /// however, it is also slower. For most seeks (e.g., scrubbing) this is not needed.
    fn seek(&mut self, position: impl Into<Position>, accurate: bool) -> Result<(), Error>;

    /// Get the media duration.
    fn duration(&self) -> Duration;

    /// Get the current subtitle URL.
    fn subtitle_url(&self) -> Option<url::Url>;

    /// Set the subtitle URL to display.
    fn set_subtitle_url(&mut self, url: &url::Url) -> Result<(), Error>;

    /// Check if subtitles are enabled
    fn subtitles_enabled(&self) -> bool;

    /// Enable or disable subtitle display
    fn set_subtitles_enabled(&mut self, enabled: bool);

    /// Get the list of available subtitle tracks
    fn subtitle_tracks(&mut self) -> Vec<SubtitleTrack>;

    /// Get the currently selected subtitle track index
    fn current_subtitle_track(&self) -> Option<i32>;

    /// Select a specific subtitle track by index, or None to disable subtitles
    fn select_subtitle_track(&mut self, track_index: Option<i32>) -> Result<(), Error>;

    /// Get the list of available audio tracks
    fn audio_tracks(&mut self) -> Vec<AudioTrack>;

    /// Get the currently selected audio track index
    fn current_audio_track(&self) -> i32;

    /// Select a specific audio track by index
    fn select_audio_track(&mut self, track_index: i32) -> Result<(), Error>;

    /// Check if the video has video tracks (not just audio)
    fn has_video(&self) -> bool;

    /// Get the underlying GStreamer pipeline.
    fn pipeline(&self) -> gst::Pipeline;
}
