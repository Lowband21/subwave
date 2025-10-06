use gstreamer::Pipeline;
use iced::{Element, Length};
use log::warn;
use std::time::Duration;
use subwave_appsink::video::AppsinkVideo;
use subwave_core::video::types::{AudioTrack, SubtitleTrack};
use subwave_core::video::video_trait::Video as VideoTrait;

#[cfg(all(feature = "wayland", target_os = "linux"))]
use std::cell::RefCell;
#[cfg(all(feature = "wayland", target_os = "linux"))]
use std::rc::Rc;
#[cfg(all(feature = "wayland", target_os = "linux"))]
use std::sync::{Arc, Mutex};
#[cfg(all(feature = "wayland", target_os = "linux"))]
use subwave_core::types::PendingState;
#[cfg(all(feature = "wayland", target_os = "linux"))]
use subwave_wayland::{SubsurfaceVideo, VideoHandle};

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
        inner: Box<AppsinkVideo>,
    },
    #[cfg(all(feature = "wayland", target_os = "linux"))]
    Wayland {
        uri: url::Url,
        cfg: SubwaveConfig,
        handle: VideoHandle,
        // Pending state to apply after wayland pipeline is initialized
        pending: Arc<Mutex<Option<PlaybackState>>>,
    },
}

impl SubwaveVideo {
    #[cfg(all(feature = "wayland", target_os = "linux"))]
    fn with_wayland<R>(&self, f: impl FnOnce(&SubsurfaceVideo) -> R) -> Option<R> {
        match self {
            SubwaveVideo::Wayland { handle, .. } => handle
                .try_borrow()
                .ok()
                .and_then(|guard| guard.as_ref().map(|video| f(video.as_ref()))),
            _ => None,
        }
    }

    #[cfg(all(feature = "wayland", target_os = "linux"))]
    fn with_wayland_mut<R>(&mut self, f: impl FnOnce(&mut SubsurfaceVideo) -> R) -> Option<R> {
        match self {
            SubwaveVideo::Wayland { handle, .. } => handle
                .try_borrow_mut()
                .ok()
                .and_then(|mut guard| guard.as_mut().map(|video| f(video.as_mut()))),
            _ => None,
        }
    }

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
                    inner: Box::new(v),
                })
            }
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            BackendPreference::ForceWayland => {
                let v = SubsurfaceVideo::new(uri)?;
                Ok(SubwaveVideo::Wayland {
                    uri: uri.clone(),
                    cfg,
                    handle: Rc::new(RefCell::new(Some(Box::new(v)))),
                    pending: Arc::new(Mutex::new(None)),
                })
            }
            #[cfg(not(all(feature = "wayland", target_os = "linux")))]
            BackendPreference::ForceWayland => {
                warn!("Wayland backend requested on non-Linux platform; falling back to Appsink");
                let v = AppsinkVideo::new(uri)?;
                Ok(SubwaveVideo::Appsink {
                    uri: uri.clone(),
                    cfg: SubwaveConfig {
                        preference: BackendPreference::ForceAppsink,
                    },
                    inner: Box::new(v),
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
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => {
                self.with_wayland_mut(|video| video.set_paused(paused));
            }
        }
    }

    pub fn paused(&self) -> bool {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.paused(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => {
                self.with_wayland(|video| video.paused()).unwrap_or(true)
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
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland_mut(|video| video.set_speed(speed))
                .unwrap_or(Ok(())),
        }
    }

    pub fn position(&self) -> Duration {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.position(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland(|video| video.position())
                .unwrap_or(Duration::ZERO),
        }
    }

    pub fn duration(&self) -> Duration {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.duration(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland(|video| video.duration())
                .unwrap_or(Duration::ZERO),
        }
    }

    pub fn seek(&mut self, position: Duration, accurate: bool) -> Result<(), subwave_core::Error> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.seek(position, accurate),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland_mut(|video| video.seek(position, accurate))
                .unwrap_or(Err(subwave_core::Error::InvalidState)),
        }
    }

    pub fn set_volume(&mut self, volume: f64) {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.set_volume(volume),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => {
                if let Some(Err(err)) =
                    self.with_wayland_mut(|video| SubsurfaceVideo::set_volume(video, volume))
                {
                    warn!("Failed to set Wayland volume: {err}");
                }
            }
        }
    }

    pub fn volume(&self) -> f64 {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.volume(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => {
                self.with_wayland(|video| video.volume()).unwrap_or(1.0)
            }
        }
    }

    pub fn set_muted(&mut self, muted: bool) {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.set_muted(muted),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => {
                self.with_wayland_mut(|video| video.set_muted(muted));
            }
        }
    }

    pub fn has_video(&self) -> bool {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.has_video(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland(|video| video.has_video())
                .unwrap_or(false),
        }
    }

    // Size
    pub fn size(&self) -> (i32, i32) {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.size(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => {
                self.with_wayland(|video| video.size()).unwrap_or((0, 0))
            }
        }
    }

    // Tracks and subtitles
    pub fn audio_tracks(&mut self) -> Vec<AudioTrack> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.audio_tracks(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland_mut(|video| video.audio_tracks())
                .unwrap_or_default(),
        }
    }

    pub fn current_audio_track(&self) -> i32 {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.current_audio_track(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland(|video| video.current_audio_track())
                .unwrap_or(0),
        }
    }

    pub fn select_audio_track(&mut self, index: i32) -> Result<(), subwave_core::Error> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.select_audio_track(index),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland_mut(|video| video.select_audio_track(index))
                .unwrap_or(Err(subwave_core::Error::InvalidState)),
        }
    }

    pub fn subtitle_tracks(&mut self) -> Vec<SubtitleTrack> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.subtitle_tracks(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland_mut(|video| video.subtitle_tracks())
                .unwrap_or_default(),
        }
    }

    pub fn current_subtitle_track(&self) -> Option<i32> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.current_subtitle_track(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland(|video| video.current_subtitle_track())
                .unwrap_or(None),
        }
    }

    pub fn select_subtitle_track(&mut self, index: Option<i32>) -> Result<(), subwave_core::Error> {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.select_subtitle_track(index),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland_mut(|video| video.select_subtitle_track(index))
                .unwrap_or(Err(subwave_core::Error::InvalidState)),
        }
    }

    pub fn subtitles_enabled(&self) -> bool {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.subtitles_enabled(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland(|video| video.subtitles_enabled())
                .unwrap_or(false),
        }
    }

    pub fn set_subtitles_enabled(&mut self, enabled: bool) {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.set_subtitles_enabled(enabled),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => {
                if let Some(Err(err)) = self.with_wayland_mut(|video| {
                    SubsurfaceVideo::set_subtitles_enabled(video, enabled)
                }) {
                    warn!("Failed to toggle Wayland subtitles: {err}");
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
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland {
                handle, pending, ..
            } => {
                // Attempt to apply any pending state if the pipeline is ready
                if let Ok(mut pending_guard) = pending.lock()
                    && let Some(state) = pending_guard.take()
                {
                    let mut requeue = true;
                    if let Ok(mut guard) = handle.try_borrow_mut() {
                        match guard.as_deref_mut() {
                            Some(video) if video.has_video() => {
                                let _ = video.set_speed(state.speed);
                                if let Err(err) = SubsurfaceVideo::set_volume(video, state.volume) {
                                    warn!(
                                        "Failed to restore Wayland volume during pending state apply: {err}"
                                    );
                                }
                                video.set_muted(state.muted);
                                let _ = video.select_audio_track(state.audio_track);
                                let target_sub = if state.subtitles_enabled {
                                    state.subtitle_track
                                } else {
                                    None
                                };
                                let _ = video.select_subtitle_track(target_sub);
                                if let Some(ref url) = state.subtitle_url {
                                    let _ = video.set_subtitle_url(url);
                                }
                                let _ = video.seek(state.position, false);
                                video.set_paused(state.paused);
                                requeue = false;
                            }
                            _ => {}
                        }
                    }

                    if requeue {
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
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { cfg, .. } => *cfg,
        }
    }

    /// Return the media URL used to create this video
    pub fn uri(&self) -> &url::Url {
        match self {
            SubwaveVideo::Appsink { uri, .. } => uri,
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { uri, .. } => uri,
        }
    }

    /// Identify the current backend
    pub fn backend(&self) -> BackendPreference {
        match self {
            SubwaveVideo::Appsink { .. } => BackendPreference::ForceAppsink,
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => BackendPreference::ForceWayland,
        }
    }

    fn capture_state(&self) -> PlaybackState {
        let paused = self.paused();
        let position = self.position();
        let speed = match self {
            SubwaveVideo::Appsink { inner, .. } => inner.speed(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self.with_wayland(|video| video.speed()).unwrap_or(1.0),
        };
        let volume = self.volume();
        let muted = match self {
            SubwaveVideo::Appsink { inner, .. } => inner.muted(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => {
                self.with_wayland(|video| video.muted()).unwrap_or(false)
            }
        };
        let audio_track = self.current_audio_track();
        let subtitle_track = self.current_subtitle_track();
        let subtitles_enabled = self.subtitles_enabled();
        let subtitle_url = match self {
            SubwaveVideo::Appsink { inner, .. } => inner.subtitle_url(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland(|video| video.subtitle_url())
                .unwrap_or(None),
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
        let target_sub = if st.subtitles_enabled {
            st.subtitle_track
        } else {
            None
        };
        let _ = inner.select_subtitle_track(target_sub);
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
                #[cfg(all(feature = "wayland", target_os = "linux"))]
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
                    inner: Box::new(inner),
                };
                Ok(())
            }
            #[cfg(all(feature = "wayland", target_os = "linux"))]
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
                    handle: Rc::new(RefCell::new(Some(Box::new(v)))),
                    pending: Arc::new(Mutex::new(None)),
                };
                Ok(())
            }
            #[cfg(not(all(feature = "wayland", target_os = "linux")))]
            BackendPreference::ForceWayland => {
                warn!("Wayland backend requested on non-Linux platform; staying on Appsink");
                // Ensure the stored preference matches actual backend
                match self {
                    SubwaveVideo::Appsink { cfg, .. } => {
                        cfg.preference = BackendPreference::ForceAppsink;
                    }
                }
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

impl SubwaveVideo {
    /// Expose underlying GStreamer pipeline for clock/base-time adoption or diagnostics
    pub fn pipeline(&self) -> Pipeline {
        match self {
            SubwaveVideo::Appsink { inner, .. } => inner.pipeline(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => self
                .with_wayland(|video| video.pipeline())
                .unwrap_or_default(),
        }
    }
}

impl std::fmt::Debug for SubwaveVideo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubwaveVideo::Appsink { .. } => f.debug_struct("SubwaveVideo::Appsink").finish(),
            #[cfg(all(feature = "wayland", target_os = "linux"))]
            SubwaveVideo::Wayland { .. } => f.debug_struct("SubwaveVideo::Wayland").finish(),
        }
    }
}
