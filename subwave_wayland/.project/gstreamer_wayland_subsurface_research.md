# Implementing a Wayland-specific iced video player crate

Creating a Wayland-specific video player crate that maintains compatibility with iced_video_player's API while leveraging native Wayland subsurfaces presents significant technical challenges but offers potential performance benefits. This comprehensive analysis examines the architectural requirements, implementation details, and critical integration points needed for such a system.

## Current iced_video_player architecture reveals key design patterns

The existing iced_video_player implementation provides important insights into video playback within iced's constraints. **The crate bypasses iced's standard Image widget entirely**, instead implementing a custom wgpu render pipeline that copies frame data directly to GPU textures. This design choice reflects a fundamental limitation: iced's Image primitive isn't optimized for rapidly changing video content.

The current architecture implements video playback as a "composable component" rather than a true iced Widget, primarily because widgets don't support subscriptions needed for real-time updates. The video player uses GStreamer for decoding with hardware acceleration support where available, performing YUV to RGB conversion on the GPU during rendering. This approach achieves reasonable performance but still requires copying frame data through wgpu's texture upload pipeline.

The implementation exposes a straightforward API where applications create a `Video` object and render it through a `VideoPlayer` component. However, this design doesn't leverage platform-specific optimizations like Wayland's hardware overlay planes, which could eliminate the need for GPU composition entirely for video content.

## Creating custom iced widgets requires careful lifecycle management

Custom widgets in iced must implement the `Widget` trait with several critical methods. The `layout()` method computes widget dimensions based on constraints, while `draw()` performs the actual rendering through the provided renderer. For a video widget, the **`on_event()` method becomes crucial for handling playback controls** and the `state()` method manages video-specific state like current playback position.

The challenge for native surface integration lies in iced's rendering model. The framework expects widgets to render through its wgpu-based renderer, which maintains exclusive control over the render pass structure. Custom widgets can theoretically access wgpu resources through the renderer, but the compositor's device and instance aren't exposed as public APIs. This architectural decision prevents widgets from creating their own surfaces or managing external GPU resources directly.

Recent developments in iced show exploration of native platform window child creation for embedding custom rendering contexts. However, these remain experimental and don't yet provide stable APIs for production use. The widget lifecycle assumes widgets are purely visual elements without their own GPU resources, making subsurface integration architecturally complex.

## Extracting Wayland handles from winit requires platform-specific APIs

Accessing native Wayland handles from iced's winit-managed windows involves several abstraction layers. The raw-window-handle crate provides the primary interface:

```rust
use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

match window.raw_window_handle() {
    RawWindowHandle::Wayland(handle) => {
        let wl_surface = handle.surface as *mut wl_surface::WlSurface;
        let wl_display = handle.display as *mut wl_display::WlDisplay;
    }
    _ => panic!("Not running on Wayland"),
}
```

These raw pointers must be carefully managed to ensure safety. **The parent surface lifetime is controlled by winit**, so any subsurfaces created must not outlive the main window. Platform-specific extensions in winit 0.29+ provide additional access to XDG toplevel handles, though these APIs remain unstable.

The fundamental issue is that winit abstracts away Wayland-specific details by design, making it difficult to perform platform-specific operations without breaking the abstraction. The event loop ownership model particularly complicates subsurface management, as winit's `run_app()` method never returns, preventing external event source coordination.

## Wayland subsurface creation offers two implementation approaches

Creating Wayland subsurfaces for video content can be accomplished through either smithay-client-toolkit (SCTK) or direct wayland-client protocol access. SCTK provides higher-level abstractions:

```rust
let video_surface = compositor_state.create_surface(&qh);
let subsurface = subcompositor_state.get_subsurface(
    &video_surface,
    &parent_surface,
    &qh
);
subsurface.set_position(100, 100);
subsurface.set_desync(); // Independent updates for smooth video
```

The **desynchronized mode is essential for video playback**, allowing the video subsurface to update independently from the parent UI surface. This prevents UI updates from blocking video frame presentation and vice versa. Synchronized mode would cache all video updates until the parent commits, causing unacceptable latency.

Direct wayland-client access provides more control but requires manual protocol handling. Subsurfaces maintain their own coordinate system relative to the parent, support negative positioning, and aren't automatically clipped to parent bounds. The z-order can be manipulated through `place_above()` and `place_below()` operations, enabling overlay UI elements.

## wgpu rendering pipeline integration faces fundamental constraints

iced's wgpu rendering pipeline presents several challenges for subsurface integration. The renderer uses a standard render pass structure supporting text (via glyphon), quads with rounded borders, clip areas, images, and triangle meshes. **Custom primitives must fit within this existing pipeline structure**, limiting flexibility for external surface management.

The most significant limitation is that wgpu's device and instance aren't exposed as public APIs. Without device access, widgets cannot create their own textures or import external buffers like DMABufs from video decoders. The surface presentation model assumes exclusive control, with `queue.submit()` and `surface.present()` calls that don't naturally accommodate external surface updates.

Format compatibility poses another challenge. Native video surfaces typically use YUV formats requiring GPU conversion, while iced's pipeline centers on RGBA formats. Hardware overlay planes that could bypass GPU composition entirely often only support YUV, creating a fundamental mismatch with iced's rendering assumptions.

The Arc<Mutex<>> pattern that might enable resource sharing raises performance concerns, particularly for high-frequency operations like video frame updates. The synchronization overhead could negate benefits from using native surfaces.

## Event loop coordination requires architectural workarounds

Integrating iced's event loop with external surface management faces fundamental architectural impediments. **winit's event loop consumes itself when started**, using the `ApplicationHandler` trait pattern that makes injecting custom surface management logic difficult without breaking abstractions.

The event loop ownership model means that once `run_app()` is called, the application loses the ability to coordinate with external event sources. This is particularly problematic for video playback, which needs to respond to decoder events, frame timing callbacks, and potentially external playback controls.

Frame timing presents additional challenges. iced's render cycle responds to winit's `RedrawRequested` events, which don't align with video frame timing or Wayland's `wl_surface.frame` callbacks. Without coordination between these timing sources, video playback may suffer from judder or tearing.

Potential workarounds include using async-winit to break the traditional event loop pattern, though this remains experimental. The proxy surface pattern, where native surfaces are managed externally but present texture data to iced through existing APIs, offers more stability but sacrifices the performance benefits of true subsurface rendering.

## GStreamer waylandsink configuration enables direct subsurface rendering

GStreamer's waylandsink element can render directly to Wayland subsurfaces through the VideoOverlay interface. The proper integration pattern uses a bus sync handler:

```rust
fn bus_sync_handler(
    _bus: &gst::Bus,
    message: &gst::Message,
    surface_handle: usize,
) -> gst::BusSyncReply {
    if is_video_overlay_prepare_window_handle_message(message) {
        if let Some(overlay) = message.src()
            .and_then(|s| s.dynamic_cast::<VideoOverlay>().ok()) {
            overlay.set_window_handle(surface_handle);
        }
    }
    gst::BusSyncReply::Pass
}
```

**The waylandsink automatically handles synchronization during geometry changes**, switching between synchronized and desynchronized modes as needed. It supports DMABuf memory for zero-copy rendering paths when hardware decoders are available, falling back to shared memory for software decoding.

Format negotiation happens automatically based on hardware capabilities, compositor support, and performance requirements. The sink prioritizes DMABuf formats like NV12 for hardware acceleration, RGB formats for GPU efficiency, and YUV formats as CPU fallbacks.

## Known limitations create significant implementation challenges

The research reveals several fundamental blockers for seamless iced-Wayland subsurface integration. The **winit architecture fundamentally conflicts with subsurface management requirements**, as its event loop model assumes single-window applications without external surface hierarchies.

wgpu's Vulkan backend shows stability issues on Wayland, with "parent device is lost" errors reported when using certain environment configurations. These suggest deeper incompatibilities in the graphics stack that could manifest unpredictably in production.

Threading constraints imposed by Rust's lifetime system conflict with efficient video decode/display pipelines. Wayland objects generally aren't Send/Sync, requiring all operations on the connection thread. Combined with wgpu's surface lifetime restrictions, this makes it difficult to implement the multi-threaded architectures typical in video players.

Perhaps most critically, iced provides no mechanism to coordinate its `wl_surface.commit` calls with subsurface commits. This could lead to visual artifacts where UI and video content update out of sync, particularly problematic for applications with overlay controls.

## Synchronization requirements demand careful architectural planning

Successful integration requires coordinating multiple timing domains. **Wayland frame callbacks must synchronize with iced's render cycle** to prevent tearing while maintaining smooth playback. The video decoder's frame rate might not match the display refresh rate, requiring frame dropping or interpolation strategies.

Subsurface commit timing becomes critical. In desynchronized mode, video frames can present immediately, but position or size changes still synchronize with parent commits. This means UI operations that reposition video content must coordinate with the video rendering pipeline to prevent visual glitches.

Damage tracking across surfaces requires coordination to avoid unnecessary redraws. When video content updates, only the video subsurface should be marked damaged, not the entire UI. Conversely, UI updates shouldn't trigger video surface recomposition.

The presentation feedback protocol could provide accurate timing information, but integrating it with iced's render loop requires architectural changes. Without proper timing coordination, applications can't implement features like audio/video synchronization or smooth variable-speed playback.

## Practical implementation recommendations

Given these constraints, the most pragmatic approach involves creating a hybrid architecture that uses native subsurfaces where beneficial while maintaining compatibility with iced's rendering model. **The video widget should manage its own Wayland subsurface** but present a standard iced Widget interface for layout and event handling.

Resource management should follow RAII patterns with careful cleanup in Drop implementations. Subsurfaces must be destroyed before their parent surfaces, requiring explicit ordering in cleanup code. Buffer pools should be sized appropriately for the expected video resolution to avoid reallocation during playback.

Error handling must account for both Wayland protocol errors and wgpu rendering failures. The implementation should gracefully fall back to texture-based rendering if subsurface creation fails, ensuring video playback works even in degraded conditions.

For optimal performance, the implementation should detect and use hardware video decoding when available, routing DMABuf handles directly to waylandsink. This zero-copy path eliminates unnecessary memory transfers and GPU operations, significantly reducing power consumption and improving playback smoothness.

## Conclusion

Creating a Wayland-specific iced video player crate that truly leverages native subsurfaces faces significant architectural impediments. The fundamental mismatch between iced's controlled rendering model and Wayland's surface hierarchy concept creates integration challenges at every level, from event loop management to frame synchronization.

The most viable path forward involves accepting certain architectural compromises. Rather than attempting to fully integrate subsurfaces into iced's widget system, **a practical implementation should treat video playback as a special case** that bypasses normal rendering paths while maintaining API compatibility. This approach sacrifices some of the theoretical benefits of full integration but remains achievable within current framework constraints.

Success requires careful attention to resource management, error handling, and synchronization between multiple timing domains. While the technical challenges are substantial, the performance benefits of native Wayland subsurface rendering—particularly for hardware-accelerated video decode—justify the implementation complexity for applications where video playback is a primary feature.