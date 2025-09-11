# Wayland Video Player - Implementation TODOs

## Overview
This document tracks the remaining implementation work needed to achieve full feature parity between `iced_video_player_wayland` (Wayland subsurface implementation) and the standard `iced_video_player`.

## Integration Status
✅ **Completed:**
- Basic video playback through Wayland subsurfaces
- Play/Pause/Seek functionality
- Volume control
- Playback rate control
- VideoPlayer widget integration
- Conditional backend selection based on environment

## Missing Features - High Priority

### 1. Track Management
- [ ] **Audio Tracks**
  - [ ] `audio_tracks()` - Query available audio tracks
  - [ ] `current_audio_track()` - Get currently selected audio track
  - [ ] `set_audio_track(track_id)` - Switch audio tracks

- [ ] **Subtitle Tracks**
  - [ ] `subtitle_tracks()` - Query available subtitle tracks
  - [ ] `current_subtitle_track()` - Get currently selected subtitle track
  - [ ] `set_subtitle_track(track_id)` - Switch subtitle tracks
  - [ ] `subtitles_enabled()` - Check if subtitles are enabled
  - [ ] `set_subtitles_enabled(bool)` - Enable/disable subtitles

### 2. Video Properties
- [ ] `width()` - Get video width from pipeline
- [ ] `height()` - Get video height from pipeline
- [ ] `framerate()` - Get video framerate
- [ ] `aspect_ratio()` - Get video aspect ratio

### 3. Playback State
- [ ] `is_loading()` - Track loading state during pipeline setup
- [ ] `eos()` - Detect end-of-stream condition
- [ ] `volume()` - Get current volume level
- [ ] `is_muted()` - Check mute state
- [ ] `set_muted(bool)` - Proper mute implementation (not just volume = 0)

### 4. Event Callbacks
- [ ] **End of Stream** - Currently partially implemented, but likely broken
- [ ] **Error handling** - Propagate GStreamer errors to UI
- [ ] **Loading progress** - Report buffering percentage for network streams (not currently implemented for standard either so not a high priority)
- [ ] **State changes** - Notify when play/pause/stop state changes

## Missing Features - Medium Priority

### 5. Advanced Seeking
- [ ] Accurate seeking flag support (frame-accurate vs fast seeking)
- [ ] Seek to specific frame number
- [ ] Segment seeking for loop playback

### 6. Performance Features
- [ ] Buffer management for network streams (As needed based on network condition detection, otherwise buffers just add overhead)
- [ ] Preroll configuration
- [ ] Pipeline latency optimization
- [ ] CPU utilization optimization

### 7. HDR Support -- The one thing that's already implemented by GStreamer and was the main reason for using wayland subsurfaces, they understand HDR metadata from waylandsink

## Missing Features - Low Priority

### 8. Advanced Controls
- [ ] Playback direction (reverse playback)
- [ ] Step frame forward/backward
- [ ] Snapshot/screenshot capability
- [ ] Picture-in-Picture mode // Something uniquely enabled by wayland subsurfaces and certainly will be implemented in the future

### 9. Diagnostics
- [ ] Pipeline graph export
- [ ] Statistics (dropped frames, bitrate, etc.)
- [ ] Debug overlay

## Known Issues
- No form of functioning playback control
- Video dimensions not properly set based on video properties
- Relatively high cpu usage during playback
- Buffer must be kept in scope

### Current Limitations
1. **Subsurface Z-ordering** - This may not be an issue as long as we can render our controls on top of the wayland subsurface as a parent surface.
2. **Window resizing** - Subsurface needs proper resize handling
3. **Multi-window** - Subsurface management across multiple windows
4. **Wayland-only** - No X11 fallback in the same binary (handled by abstraction layer)

### Bugs to Fix
- [ ] Subsurface not properly destroyed on video close
- [ ] Position/size updates may lag behind widget layout
- [ ] Memory leak in pipeline recreation

## Implementation Notes

### Priority Order
1. **First**: Complete track management (audio/subtitle) as these are actively used
2. **Second**: Video properties for proper aspect ratio handling
3. **Third**: Playback state for UI feedback
4. **Fourth**: Event callbacks for user interaction

### Testing Requirements
- Test on native Wayland (Hyprland - Done, GNOME, KDE Plasma)
- Test on XWayland for compatibility
- Test with various video formats (H.264, H.265, VP9, AV1)
- Test with network streams (HTTP, RTSP)
- Test subsurface behavior with window operations (minimize, maximize, fullscreen)

### API Compatibility Notes
- Methods should match the standard `iced_video_player` API where possible
- Use `Result<T, Error>` for operations that can fail in Wayland
- Provide sensible defaults/fallbacks for unimplemented features
- Document any behavioral differences

## Development Workflow
1. Pick a feature from the high-priority list
2. Implement in `iced_video_player_wayland/src/video.rs`
3. Update the abstraction layer if needed
4. Test on Wayland desktop
5. Update this document

## Resources
- [Wayland Subsurface Protocol](https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_subsurface)
- [GStreamer Wayland Sink Documentation](https://gstreamer.freedesktop.org/documentation/waylandsink/)
- [Original Integration Guide](../iced_video_player_wayland/integration_guide.md)
