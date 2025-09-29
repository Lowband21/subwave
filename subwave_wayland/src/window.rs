//! Window integration helpers for getting Wayland handles

use crate::WaylandIntegration;

/// Get WaylandIntegration from the current window
///
/// This should be called after the window is created but before
/// creating VideoPlayer widgets. The integration is needed to
/// initialize Video objects with Wayland subsurfaces.
///
/// # Example
/// ```no_run
/// use subwave_wayland::{window, SubsurfaceVideo};
///
/// // In your application's update function, after window creation:
/// if let Some(integration) = window::get_wayland_integration() {
///     let video = SubsurfaceVideo::new(&uri)?;
///     video.init_wayland(&integration)?;
///     // Now the video can be used with VideoPlayer widget
/// }
/// ```
#[cfg(target_os = "linux")]
pub fn get_wayland_integration() -> Option<WaylandIntegration> {
    // Try to get from the current iced window context
    // This would need to be set by the iced runtime
    iced_winit::wayland_integration::wayland::with_current_wayland_integration(|integration| {
        // Convert from iced's internal type to our public type
        WaylandIntegration::new(integration.surface, integration.display)
    })
}

#[cfg(not(target_os = "linux"))]
pub fn get_wayland_integration() -> Option<WaylandIntegration> {
    None
}
