use gstreamer as gst;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct PendingState {
    pub paused: bool,
    pub position: Duration,
    pub speed: f64,
    pub volume: f64,
    pub muted: bool,
    pub audio_track: i32,
    pub subtitle_track: Option<i32>,
    pub subtitles_enabled: bool,
    pub subtitle_url: Option<url::Url>,
}

#[derive(Debug, Clone)]
pub struct VideoProperties {
    pub width: i32,
    pub height: i32,
    pub framerate: f64,
    pub has_video: bool,
}

/// Position in the media.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Position {
    /// Position based on time.
    ///
    /// Not the most accurate format for videos.
    Time(Duration),
    /// Position based on nth frame.
    Frame(u64),
}

/// Information about a subtitle track
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleTrack {
    /// The track index (0-based)
    pub index: i32,
    /// Language code (e.g., "en", "es", "fr")
    pub language: Option<String>,
    /// Human-readable title/name
    pub title: Option<String>,
    /// Codec used for the subtitle
    pub codec: Option<String>,
}

impl SubtitleTrack {
    /// Check if this subtitle track is text-based (not image-based)
    pub fn is_text_based(&self) -> bool {
        if let Some(ref codec) = self.codec {
            let codec_lower = codec.to_lowercase();
            // Common image-based subtitle formats
            let is_image_based = codec_lower.contains("pgs")
                || codec_lower.contains("hdmv")
                || codec_lower.contains("dvb")
                || codec_lower.contains("dvd")
                || codec_lower.contains("bluray")
                || codec_lower.contains("bitmap")
                || codec_lower.contains("vobsub");
            !is_image_based
        } else {
            // If no codec info, assume it might be text-based
            true
        }
    }
}

/// Information about an audio track
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrack {
    /// The track index (0-based)
    pub index: i32,
    /// Language code (e.g., "en", "es", "fr")
    pub language: Option<String>,
    /// Human-readable title/name
    pub title: Option<String>,
    /// Codec used for the audio (e.g., "AAC", "MP3", "AC3")
    pub codec: Option<String>,
    /// Number of channels (e.g., 2 for stereo, 6 for 5.1)
    pub channels: Option<i32>,
    /// Sample rate in Hz
    pub sample_rate: Option<i32>,
}

impl From<Position> for gst::GenericFormattedValue {
    fn from(pos: Position) -> Self {
        match pos {
            Position::Time(t) => gst::ClockTime::from_nseconds(t.as_nanos() as _).into(),
            Position::Frame(f) => gst::format::Default::from_u64(f).into(),
        }
    }
}

impl From<Duration> for Position {
    fn from(t: Duration) -> Self {
        Position::Time(t)
    }
}

impl From<u64> for Position {
    fn from(f: u64) -> Self {
        Position::Frame(f)
    }
}

// Display implementations for track selection in pick_list
impl std::fmt::Display for AudioTrack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(lang) = &self.language {
            write!(f, "{}", lang)?;
        } else if let Some(title) = &self.title {
            write!(f, "{}", title)?;
        } else {
            write!(f, "Track {}", self.index + 1)?;
        }
        Ok(())
    }
}

impl std::fmt::Display for SubtitleTrack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(lang) = &self.language {
            write!(f, "{}", lang)?;
        } else if let Some(title) = &self.title {
            write!(f, "{}", title)?;
        } else {
            write!(f, "Track {}", self.index + 1)?;
        }
        Ok(())
    }
}
