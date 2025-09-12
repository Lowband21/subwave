use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::{Element, Length};
use subwave_appsink::video::AppsinkVideo;
use subwave_core::types::PendingState;
use subwave_core::video::types::{AudioTrack, SubtitleTrack};
use subwave_core::video::video_trait::Video as VideoTrait;
use subwave_wayland::SubsurfaceVideo;

/// Which backend to use
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendPreference {
    Auto,
    ForceAppsink,
    ForceWayland,
}

/// Configuration for backend selection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubwaveConfig {
    pub preference: BackendPreference,
}

impl Default for SubwaveConfig {
    fn default() -> Self {
        Self {
            preference: BackendPreference::Auto,
        }
    }
}

/// Snapshot of playback state used for backend switching
#[derive(Debug, Clone)]
pub struct PlaybackState {
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

/// Environment-based backend selection
#[inline]
pub fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
}

/// A unified video wrapper over Appsink (generic) and Wayland (subsurface) backends.
///
/// This allows downstream applications to depend on a single type while using the
/// shared subwave_core trait and concrete implementations.
pub enum SubwaveVideo {
    Appsink {
        uri: url::Url,
        cfg: SubwaveConfig,
        inner: AppsinkVideo,
    },
    Wayland {
        uri: url::Url,
        cfg: SubwaveConfig,
        handle: Arc<Mutex<Option<Box<SubsurfaceVideo>>>>,
        // Pending state to apply after wayland pipeline is initialized
        pending: Arc<Mutex<Option<PlaybackState>>>,
    },
}

impl SubwaveVideo {
    /// Create a new unified video instance from a URL, selecting backend by config.
    pub fn new_with_config(
        uri: &url::Url,
        cfg: SubwaveConfig,
    ) -> Result<Self, subwave_core::Error> {
        let backend = match cfg.preference {
            BackendPreference::Auto => {
                if is_wayland() {
                    BackendPreference::ForceWayland
                } else {
                    BackendPreference::ForceAppsink
                }
            }
            other => other,
        };
        match backend {
            BackendPreference::ForceAppsink => {
                let v = AppsinkVideo::new(uri)?;
                Ok(SubwaveVideo::Appsink {
                    uri: uri.clone(),
                    cfg,
                    inner: v,
                })
            }
            BackendPreference::ForceWayland => {
                let v = SubsurfaceVideo::new(uri)?;
                Ok(SubwaveVideo::Wayland {
                    uri: uri.clone(),
                    cfg,
                    handle: Arc::new(Mutex::new(Some(Box::new(v)))),
                    pending: Arc::new(Mutex::new(None)),
                })
            }
            BackendPreference::Auto => unreachable!(),
        }
    }

    /// Create a new unified video with default config (Auto selection)
    pub fn new(uri: &url::Url) -> Result<Self, subwave_core::Error> {
        Self::new_with_config(uri, SubwaveConfig::default())
    }

    /// Playback control
    pub fn set_paused(&mut self, paused: bool) {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.set_paused(paused),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(mut g) = handle.lock()
                    && let Some(ref mut v) = g.as_mut() {
                        v.set_paused(paused);
                    }
            }
        }
    }

    pub fn paused(&self) -> bool {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.paused(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock()
                    && let Some(v) = g.as_ref() {
                        return v.paused();
                    }
                true
            }
        }
    }

    pub fn play(&mut self) {
        self.set_paused(false)
    }

    pub fn pause(&mut self) {
        self.set_paused(true)
    }

    pub fn set_speed(&mut self, speed: f64) -> Result<(), subwave_core::Error> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.set_speed(speed),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(mut g) = handle.lock()
                    && let Some(ref mut v) = g.as_mut() {
                        return v.set_speed(speed);
                    }
                Ok(())
            }
        }
    }

    pub fn position(&self) -> Duration {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.position(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock()
                    && let Some(v) = g.as_ref() {
                        return v.position();
                    }
                Duration::ZERO
            }
        }
    }

    pub fn duration(&self) -> Duration {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.duration(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock()
                    && let Some(v) = g.as_ref() {
                        return v.duration();
                    }
                Duration::ZERO
            }
        }
    }

    pub fn seek(&mut self, position: Duration, accurate: bool) -> Result<(), subwave_core::Error> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.seek(position, accurate),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(mut g) = handle.lock()
                    && let Some(ref mut v) = g.as_mut() {
                        return v.seek(position, accurate);
                    }
                Err(subwave_core::Error::InvalidState)
            }
        }
    }

    pub fn set_volume(&mut self, volume: f64) {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.set_volume(volume),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(mut g) = handle.lock()
                    && let Some(ref mut v) = g.as_mut() {
                        v.set_volume(volume);
                    }
            }
        }
    }

    pub fn volume(&self) -> f64 {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.volume(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock()
                    && let Some(v) = g.as_ref() {
                        return v.volume();
                    }
                1.0
            }
        }
    }

    pub fn set_muted(&mut self, muted: bool) {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.set_muted(muted),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(mut g) = handle.lock()
                    && let Some(ref mut v) = g.as_mut() {
                        v.set_muted(muted);
                    }
            }
        }
    }

    pub fn has_video(&self) -> bool {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.has_video(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock()
                    && let Some(v) = g.as_ref() {
                        return v.has_video();
                    }
                false
            }
        }
    }

    // Size
    pub fn size(&self) -> (i32, i32) {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.size(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock()
                    && let Some(v) = g.as_ref() {
                        return v.size();
                    }
                (0, 0)
            }
        }
    }

    // Tracks and subtitles
    pub fn audio_tracks(&mut self) -> Vec<AudioTrack> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.audio_tracks(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(mut g) = handle.lock()
                    && let Some(ref mut v) = g.as_mut() {
                        return v.audio_tracks();
                    }
                vec![]
            }
        }
    }

    pub fn current_audio_track(&self) -> i32 {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.current_audio_track(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock()
                    && let Some(v) = g.as_ref() {
                        return v.current_audio_track();
                    }
                0
            }
        }
    }

    pub fn select_audio_track(&mut self, index: i32) -> Result<(), subwave_core::Error> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.select_audio_track(index),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(mut g) = handle.lock()
                    && let Some(ref mut v) = g.as_mut() {
                        return v.select_audio_track(index);
                    }
                Err(subwave_core::Error::InvalidState)
            }
        }
    }

    pub fn subtitle_tracks(&mut self) -> Vec<SubtitleTrack> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.subtitle_tracks(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(mut g) = handle.lock()
                    && let Some(ref mut v) = g.as_mut() {
                        return v.subtitle_tracks();
                    }
                vec![]
            }
        }
    }

    pub fn current_subtitle_track(&self) -> Option<i32> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.current_subtitle_track(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock()
                    && let Some(v) = g.as_ref() {
                        return v.current_subtitle_track();
                    }
                None
            }
        }
    }

    pub fn select_subtitle_track(&mut self, index: Option<i32>) -> Result<(), subwave_core::Error> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.select_subtitle_track(index),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(mut g) = handle.lock()
                    && let Some(ref mut v) = g.as_mut() {
                        return v.select_subtitle_track(index);
                    }
                Err(subwave_core::Error::InvalidState)
            }
        }
    }

    pub fn subtitles_enabled(&self) -> bool {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.subtitles_enabled(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock()
                    && let Some(v) = g.as_ref() {
                        return v.subtitles_enabled();
                    }
                false
            }
        }
    }

    pub fn set_subtitles_enabled(&mut self, enabled: bool) {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.set_subtitles_enabled(enabled),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(mut g) = handle.lock()
                    && let Some(ref mut v) = g.as_mut() {
                        v.set_subtitles_enabled(enabled);
                    }
            }
        }
    }

    /// Convenience to construct a backend-agnostic video widget.
    pub fn widget<'a, Message, Theme>(
        &'a self,
        content_fit: iced::ContentFit,
        on_new_frame: Option<Message>,
    ) -> Element<'a, Message, Theme, iced_wgpu::Renderer>
    where
        Message: Clone + 'a,
        Theme: 'a,
    {
        match self {
            SubwaveVideo::Appsink { inner, .. } => {
                let mut w = subwave_appsink::video_player::VideoPlayer::new(inner)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(content_fit);
                if let Some(m) = on_new_frame.clone() {
                    w = w.on_new_frame(m);
                }
                w.into()
            }
            SubwaveVideo::Wayland {
                handle, pending, ..
            } => {
                // Attempt to apply any pending state if the pipeline is ready
                if let Ok(mut pending_guard) = pending.lock()
                    && let Some(state) = pending_guard.take() {
                        if let Ok(mut h) = handle.lock() {
                            if let Some(ref mut v) = h.as_mut() {
                                // Only apply if video has started producing frames
                                if v.has_video() {
                                    let _ = v.set_speed(state.speed);
                                    v.set_volume(state.volume);
                                    v.set_muted(state.muted);
                                    let _ = v.select_audio_track(state.audio_track);
                                    let _ = v.select_subtitle_track(state.subtitle_track);
                                    v.set_subtitles_enabled(state.subtitles_enabled);
                                    if let Some(url) = state.subtitle_url {
                                        let _ = v.set_subtitle_url(&url);
                                    }
                                    let _ = v.seek(state.position, false);
                                    v.set_paused(state.paused);
                                } else {
                                    // Not ready yet - put it back
                                    *pending_guard = Some(state);
                                }
                            } else {
                                // No inner video yet - put it back
                                *pending_guard = Some(state);
                            }
                        } else {
                            // Failed to lock - put it back
                            *pending_guard = Some(state);
                        }
                    }

                let mut w = subwave_wayland::VideoPlayer::new(handle)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(content_fit);
                if let Some(m) = on_new_frame.clone() {
                    w = w.on_new_frame(m);
                }
                w.into()
            }
        }
    }
    /// Return the configured backend preference
    pub fn config(&self) -> SubwaveConfig {
        match self {
            SubwaveVideo::Appsink { cfg, .. } => *cfg,
            SubwaveVideo::Wayland { cfg, .. } => *cfg,
        }
    }

    /// Return the media URL used to create this video
    pub fn uri(&self) -> &url::Url {
        match self {
            SubwaveVideo::Appsink { uri, .. } => uri,
            SubwaveVideo::Wayland { uri, .. } => uri,
        }
    }

    /// Identify the current backend
    pub fn backend(&self) -> BackendPreference {
        match self {
            SubwaveVideo::Appsink { .. } => BackendPreference::ForceAppsink,
            SubwaveVideo::Wayland { .. } => BackendPreference::ForceWayland,
        }
    }

    fn capture_state(&self) -> PlaybackState {
        let paused = self.paused();
        let position = self.position();
        let speed = match self {
            SubwaveVideo::Appsink { inner, .. } => inner.speed(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock() {
                    if let Some(v) = g.as_ref() {
                        v.speed()
                    } else {
                        1.0
                    }
                } else {
                    1.0
                }
            }
        };
        let volume = self.volume();
        let muted = match self {
            SubwaveVideo::Appsink { inner, .. } => inner.muted(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock() {
                    if let Some(v) = g.as_ref() {
                        v.muted()
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
        };
        let audio_track = self.current_audio_track();
        let subtitle_track = self.current_subtitle_track();
        let subtitles_enabled = self.subtitles_enabled();
        let subtitle_url = match self {
            SubwaveVideo::Appsink { inner, .. } => inner.subtitle_url(),
            SubwaveVideo::Wayland { handle, .. } => {
                if let Ok(g) = handle.lock() {
                    if let Some(v) = g.as_ref() {
                        v.subtitle_url()
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };
        PlaybackState {
            paused,
            position,
            speed,
            volume,
            muted,
            audio_track,
            subtitle_track,
            subtitles_enabled,
            subtitle_url,
        }
    }

    fn apply_state_to_appsink(inner: &mut AppsinkVideo, st: &PlaybackState) {
        // Pause before applying state to ensure seeks land correctly
        inner.set_paused(true);
        let _ = inner.select_audio_track(st.audio_track);
        let _ = inner.select_subtitle_track(st.subtitle_track);
        inner.set_subtitles_enabled(st.subtitles_enabled);
        if let Some(url) = &st.subtitle_url {
            let _ = inner.set_subtitle_url(url);
        }
        let _ = inner.seek(st.position, true);
        inner.set_volume(st.volume);
        inner.set_muted(st.muted);
        let _ = inner.set_speed(st.speed);
        inner.set_paused(st.paused);
    }

    /// Change backend preference and switch if needed (preserving playback state)
    pub fn set_preference(
        &mut self,
        preference: BackendPreference,
    ) -> Result<(), subwave_core::Error> {
        let uri = self.uri().clone();
        let current = self.backend();
        if (preference == BackendPreference::Auto
            && current
                == if is_wayland() {
                    BackendPreference::ForceWayland
                } else {
                    BackendPreference::ForceAppsink
                })
            || preference == current
        {
            // No change required
            // Still update config
            match self {
                SubwaveVideo::Appsink { cfg, .. } => cfg.preference = preference,
                SubwaveVideo::Wayland { cfg, .. } => cfg.preference = preference,
            }
            return Ok(());
        }
        // Capture state
        let st = self.capture_state();
        // Build new per preference
        match preference {
            BackendPreference::ForceAppsink => {
                let mut inner = AppsinkVideo::new(&uri)?;
                Self::apply_state_to_appsink(&mut inner, &st);
                *self = SubwaveVideo::Appsink {
                    uri,
                    cfg: SubwaveConfig { preference },
                    inner,
                };
                Ok(())
            }
            BackendPreference::ForceWayland => {
                let v = SubsurfaceVideo::new(&uri)?;
                // Queue state into Wayland video to apply after init
                v.queue_pending_state(PendingState {
                    paused: st.paused,
                    position: st.position,
                    speed: st.speed,
                    volume: st.volume,
                    muted: st.muted,
                    audio_track: st.audio_track,
                    subtitle_track: st.subtitle_track,
                    subtitles_enabled: st.subtitles_enabled,
                    subtitle_url: st.subtitle_url.clone(),
                });
                *self = SubwaveVideo::Wayland {
                    uri,
                    cfg: SubwaveConfig { preference },
                    handle: Arc::new(Mutex::new(Some(Box::new(v)))),
                    pending: Arc::new(Mutex::new(None)),
                };
                Ok(())
            }
            BackendPreference::Auto => {
                let pref = if is_wayland() {
                    BackendPreference::ForceWayland
                } else {
                    BackendPreference::ForceAppsink
                };
                self.set_preference(pref)
            }
        }
    }
}

impl std::fmt::Debug for SubwaveVideo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubwaveVideo::Appsink { .. } => f.debug_struct("SubwaveVideo::Appsink").finish(),
            SubwaveVideo::Wayland { .. } => f.debug_struct("SubwaveVideo::Wayland").finish(),
        }
    }
}
