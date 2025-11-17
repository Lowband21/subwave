use std::{
    sync::{atomic::AtomicBool, Arc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use gstreamer::StreamCollection;
use std::sync::mpsc;
use subwave_core::{
    types::PendingState,
    video::types::{AudioTrack, SubtitleTrack},
};

use crate::{pipeline::SubsurfacePipeline, video::Cmd, WaylandSubsurfaceManager};

// Internal encapsulates all state and is only accessed behind the RwLock
pub(crate) struct Internal {
    // Identity
    pub(crate) uri: url::Url,

    // Core handles
    pub(crate) pipeline: Option<Arc<SubsurfacePipeline>>, // read-mostly; clone and drop lock before external calls
    pub(crate) subsurface: Option<Arc<WaylandSubsurfaceManager>>, // same

    pub(crate) duration: Option<Duration>,
    pub(crate) speed: f64,

    // Playback state flags for trait support
    pub(crate) looping: bool,
    pub(crate) is_eos: bool,
    pub(crate) restart_stream: bool,

    // Buffering state
    pub(crate) is_buffering: bool,
    pub(crate) buffering_percent: i32,
    pub(crate) user_paused: bool,

    // Bus thread control
    pub(crate) bus_thread: Option<JoinHandle<()>>,
    pub(crate) bus_stop: Arc<AtomicBool>,

    // Command receiver for bus->UI updates
    pub(crate) cmd_rx: Option<mpsc::Receiver<Cmd>>,

    // Track selection state
    pub(crate) stream_collection: Option<StreamCollection>,

    // Subtitle tracking
    pub(crate) available_subtitles: Vec<SubtitleTrack>,
    pub(crate) current_subtitle_track: Option<i32>,
    pub(crate) subtitles_enabled: bool,

    // Audio track tracking
    pub(crate) available_audio_tracks: Vec<AudioTrack>,
    pub(crate) current_audio_track: i32,

    pub(crate) audio_index_to_stream_id: Vec<String>,
    pub(crate) subtitle_index_to_stream_id: Vec<String>,

    pub(crate) selected_stream_ids: Vec<String>,

    // Pending playback state to apply when pipeline is ready
    pub(crate) pending_state: Option<PendingState>,

    // Pending HTTP headers to apply to pipeline when available
    pub(crate) pending_http_headers: Option<Vec<(String, String)>>,

    // Autoplay gating: when true, wait for seek completion (AsyncDone) before starting playback
    pub(crate) pending_play_after_seek: bool,
    pub(crate) pending_start_position: Option<Duration>,

    // Throttling
    pub(crate) last_position_update: Instant,
}
