# Changelog

## 2024-08-24: Major Progress on Wayland Subsurface Integration

### Added
- FFI-based GStreamer context creation to bypass Send trait constraints
- Proper Wayland display context sharing with waylandsink
- Thread-local storage in iced_winit for WaylandIntegration access

### Fixed
- ✅ "proxy already has listener" segfault - use ObjectId::from_ptr without ownership
- ✅ Circular dependency between iced_wgpu and iced_winit
- ✅ Pipeline state change errors - defer pause until after surface handle set
- ✅ Separate GStreamer window creation - waylandsink now uses our surface

### Changed
- Context structure field setting now uses gst_ffi::gst_structure_set_value directly
- Surface handle retrieval uses video_surface.id().as_ptr() instead of c_ptr()
- Test implementation uses prepare_window_handle() before set_window_handle()

### Known Issues
- Subsurface exists but is not visible in iced window
- GStreamer output not rendering to subsurface despite no errors
- Need to verify subsurface visibility with simple colored buffer test

### Technical Notes
- waylandsink requires GstContext with type "GstWaylandDisplayHandleContextType"
- Context structure needs "display" field with wl_display pointer
- Send trait prevents direct pointer storage in structures - use FFI workaround
- Pre-commit hooks work for position synchronization but subsurface still invisible

### Next Steps
1. Create minimal test with colored buffer (no GStreamer) to verify subsurface
2. Enable GST_DEBUG=waylandsink:7 for detailed pipeline debugging
3. Consider alternative approaches (appsink, GL pipeline) if subsurface remains invisible