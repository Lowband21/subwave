# Wayland Subsurface Video Component Implementation Plan

## Overview
Create a separate crate/module that renders video through Wayland subsurfaces, bypassing iced's wgpu rendering pipeline entirely. This enables true HDR passthrough from GStreamer to waylandsink without tone mapping or format conversion.

## Architecture Summary

### 1. Module Structure
```
iced_video_player_wayland/
├── Cargo.toml
├── src/
│   ├── lib.rs                  # Public API matching iced_video_player
│   ├── subsurface_manager.rs   # Wayland subsurface lifecycle management
│   ├── wayland_integration.rs  # Platform handle extraction
│   ├── widget.rs               # Placeholder widget for iced
│   ├── pipeline.rs             # GStreamer pipeline with waylandsink
│   └── synchronization.rs      # Frame/commit synchronization
```

### 2. Key Components

#### A. Minimal iced Fork Modifications
- **Location**: `iced_fork/winit/src/window/state.rs` (or similar)
- **Changes**:
  1. Expose Wayland surface/display handles
  2. Add pre-commit hook mechanism
  3. Provide viewport/scroll offset access

#### B. Subsurface Manager
- Creates and manages Wayland subsurfaces independently
- Handles position/size updates synchronized with parent commits
- Manages cleanup in correct order (subsurface before parent)

#### C. Placeholder Widget
- Reserves space in iced's layout system
- Tracks position changes for subsurface repositioning
- Doesn't render anything (subsurface handles display)

#### D. GStreamer Integration
- Direct waylandsink connection to subsurface
- HDR format preservation (P010_10LE, P012_LE, P016_LE)
- Zero-copy DMABuf path when available

## Implementation Steps

### Phase 1: iced Fork Modifications
1. Add `WaylandIntegration` struct to expose handles
2. Implement `wayland_integration()` method on Application
3. Add `register_pre_commit_hook()` for synchronization
4. Test handle extraction on Wayland

### Phase 2: Create Wayland Subsurface Module
1. Set up new crate with wayland-client dependencies
2. Implement `WaylandVideoSubsurface` struct
3. Create subsurface from parent handle
4. Set desynchronized mode for independent updates
5. Implement position/size update methods

### Phase 3: Widget Implementation
1. Create `VideoWidget` matching iced_video_player API
2. Implement layout reservation logic
3. Track position in window coordinates
4. Update subsurface position on draw

### Phase 4: GStreamer Pipeline Integration
1. Build pipeline with waylandsink element
2. Configure for HDR format preservation:
   - Remove tone mapping elements
   - Accept P010_10LE/P012_LE/P016_LE formats
   - Enable DMABuf memory when available
3. Connect waylandsink to subsurface via VideoOverlay
4. Handle bus messages for state changes

### Phase 5: Synchronization & Cleanup
1. Implement pre-commit hook for position sync
2. Handle damage tracking independently
3. Ensure proper cleanup order in Drop implementations
4. Test with scrolling/layout changes

### Phase 6: Testing & Optimization
1. Verify HDR passthrough with test content
2. Check zero-copy path with hardware decoders
3. Test fallback to software rendering
4. Validate subsurface z-ordering

## Critical Requirements

### HDR Passthrough
- **NO tone mapping** in the pipeline
- Preserve original bit depth (10/12/16-bit)
- Support HDR10, HLG, Dolby Vision metadata
- Direct path from decoder to display

### Performance
- Zero-copy DMABuf when possible
- Desynchronized updates for smooth playback
- Minimal CPU involvement in video path
- Hardware overlay plane usage when available

### Compatibility
- Fallback to existing wgpu path on non-Wayland
- API compatibility with current iced_video_player
- Graceful degradation if subsurface creation fails

## Dependencies
```toml
[dependencies]
wayland-client = "0.31"
wayland-protocols = { version = "0.31", features = ["client"] }
raw-window-handle = "0.6"
gstreamer = "0.24"
gstreamer-video = "0.24"
```

## Success Criteria
1. Video renders through Wayland subsurface, not wgpu
2. HDR content displays without tone mapping
3. Performance matches or exceeds current implementation
4. API remains compatible with existing code
5. Clean integration with iced's layout system

## Detailed Implementation Notes

### Wayland Handle Extraction
The iced fork needs to expose raw Wayland handles from winit. This requires:
```rust
use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

pub struct WaylandIntegration {
    pub surface: *mut wl_surface::WlSurface,
    pub display: *mut wl_display::WlDisplay,
    pub commit_callback: Arc<dyn Fn() + Send + Sync>,
}
```

### Subsurface Lifecycle
1. **Creation**: After window is created, extract handles and create subsurface
2. **Updates**: Position/size changes sync with parent on commit
3. **Destruction**: Must destroy subsurface before parent surface

### GStreamer Pipeline Configuration
For HDR passthrough, the pipeline should be minimal:
```
playbin ! videoconvertscale n-threads=0 ! waylandsink
```

With caps negotiation allowing HDR formats:
```
caps="video/x-raw,format=(string){NV12,P010_10LE,P012_LE,P016_LE},pixel-aspect-ratio=1/1"
```

### Synchronization Strategy
- Subsurface operates in desynchronized mode for independent frame updates
- Position/size changes still require parent commit synchronization
- Use atomic operations for thread-safe state management

### Error Handling
- Graceful fallback if Wayland subsurface creation fails
- Detection of non-Wayland platforms with compile-time conditionals
- Runtime checks for waylandsink availability