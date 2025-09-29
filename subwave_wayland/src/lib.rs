pub mod gstplayflags;
pub mod internal;
mod pipeline;
mod position;
pub mod subsurface_manager;
mod video;
mod video_player;
mod wayland_integration;
pub mod window;

pub use subsurface_manager::WaylandSubsurfaceManager;
pub use subwave_core::Error;
pub use video::SubsurfaceVideo;
pub use video_player::{VideoHandle, VideoPlayer};
pub use wayland_integration::WaylandIntegration;

pub type Result<T> = std::result::Result<T, subwave_core::Error>;

/// Initialize GStreamer. Must be called before creating any videos.
pub fn init() -> Result<()> {
    gstreamer::init().map_err(|e| Error::Pipeline(e.to_string()))?;
    Ok(())
}
