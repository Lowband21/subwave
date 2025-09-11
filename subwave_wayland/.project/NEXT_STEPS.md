# Next Steps for Wayland Subsurface Implementation

## Summary of Current State
**BREAKTHROUGH**: The subsurface is working! We can see the red test buffer appear when iced redraws (on mouse events). The subsurface architecture is functional.

## Key Findings (2024-08-24)
1. **Subsurface IS visible**: Red test buffer confirmed visible at position (0,0) in top-left corner
2. **Buffer lifecycle working**: Buffer is received, displayed, and released correctly
3. **Iced only redraws on events**: Window updates only on mouse enter/leave, not continuously
4. **Buffer becomes empty after release**: This is normal Wayland behavior - surfaces don't retain content
5. **Architecture is correct**: The subsurface approach works as designed

## Next Steps Now That Subsurface Works

### 1. Connect GStreamer to the Subsurface
Now that we've proven the subsurface works, connect GStreamer:
- GStreamer's continuous buffer updates will keep the surface visible
- waylandsink will handle buffer management automatically
- No need for manual buffer re-attachment

### 2. Test with GStreamer Pipeline
Enable the GStreamer test pattern that's already in the code:
- Comment out the early return in `attach_test_pattern()` 
- GStreamer will continuously provide buffers
- This should keep the subsurface visible without mouse movement

### 3. Integration with Video Widget
- Move subsurface creation to widget initialization
- Update position based on widget layout
- Handle resize events properly

### 4. Production Implementation
- Remove test buffer code
- Connect actual video pipeline
- Handle playback controls
- Implement proper cleanup

## Potential Issues to Investigate

### 1. Buffer Attachment
The subsurface might need an initial buffer to become visible:
- Wayland surfaces are invisible until a buffer is attached
- waylandsink might not attach buffers until pipeline is PLAYING
- Check if `wl_surface_attach` is being called

### 2. Z-ordering
The subsurface is placed above parent with `place_above()`, but:
- Check if parent surface has content that might obscure subsurface
- Try `place_below()` to see if it makes a difference
- Verify with `WAYLAND_DEBUG=1` to see protocol messages

### 3. Synchronization Mode
Currently using `set_desync()` for independent updates:
- Try synchronized mode to see if it affects visibility
- Check if parent commits are happening after subsurface setup

### 4. Compositor Issues
Some compositors have quirks with subsurfaces:
- Test with different compositors (weston, sway, GNOME)
- Check compositor logs for any warnings
- Use `weston-info` or `wayland-info` to verify subsurface protocol

## Alternative Approaches if Subsurface Remains Invisible

### Option 1: Appsink + Texture Upload
Follow the approach used by `iced_video_player`:
- Use appsink to get frames
- Upload to wgpu texture
- Render within iced's pipeline
- Loses zero-copy benefit but proven to work

### Option 2: GStreamer GL Pipeline
Use GStreamer's GL elements with shared context:
- Create GL context shared with wgpu
- Use glsinkbin with shared context
- May preserve some performance benefits

### Option 3: DMABuf Sharing
If hardware decoder provides DMABuf:
- Import DMABuf into wgpu as external texture
- Avoids CPU copy while staying in iced pipeline
- Requires Linux-specific code

## Code Areas to Focus On

1. **subsurface_manager.rs:166-167**
   - After `set_position(0, 0)` and `commit()`
   - Add a test buffer attachment here

2. **test_subsurface.rs:99-100**
   - Between `set_size()` and `flush()`
   - Try adding explicit `damage()` call

3. **video_player.rs:206-220**
   - Position and size updates
   - Verify these are actually being called

## Debugging Commands

```bash
# Wayland protocol debugging
WAYLAND_DEBUG=1 cargo run --example simple_player 2>&1 | grep -E "wl_surface|wl_subsurface"

# GStreamer pipeline graph
GST_DEBUG_DUMP_DOT_DIR=/tmp cargo run --example simple_player
# Then convert with: dot -Tpng /tmp/*.dot -o pipeline.png

# Check if waylandsink is using our display
GST_DEBUG=waylandsink:7 cargo run --example simple_player 2>&1 | grep -i display
```

## Success Criteria
The implementation will be considered working when:
1. Test pattern appears within iced window boundaries
2. Pattern moves when window is moved
3. Pattern scales with widget size changes
4. No separate windows are created

## Contact Points
- GStreamer Discourse: https://discourse.gstreamer.org/
- Wayland Protocol Documentation: https://wayland.freedesktop.org/docs/html/
- Iced Discord: https://discord.gg/3xZJ65GAhd