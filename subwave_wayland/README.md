# subwave_wayland

Wayland subsurface-based video output for Iced with HDR passthrough support.

## Current Status: Alpha

**Video output is fully functional** - Dual subsurface implementation with background and video layers.

### What Works
- [x] Video playback via Wayland subsurfaces
- [x] Dual-layer subsurface architecture (background + video)
- [x] Integration with iced's rendering pipeline via thread-local storage
- [x] GStreamer pipeline with waylandsink
- [x] Wayland display context sharing
- [x] Pre-commit hook synchronization for position updates
- [x] Zero-copy video rendering with hardware acceleration

## Architecture
This crate implements video playback using Wayland subsurfaces, which allows:
- Zero-copy video rendering
- HDR passthrough without tone mapping
- Hardware-accelerated decoding directly to display

### Key Components

1. **WaylandVideoSubsurface** (`subsurface_manager.rs`)
   - Creates and manages Wayland subsurface
   - Handles positioning and synchronization with parent surface
   - Uses desynchronized mode for independent video updates

2. **VideoPlayer Widget** (`video_player.rs`)
   - Iced widget that reserves space in layout
   - Accesses WaylandIntegration via thread-local storage
   - Updates subsurface position based on widget bounds

3. **Pipeline** (`pipeline.rs`)
   - Dynamic pipeline creation
   - Direct rendering to Wayland surface via VideoOverlay interface

4. **WaylandIntegration** (in iced_ferrex)
   - Thread-local storage for Wayland handles
   - Pre-commit hooks for atomic updates
   - Set/cleared during iced's draw cycle

## Technical Implementation

### GStreamer Context Sharing
waylandsink requires a GstContext with the Wayland display handle:

```rust
// FFI workaround for Send trait constraint on pointers
unsafe {
    gst_ffi::gst_structure_set_value(
        structure.as_ptr() as *mut gst_ffi::GstStructure,
        b"display\0".as_ptr() as *const _,
        value.to_glib_none().0,
    );
}
```

### Surface Handle Format
```rust
pub fn surface_handle(&self) -> usize {
    self.video_surface.id().as_ptr() as usize
}
```

## Required Iced Fork Modifications

The iced fork at `git@github.com:Lowband21/iced-ferrex.git` has been modified to expose Wayland handles:

### iced_winit Changes
- Added thread-local storage for WaylandIntegration
- Set integration before widget draw
- Trigger pre-commit hooks before surface presentation

## Implementation Details

### Dual Subsurface Architecture
The implementation uses two subsurfaces for optimal rendering:
1. **Background Subsurface**: Provides a black background layer
2. **Video Subsurface**: Renders the actual video content on top

This approach ensures proper video display and prevents transparency issues, but will likely be made redundant by upstream gstreamer changes eventually.

### Next Steps
1. Subtitle management
  - Requires upstream gstreamer changes
2. Add content fit support through aspect ratio pipeline element
3. Playback speed integration

## Usage

```rust
use subwave_wayland::{init, VideoPlayer, SubsurfaceVideo};

// Initialize GStreamer first
init().expect("Failed to initialize GStreamer");

// Create video from URL
let url = url::Url::parse("file:///path/to/video.mkv").unwrap();
let video = SubsurfaceVideo::new(&url).expect("Failed to create video");

// Build an Iced widget to reserve layout space and drive updates
let player = VideoPlayer::new(&video)
    .width(iced::Length::Fill)
    .height(iced::Length::Fill);
```

Note: Keep the `SubsurfaceVideo` alive for the duration of playback.

## Testing

```bash
# Run example with debug output
RUST_LOG=debug cargo run --example simple_player

# With GStreamer debugging
GST_DEBUG=waylandsink:7 cargo run --example simple_player

# Check Wayland info
wayland-info | grep -A5 subsurface
```


## Dependencies

- Iced (using a fork that exposes Wayland handles)
- GStreamer 1.27.x developer build
- wayland-client 0.31
- Wayland compositor with subsurface support

Note: The Wayland backend is WGPU-only. Use Iced with the `wgpu` renderer and disable tiny-skia fallback in your application to ensure subsurface video integrates with the render loop correctly.

## License

MIT OR Apache-2.0
