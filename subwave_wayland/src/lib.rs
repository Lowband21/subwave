mod pipeline;
mod position;
pub mod subsurface_manager;
mod video;
mod video_player;
mod wayland_integration;
pub mod window;
pub mod gstplayflags;
pub mod internal;

pub use subsurface_manager::WaylandSubsurfaceManager;
pub use video::SubsurfaceVideo;
pub use video_player::VideoPlayer;
pub use wayland_integration::WaylandIntegration;
pub use subwave_core::Error;

pub type Result<T> = std::result::Result<T, subwave_core::Error>;

/// Initialize GStreamer. Must be called before creating any videos.
pub fn init() -> Result<()> {
    gstreamer::init().map_err(|e| Error::Pipeline(e.to_string()))?;
    Ok(())
}
