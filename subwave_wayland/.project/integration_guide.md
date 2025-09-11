# Wayland Subsurface Video Integration Guide

## Overview
This guide documents the successful integration of GStreamer video playback through Wayland subsurfaces in iced applications. This approach provides zero-copy, hardware-accelerated video rendering that operates independently of iced's render pipeline.

## Architecture Summary

### Why Subsurfaces Work
1. **Complete Independence**: Video subsurface operates outside iced's render loop
2. **Zero-Copy Path**: GStreamer renders directly through waylandsink to the subsurface
3. **No wgpu Conflicts**: Avoids GPU resource sharing issues by bypassing iced's rendering
4. **Hardware Acceleration**: Can use DMABuf and hardware overlay planes when available

### Key Components
- **WaylandVideoSubsurface**: Manages the Wayland subsurface lifecycle
- **VideoPlayer Widget**: iced widget that reserves space and controls subsurface position/size
- **GStreamer Pipeline**: Handles video decoding and rendering to the subsurface

## Critical Implementation Details

### 1. Subsurface Lifecycle Management
**Problem**: Subsurface disappears when dropped
**Solution**: Store the subsurface in the widget to keep it alive
```rust
pub struct VideoPlayer<'a, Message, Theme> {
    // ... other fields ...
    test_subsurface: RefCell<Option<Arc<WaylandVideoSubsurface>>>,
}

// In draw() when creating subsurface:
*self.test_subsurface.borrow_mut() = Some(subsurface);
```

### 2. GStreamer waylandsink Configuration
**Problem**: "Window has no size set" error
**Solution**: Call `set_render_rectangle()` after `set_window_handle()`
```rust
unsafe {
    video_overlay.prepare_window_handle();
    video_overlay.set_window_handle(surface_handle);
    // CRITICAL: Must set render rectangle
    video_overlay.set_render_rectangle(0, 0, width, height);
}
```

### 3. Surface Handle Format
**Problem**: Segfault when passing wrong handle type
**Solution**: waylandsink expects raw `wl_surface` pointer, NOT `wl_egl_window`
```rust
pub fn surface_handle(&self) -> usize {
    // Return raw wl_surface pointer
    self.video_surface.id().as_ptr() as usize
}
```

### 4. Pipeline Configuration for Video Files
```rust
let pipeline_str = format!(
    "filesrc location=\"{}\" ! parsebin ! decodebin3 ! \
     videoconvertscale n-threads=0 ! waylandsink name=testsink",
    video_path
);
```

## Integration Steps for ferrix-player

### Step 1: Replace iced_video_player with iced_video_player_wayland

In `ferrix-player/Cargo.toml`:
```toml
[dependencies]
# Replace: iced_video_player = { path = "../iced_video_player" }
iced_video_player_wayland = { path = "../iced_video_player_wayland" }
```

### Step 2: Update VideoPlayer Widget Usage

The widget API remains largely the same:
```rust
use iced_video_player_wayland::{Video, VideoPlayer};

// In your view function:
VideoPlayer::new(&video)
    .width(Length::Fill)
    .height(Length::Fill)
    .content_fit(ContentFit::Cover)
    .on_end_of_stream(Message::VideoEnded)
```

### Step 3: Video Creation and Control

```rust
// Create video from URI
let video = Video::new(&uri)?;

// Control playback
video.play()?;
video.pause()?;
video.seek(position)?;
video.set_volume(0.8)?;
```

### Step 4: Handle Wayland Integration

The VideoPlayer widget automatically handles Wayland integration on first draw after frame 5 (to ensure parent surface is ready). No manual initialization needed.

## Dynamic Sizing and Positioning

The subsurface automatically updates position and size based on:
1. Widget layout bounds
2. ContentFit mode (Cover, Contain, Fill, etc.)
3. Video aspect ratio

```rust
// In VideoPlayer::draw()
if let Some(ref subsurface) = *self.test_subsurface.borrow() {
    subsurface.set_position(window_x, window_y);
    subsurface.set_size(fitted.width as u32, fitted.height as u32);
}
```

## Abstraction Requirements for Production

To make this a true drop-in replacement, the crate should abstract:

### 1. Subsurface Management
- Move subsurface creation/storage into Video struct
- Hide Wayland-specific details from widget

### 2. Pipeline Configuration
- Support various source types (file, HTTP, RTSP, etc.)
- Handle format negotiation automatically
- Implement proper error recovery

### 3. Synchronization
- Handle seek operations with subsurface updates
- Coordinate playback state with UI updates
- Manage buffer presentation timing

### 4. Event Handling
- End-of-stream notifications
- Error reporting
- Playback state changes

## Example Integration Pattern

```rust
pub struct MediaPlayer {
    video: Option<Video>,
    controls_visible: bool,
    // ... other UI state ...
}

impl MediaPlayer {
    fn view(&self) -> Element<Message> {
        let video_widget = if let Some(ref video) = self.video {
            VideoPlayer::new(video)
                .width(Length::Fill)
                .height(Length::Fill)
                .content_fit(self.content_fit_mode)
                .on_end_of_stream(Message::VideoEnded)
        } else {
            // Placeholder or loading state
        };
        
        // Overlay controls on top (in iced's render layer)
        Stack::new()
            .push(video_widget)
            .push(self.render_controls())
    }
}
```

## Performance Considerations

1. **Subsurface Desync Mode**: Ensures video updates don't block UI
2. **Hardware Decoding**: Use `vaapidecodebin` or `nvdec` when available
3. **Buffer Management**: waylandsink handles double/triple buffering
4. **Damage Tracking**: Only damaged regions are recomposed

## Known Limitations

1. **Wayland Only**: This approach requires Wayland (no X11 fallback)
2. **Single Surface**: One subsurface per widget instance
3. **Z-Order**: Subsurface is always above parent surface content

## Testing Checklist

- [x] Video file playback from command line
- [x] Test pattern fallback when no file specified  
- [x] Dynamic resize with ContentFit modes
- [x] Position updates when widget moves
- [x] Subsurface persists during playback
- [ ] Seek operations update subsurface correctly
- [ ] Multiple video widgets in same window
- [ ] Proper cleanup on video change/destruction

## Future Enhancements

1. **Multiple Backend Support**: Add fallback to appsink + texture upload for X11
2. **HDR Support**: Leverage Wayland's HDR capabilities
3. **Overlay Plane Optimization**: Hint for hardware overlay usage
4. **Picture-in-Picture**: Detachable subsurfaces for PiP mode

## Debugging Tips

### Enable GStreamer Debug Output
```bash
GST_DEBUG=waylandsink:7 cargo run
```

### Monitor Wayland Protocol
```bash
WAYLAND_DEBUG=1 cargo run 2>&1 | grep -E "wl_surface|wl_subsurface"
```

### Check Pipeline State
The test implementation logs pipeline state and bus messages for debugging.

## Conclusion

The Wayland subsurface approach successfully provides hardware-accelerated video playback in iced applications. With proper subsurface lifecycle management and correct waylandsink configuration, this can be a drop-in replacement for the existing iced_video_player, offering superior performance and lower CPU usage.

For ferrix-player integration, the main considerations are:
1. Ensuring the subsurface is kept alive throughout playback
2. Properly updating position/size based on layout
3. Coordinating playback controls with the subsurface state

The crate should evolve to hide more implementation details and provide a cleaner abstraction for production use.