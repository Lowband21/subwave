# Memory Leak Fixes for Iced Video Player

## Overview
This document describes critical memory leak fixes applied to the Rust video player that uses GStreamer and Wayland subsurfaces. The fixes address reference cycles, improper resource cleanup, and lifecycle management issues.

## Key Issues Identified

### 1. Pre-commit Hook Reference Cycles
**Problem**: The pre-commit hook closure in `subsurface_manager.rs` captured 11 Arc clones, creating strong reference cycles that prevented proper cleanup.

**Solution**: 
- Converted Arc clones to weak references (Arc::downgrade) for closure captures
- Added `clear_pre_commit_hooks()` method to WaylandIntegration
- Call clear method during subsurface cleanup to break cycles

### 2. GStreamer Bus Watch Leak
**Problem**: Bus watch was added but never removed, keeping callbacks and references alive indefinitely.

**Solution**:
- Store SourceId returned by `bus.add_watch()`
- Remove bus watch in `Video::stop()` using `watch_id.remove()`
- Ensures callbacks stop after pipeline stops

### 3. GStreamer Timeout Callback Leak  
**Problem**: Metadata extraction timeout was never cancelled, keeping references alive.

**Solution**:
- Store timeout SourceId
- Remove timeout in `Video::stop()` using `timeout_id.remove()`

### 4. GStreamer Signal Handler Leaks
**Problem**: Dynamic pad signal handlers (pad_added) were never disconnected.

**Solution**:
- Store signal handler IDs in Pipeline struct
- Disconnect all handlers in Drop implementation
- Clear elements HashMap to release references

### 5. Wayland Subsurface Cleanup Order
**Problem**: Incorrect cleanup order could cause use-after-free errors per Wayland documentation.

**Solution**:
- Clear pre-commit hooks first
- Unmap subsurfaces before destruction (attach NULL buffer)
- Destroy subsurfaces BEFORE parent surface
- Proper flush and synchronization

## Implementation Details

### Modified Files

1. **subsurface_manager.rs**
   - Changed Arc clones to weak references in pre-commit hook
   - Enhanced Drop implementation with proper cleanup order
   - Added pre-commit hook clearing

2. **wayland_integration.rs**
   - Added `clear_pre_commit_hooks()` method

3. **video.rs**
   - Added fields to store bus watch and timeout SourceIds
   - Enhanced `stop()` to remove watches and timeouts
   - Proper cleanup in Drop implementation

4. **pipeline.rs**
   - Added signal handler ID storage
   - Enhanced Drop to disconnect handlers
   - Clear element references

## Testing Recommendations

1. **Memory Monitoring**
   - Use `valgrind` with `--leak-check=full` flag
   - Monitor process memory usage over time with extended playback
   - Test multiple play/stop cycles

2. **Stress Testing**
   - Create and destroy multiple video instances
   - Rapid play/pause/stop sequences
   - Window resize during playback
   - Application shutdown during playback

3. **Debug Logging**
   - Added debug prints in Drop implementations
   - Monitor Arc strong/weak counts
   - Verify cleanup sequence order

## GStreamer Best Practices Applied

Based on GStreamer 0.24 documentation:
- Bus watches must be explicitly removed
- Signal handlers must be disconnected
- Pipeline state must be set to Null before cleanup
- Reference counting must be properly managed

## Wayland Best Practices Applied

Based on wayland-client 0.31 documentation:
- Subsurfaces must be destroyed before parent surfaces
- Unmapping should use NULL buffer attachment
- Proper event queue flushing during cleanup
- Immediate effect of subsurface destruction

## Verification

To verify the fixes:

```bash
# Build with debug symbols
cargo build

# Run with valgrind
valgrind --leak-check=full --show-leak-kinds=all ./target/debug/simple_player

# Monitor memory usage
watch -n 1 'ps aux | grep simple_player'
```

## Future Improvements

1. Add automated memory leak detection in CI
2. Implement resource usage metrics
3. Add debug assertions for lifecycle invariants
4. Consider using RAII patterns more extensively