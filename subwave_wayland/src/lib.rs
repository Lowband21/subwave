use thiserror::Error;

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

#[derive(Error, Debug)]
pub enum Error {
    #[error("GStreamer error: {0}")]
    GStreamer(#[from] gstreamer::glib::Error),

    #[error("Wayland error: {0}")]
    Wayland(String),

    #[error("Not running on Wayland")]
    NotWayland,

    #[error("Failed to create subsurface: {0}")]
    SubsurfaceCreation(String),

    #[error("Pipeline error: {0}")]
    Pipeline(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Initialize GStreamer. Must be called before creating any videos.
pub fn init() -> Result<()> {
    gstreamer::init().map_err(|e| Error::Pipeline(e.to_string()))?;
    Ok(())
}
