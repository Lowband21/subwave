#[cfg(target_os = "linux")]
pub mod gstplayflags;
#[cfg(target_os = "linux")]
pub mod internal;
#[cfg(target_os = "linux")]
mod pipeline;
#[cfg(target_os = "linux")]
mod position;
#[cfg(target_os = "linux")]
pub mod subsurface_manager;
#[cfg(target_os = "linux")]
mod video;
#[cfg(target_os = "linux")]
mod video_player;
#[cfg(target_os = "linux")]
mod wayland_integration;
#[cfg(target_os = "linux")]
pub mod window;

#[cfg(target_os = "linux")]
pub use subsurface_manager::WaylandSubsurfaceManager;
#[cfg(target_os = "linux")]
pub use subwave_core::Error;
#[cfg(target_os = "linux")]
pub use video::SubsurfaceVideo;
#[cfg(target_os = "linux")]
pub use video_player::{VideoHandle, VideoPlayer};
#[cfg(target_os = "linux")]
pub use wayland_integration::WaylandIntegration;

#[cfg(target_os = "linux")]
pub type Result<T> = std::result::Result<T, subwave_core::Error>;

/// Initialize GStreamer. Must be called before creating any videos.
#[cfg(target_os = "linux")]
pub fn init() -> Result<()> {
    gstreamer::init().map_err(|e| Error::Pipeline(e.to_string()))?;
    Ok(())
}
