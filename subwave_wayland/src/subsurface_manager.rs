use crate::{Error, Result, WaylandIntegration};
use parking_lot::Mutex;
use std::io::Write;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tempfile::tempfile;
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_surface::Event;
use wayland_client::{
    protocol::{
        wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_registry::WlRegistry, wl_shm::Format,
        wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_subcompositor::WlSubcompositor,
        wl_subsurface::WlSubsurface, wl_surface::WlSurface,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

/// Manages a Wayland subsurface for video rendering
pub struct WaylandSubsurfaceManager {
    /// The Wayland connection (shared with parent)
    _connection: Connection,

    // The Wayland integration data from Iced
    pub integration: WaylandIntegration,

    /// Event queue for handling Wayland events
    event_queue: Mutex<EventQueue<State>>,

    /// Shared compositor
    compositor: WlCompositor,

    /// The subsurface protocol object
    pub video_subsurface: WlSubsurface,

    /// Background subsurface for black background
    background_subsurface: WlSubsurface,

    /// The video surface
    video_surface: WlSurface,

    /// Background surface
    background_surface: WlSurface,

    /// Subtitle subsurface (overlay)
    subtitle_subsurface: WlSubsurface,

    /// Subtitle surface
    subtitle_surface: WlSurface,

    /// Viewport for controlling surface size independently of buffer size
    video_viewport: Option<WpViewport>,

    /// Viewport for background surface
    background_viewport: Option<WpViewport>,

    /// Viewport for subtitle surface
    subtitle_viewport: Option<WpViewport>,

    /// Current position relative to parent
    position: Arc<Mutex<(i32, i32)>>,

    /// Current size
    size: Arc<Mutex<(i32, i32)>>,

    /// The size of the renderable area we provide gstreamer
    source_size: Arc<Mutex<(i32, i32, i32, i32)>>,

    /// Flag indicating we need to update on next parent commit
    needs_update: Arc<AtomicBool>,

    /// Shared memory object for creating black buffer
    shm: Option<WlShm>,

    /// Background buffer (black rectangle)
    background_buffer: Mutex<Option<WlBuffer>>,
    background_pool: Mutex<Option<WlShmPool>>,

    /// Subtitle buffer resources
    subtitle_buffer: Mutex<Option<WlBuffer>>,
    subtitle_pool: Mutex<Option<WlShmPool>>,
    subtitle_file: Mutex<Option<std::fs::File>>,
    subtitle_pool_dims: Mutex<Option<(i32, i32, i32)>>, // (w,h,stride)
}

impl std::fmt::Debug for WaylandSubsurfaceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandVideoSubsurface")
            .field("position", &self.position.lock())
            .field("size", &self.size.lock())
            .field(
                "needs_update",
                &self.needs_update.load(std::sync::atomic::Ordering::Relaxed),
            )
            .field("has_buffer", &self.background_buffer.lock().is_some())
            .finish()
    }
}

/// State for Wayland event dispatching
struct State {
    globals: Vec<(u32, String, u32)>, // (name, interface, version)
}

impl State {
    fn new() -> Self {
        Self {
            globals: Vec::new(),
        }
    }
}

impl WaylandSubsurfaceManager {
    /// Create a new video subsurface as a child of the given parent surface
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(integration: WaylandIntegration) -> Result<Arc<Self>> {
        unsafe {
            // Create backend from the foreign display - this creates a "guest" backend
            // that won't close the connection when dropped
            let backend = Backend::from_foreign_display(integration.display as *mut _);

            // Create connection from the backend
            let connection = Connection::from_backend(backend);

            let mut event_queue = connection.new_event_queue();
            let qh = event_queue.handle();

            let display = connection.display();

            let registry = display.get_registry(&qh, ());

            let mut state = State::new();

            // Roundtrip to receive all global events during initialization (necessary)
            event_queue
                .roundtrip(&mut state)
                .map_err(|e| Error::Wayland(format!("Failed to roundtrip: {}", e)))?;

            let compositor = if let Some(compositor_global) = state
                .globals
                .iter()
                .find(|(_, interface, _)| interface == "wl_compositor")
            {
                let compositor: WlCompositor =
                    registry.bind(compositor_global.0, compositor_global.2.min(6), &qh, ());
                compositor
            } else {
                return Err(Error::Wayland("No compositor found".into()));
            };

            let subcompositor_global = state
                .globals
                .iter()
                .find(|(_, interface, _)| interface == "wl_subcompositor")
                .ok_or_else(|| Error::Wayland("No subcompositor found".into()))?;

            let subcompositor: WlSubcompositor = registry.bind(
                subcompositor_global.0,
                subcompositor_global.2.min(1),
                &qh,
                (),
            );

            let viewporter = if let Some(viewporter_global) = state
                .globals
                .iter()
                .find(|(_, interface, _)| interface == "wp_viewporter")
            {
                let viewporter: WpViewporter =
                    registry.bind(viewporter_global.0, viewporter_global.2.min(1), &qh, ());
                log::info!("Found and bound wp_viewporter");
                Some(viewporter)
            } else {
                log::error!("No wp_viewporter found - viewport sizing unavailable");
                None
            };

            // Shm buffer for background data
            let shm = if let Some(shm_global) = state
                .globals
                .iter()
                .find(|(_, interface, _)| interface == "wl_shm")
            {
                let shm: WlShm = registry.bind(shm_global.0, shm_global.2.min(1), &qh, ());
                log::debug!("Found and bound wl_shm for black background buffer");
                Some(shm)
            } else {
                log::error!("No wl_shm found - black background buffer will not be available");
                None
            };

            // Create a proxy for the parent surface without taking ownership
            // The parent surface is already managed by winit/iced
            log::debug!(
                "Creating parent surface proxy from ptr: {:p}",
                integration.surface as *const _
            );

            let parent_surface_id =
                ObjectId::from_ptr(WlSurface::interface(), integration.surface as *mut _);

            let parent_surface: WlSurface = match parent_surface_id {
                Ok(id) => {
                    log::debug!("Created ObjectId: {:?}", id);
                    // Create the proxy from the ObjectId without managing it
                    let parent_surface = Proxy::from_id(&connection, id);
                    match parent_surface {
                        Ok(parent_surface) => {
                            log::debug!("Successfully created parent surface proxy");
                            parent_surface
                        }
                        Err(e) => {
                            log::error!("Failed to create proxy from ID: {}", e);
                            return Err(Error::Wayland(format!(
                                "Failed to create parent surface proxy: {}",
                                e
                            )));
                        }
                    }
                }
                Err(e) => {
                    log::error!("Failed to create ObjectId: {}", e);
                    return Err(Error::Wayland(format!(
                        "Failed to create parent surface proxy: {}",
                        e
                    )));
                }
            };

            let background_surface = compositor.create_surface(&qh, ());
            log::debug!("Created background surface");

            let video_surface = compositor.create_surface(&qh, ());
            log::debug!("Created video surface");

            let subtitle_surface = compositor.create_surface(&qh, ());
            log::debug!("Created subtitle surface");

            // Make subtitle surface input-transparent so parent controls remain usable
            // Create an empty region and set it as the input region for the subtitle surface
            let empty_region = compositor.create_region(&qh, ());
            subtitle_surface.set_input_region(Some(&empty_region));
            empty_region.destroy();
            log::info!("[subs] Subtitle surface input region set to empty (passthrough)");

            let background_viewport = if let Some(ref viewporter) = viewporter {
                let viewport = viewporter.get_viewport(&background_surface, &qh, ());
                log::debug!("Created viewport for background surface");
                Some(viewport)
            } else {
                None
            };

            let video_viewport = if let Some(ref viewporter) = viewporter {
                let viewport = viewporter.get_viewport(&video_surface, &qh, ());
                log::debug!("Created viewport for video surface");
                Some(viewport)
            } else {
                None
            };

            let subtitle_viewport = if let Some(ref viewporter) = viewporter {
                let viewport = viewporter.get_viewport(&subtitle_surface, &qh, ());
                log::debug!("Created viewport for subtitle surface");
                Some(viewport)
            } else {
                None
            };

            // Background (bottom layer)
            let background_subsurface =
                subcompositor.get_subsurface(&background_surface, &parent_surface, &qh, ());
            log::debug!("Created background subsurface");

            // Video (middle layer)
            let video_subsurface =
                subcompositor.get_subsurface(&video_surface, &parent_surface, &qh, ());
            log::debug!("Created video subsurface");

            // Subtitle (top under parent)
            let subtitle_subsurface =
                subcompositor.get_subsurface(&subtitle_surface, &parent_surface, &qh, ());
            log::debug!("Created subtitle subsurface");

            // Set to desynchronized mode for independent video and subtitle updates
            video_subsurface.set_desync();
            subtitle_subsurface.set_desync();
            log::debug!(
                "Set video and subtitle subsurfaces to desync mode for independent updates"
            );

            // Background surface can be synchronized with parent since it only needs to change on resize
            background_subsurface.set_sync();
            log::debug!("Set background subsurface to sync mode");

            // Z-ordering: video below parent, subtitle above parent
            video_subsurface.place_below(&parent_surface);

            // IMPORTANT: Put subtitles above the parent so they truly overlay video
            subtitle_subsurface.place_above(&parent_surface);

            background_subsurface.place_below(&video_surface);

            // Commit children so compositor can pick up reordering right away
            background_surface.commit();
            video_surface.commit();
            subtitle_surface.commit();

            // Both subsurface default position is (0, 0) in the top-left corner of the parent surface,
            // only reason to modify is PIP support

            background_surface.commit();
            video_surface.commit();
            subtitle_surface.commit();

            // Roundtrip to ensure subsurfaces are properly registered
            event_queue.roundtrip(&mut state).map_err(|e| {
                Error::Wayland(format!(
                    "Failed to roundtrip after subsurface creation: {}",
                    e
                ))
            })?;

            let subsurface_manager = Arc::new(Self {
                _connection: connection,
                integration: integration.clone(),
                event_queue: Mutex::new(event_queue),
                compositor,
                video_subsurface,
                background_subsurface,
                video_surface,
                background_surface,
                subtitle_subsurface,
                subtitle_surface,
                video_viewport,
                background_viewport,
                subtitle_viewport,
                position: Arc::new(Mutex::new((0, 0))),
                size: Arc::new(Mutex::new((0, 0))),
                source_size: Arc::new(Mutex::new((0, 0, 0, 0))),
                needs_update: Arc::new(AtomicBool::new(false)),
                shm,
                background_buffer: Mutex::new(None),
                background_pool: Mutex::new(None),
                subtitle_buffer: Mutex::new(None),
                subtitle_pool: Mutex::new(None),
                subtitle_file: Mutex::new(None),
                subtitle_pool_dims: Mutex::new(None),
            });

            // Create initial background buffer
            if let Err(e) = subsurface_manager.ensure_background_buffer() {
                log::error!("Failed to create initial background buffer: {}", e);
            } else {
                // Set an initial size for the background
                if let Some(ref viewport) = subsurface_manager.background_viewport {
                    viewport.set_destination(1280, 720);
                    log::debug!(
                        "Set initial background size to 1280x720 (will be updated on first resize)"
                    );
                }
                subsurface_manager
                    .background_surface
                    .damage(0, 0, 1280, 720);
                subsurface_manager.background_surface.commit();

                // Flush to ensure the background is processed
                if let Err(e) = subsurface_manager.flush() {
                    log::warn!("Failed to flush after background setup: {}", e);
                }
            }

            // Register pre-commit hook for position synchronization
            // Use weak references to avoid reference cycles
            let needs_update_weak = Arc::downgrade(&subsurface_manager.needs_update);
            let position_weak = Arc::downgrade(&subsurface_manager.position);
            let size_weak = Arc::downgrade(&subsurface_manager.size);
            let source_size_weak = Arc::downgrade(&subsurface_manager.source_size);
            let subsurface_clone = subsurface_manager.video_subsurface.clone();
            let video_surface_clone = subsurface_manager.video_surface.clone();
            let viewport_clone = subsurface_manager.video_viewport.clone();
            let background_subsurface_clone = subsurface_manager.background_subsurface.clone();
            let background_surface_clone = subsurface_manager.background_surface.clone();
            let background_viewport_clone = subsurface_manager.background_viewport.clone();
            let subtitle_subsurface_clone = subsurface_manager.subtitle_subsurface.clone();
            let subtitle_surface_clone = subsurface_manager.subtitle_surface.clone();
            let subtitle_viewport_clone = subsurface_manager.subtitle_viewport.clone();

            integration.register_pre_commit_hook(move || {
                // Check weak references and bail early if they're gone
                let (needs_update, position, size, source_size) = match (
                    needs_update_weak.upgrade(),
                    position_weak.upgrade(),
                    size_weak.upgrade(),
                    source_size_weak.upgrade(),
                ) {
                    (Some(n), Some(p), Some(s), Some(src)) => (n, p, s, src),
                    _ => return, // Subsurface has been dropped, nothing to do
                };

                if needs_update.swap(false, Ordering::Relaxed) {
                    let (x, y) = *position.lock();
                    let (dest_w, dest_h) = *size.lock();

                    // Update video subsurface position
                    subsurface_clone.set_position(x, y);

                    // Update background subsurface position and size
                    background_subsurface_clone.set_position(x, y);
                    if let Some(ref bg_viewport) = background_viewport_clone {
                        bg_viewport.set_destination(dest_w, dest_h);
                        log::debug!("Background viewport updated to {}x{}", dest_w, dest_h);
                        background_surface_clone.damage(0, 0, dest_w, dest_h);
                        log::debug!(
                            "Background committed at ({},{}) size {}x{}",
                            x,
                            y,
                            dest_w,
                            dest_h
                        );
                    } else {
                        log::error!("Error: No background viewport in pre-commit hook!");
                    }

                    // Update subtitle subsurface position to match video
                    subtitle_subsurface_clone.set_position(x, y);
                    if let Some(ref sub_viewport) = subtitle_viewport_clone {
                        sub_viewport.set_destination(dest_w, dest_h);
                        log::debug!("Background viewport updated to {}x{}", dest_w, dest_h);
                        subtitle_surface_clone.damage(0, 0, dest_w, dest_h);
                        log::debug!(
                            "Background committed at ({},{}) size {}x{}",
                            x,
                            y,
                            dest_w,
                            dest_h
                        );
                    } else {
                        log::error!("Error: No subtitle viewport in pre-commit hook!");
                    }

                    log::debug!("[subs] Subtitle subsurface positioned at ({}, {})", x, y);

                    // Update video viewport (if present); otherwise skip to avoid complications
                    if let Some(ref vp) = viewport_clone {
                        vp.set_destination(dest_w, dest_h);
                        log::debug!("Updated dest to {}x{}", dest_w, dest_h);
                        let (x, y, w, h) = *source_size.lock();
                        vp.set_source(
                            f64::from(x.max(1)),
                            f64::from(y.max(1)),
                            f64::from(w.max(1)),
                            f64::from(h.max(1)),
                        );
                        video_surface_clone.damage(0, 0, dest_w, dest_h);
                    }
                }
            });

            Ok(subsurface_manager)
        }
    }

    /// Attach a rendered ARGB32 subtitle frame to the subtitle surface and commit
    pub fn attach_subtitle_frame(
        &self,
        data: &[u8],
        width: i32,
        height: i32,
        stride: i32,
    ) -> Result<()> {
        if self.shm.is_none() {
            return Err(Error::Wayland("No wl_shm for subtitle".into()));
        }
        let needed = (stride as usize) * (height as usize);
        log::debug!(
            "[subs] attach_subtitle_frame called: {}x{} stride={} ({} bytes)",
            width,
            height,
            stride,
            needed
        );

        let mut pool_guard = self.subtitle_pool.lock();
        let mut buf_guard = self.subtitle_buffer.lock();
        let mut file_guard = self.subtitle_file.lock();
        let mut dims_guard = self.subtitle_pool_dims.lock();

        let need_recreate = match *dims_guard {
            Some((w, h, s)) => w != width || h != height || s != stride,
            None => true,
        };
        if need_recreate {
            log::info!(
                "[subs] Recreating subtitle buffer/pool for size {}x{} stride={}",
                width,
                height,
                stride
            );
            if let Some(old) = buf_guard.take() {
                old.destroy();
            }
            if let Some(old) = pool_guard.take() {
                old.destroy();
            }
            *file_guard = None;

            let file = tempfile::tempfile()
                .map_err(|e| Error::Wayland(format!("subtitle tempfile: {}", e)))?;

            file.set_len(needed as u64)
                .map_err(|e| Error::Wayland(format!("subtitle resize: {}", e)))?;

            let event_queue = self.event_queue.lock();
            let qh = event_queue.handle();
            let shm = self.shm.as_ref().unwrap();
            let pool = shm.create_pool(file.as_fd(), needed as i32, &qh, ());
            let buffer = pool.create_buffer(0, width, height, stride, Format::Argb8888, &qh, ());

            *pool_guard = Some(pool);
            *buf_guard = Some(buffer);
            *file_guard = Some(file);
            *dims_guard = Some((width, height, stride));
        }

        if let Some(file) = file_guard.as_mut() {
            use std::io::{Seek, SeekFrom, Write};
            file.seek(SeekFrom::Start(0))
                .map_err(|e| Error::Wayland(format!("subtitle seek: {}", e)))?;
            file.write_all(data)
                .map_err(|e| Error::Wayland(format!("subtitle write: {}", e)))?;
            file.flush().ok();
        }

        if let Some(ref buffer) = &*buf_guard {
            log::debug!("[subs] Attaching buffer to subtitle surface and committing");
            self.subtitle_surface.attach(Some(buffer), 0, 0);
            self.subtitle_surface.damage(0, 0, width, height);
            self.subtitle_surface.commit();
        } else {
            log::warn!("[subs] Subtitle surface/buffer missing; cannot attach subtitle frame");
        }
        Ok(())
    }

    /// Clear the subtitle surface by detaching any buffer and committing
    pub fn clear_subtitle(&self) -> Result<()> {
        log::debug!("[subs] Clearing subtitle surface (detach + commit)");
        self.subtitle_surface.attach(None, 0, 0);
        self.subtitle_surface.commit();
        Ok(())
    }

    /// DEBUG: Paint a visible test pattern onto the subtitle subsurface.
    /// Enabled by callers for sanity checks during playback.
    pub fn debug_show_test_overlay(&self) -> Result<()> {
        let (mut w, mut h) = self.get_size();
        if w <= 0 || h <= 0 {
            // Fallback to a reasonable default if size has not been set yet
            w = 640;
            h = 360;
        }
        let stride = w * 4;
        let mut data = vec![0u8; (stride as usize) * (h as usize)];

        // Fill with transparent background (already zeroed)
        // Draw a bright magenta bar in the center
        let rect_w = (w / 2).max(64).min(w);
        let rect_h = (h / 6).max(32).min(h);
        let rx0 = (w - rect_w) / 2;
        let ry0 = (h - rect_h) / 2;
        for y in ry0..(ry0 + rect_h) {
            let row = (y * stride) as usize;
            for x in rx0..(rx0 + rect_w) {
                let idx = row + (x as usize) * 4;
                // wl_shm Format::Argb8888 on little-endian is stored as BGRA bytes
                data[idx] = 0xFF; // B
                data[idx + 1] = 0x00; // G
                data[idx + 2] = 0xFF; // R
                data[idx + 3] = 0xFF; // A (opaque)
            }
        }

        // Corner markers (lime) for extra visibility
        let mark_w = w.clamp(20, 200);
        let mark_h = h.clamp(10, 60);
        for y in 0..mark_h {
            let row = (y * stride) as usize;
            for x in 0..mark_w {
                let idx = row + (x as usize) * 4;
                data[idx] = 0x00; // B
                data[idx + 1] = 0xFF; // G
                data[idx + 2] = 0x00; // R
                data[idx + 3] = 0xFF; // A
            }
        }

        log::info!(
            "[subs][DEBUG] Painting test overlay onto subtitle surface ({}x{} stride={})",
            w,
            h,
            stride
        );
        self.attach_subtitle_frame(&data, w, h, stride)
    }

    /// Set or clear input passthrough on the subtitle surface.
    /// When enabled, the subtitle surface will not receive input events
    /// (pointer/keyboard), allowing the parent UI to handle them.
    pub fn set_subtitle_input_passthrough(&self, enable: bool) {
        let qh = self.event_queue.lock().handle();
        if enable {
            let region = self.compositor.create_region(&qh, ());
            self.subtitle_surface.set_input_region(Some(&region)); // empty region
            region.destroy();
        } else {
            // None restores default input region matching the surface extents
            self.subtitle_surface.set_input_region(None);
        }
        self.subtitle_surface.commit();
    }

    /// Set the position of the video surface relative to the parent
    pub fn set_position(&self, x: i32, y: i32) {
        let current_pos = *self.position.lock();
        if current_pos != (x, y) {
            *self.position.lock() = (x, y);
            self.needs_update.store(true, Ordering::Relaxed);
        }
    }

    pub fn set_size(&self, w: i32, h: i32) {
        log::info!("[subs] WaylandSubsurfaceManager::set_size -> {}x{}", w, h);
        *self.size.lock() = (w, h);

        self.needs_update.store(true, Ordering::Relaxed);
        self.video_surface.commit();
        self.subtitle_surface.commit();
    }

    pub fn set_source_size(&self, (x, y, w, h): (i32, i32, i32, i32)) {
        *self.source_size.lock() = (x, y, w, h);

        self.needs_update.store(true, Ordering::Relaxed);
        self.video_surface.commit();
    }

    /// Get the current position
    pub fn get_position(&self) -> (i32, i32) {
        *self.position.lock()
    }

    /// Get the current size
    pub fn get_size(&self) -> (i32, i32) {
        *self.size.lock()
    }

    /// Get the current source size
    pub fn get_source_size(&self) -> (i32, i32, i32, i32) {
        *self.source_size.lock()
    }

    // Do we have use for this function?
    pub fn set_buffer_offset(&self, x: i32, y: i32) {
        self.video_surface.offset(x, y);

        // Mark the entire surface as damaged when size changes
        self.video_surface.damage_buffer(0, 0, x, y);
        self.video_surface.commit();
        log::debug!("Buffer offset changed to {}x{}, surface committed", x, y,);
    }

    /// Set video viewport with source and destination rectangles for ContentFit mapping
    /// source: Optional source rectangle (x, y, width, height) in wl_fixed coordinates
    /// dest: Destination size (width, height) in surface coordinates
    pub fn set_video_viewport(
        &self,
        source: Option<(i32, i32, i32, i32)>,
        dest: Option<(i32, i32)>,
    ) {
        if let Some(ref viewport) = self.video_viewport {
            // Set source rectangle if provided (for cropping/scaling)
            if let Some((x, y, w, h)) = source {
                viewport.set_source(f64::from(x), f64::from(y), f64::from(w), f64::from(h));
                log::debug!(
                    "Viewport source set to ({:.2}, {:.2}, {:.2}, {:.2})",
                    x,
                    y,
                    w,
                    h
                );
            }

            if let Some((x, y)) = dest {
                // Set destination size (surface size)
                viewport.set_destination(x, y);
                log::debug!("Viewport destination set to {}x{}", x, y);
            }

            self.video_surface.commit();
        } else {
            log::error!("No viewport available");
        }
    }

    pub fn set_video_surface_opaque_region(&self, x: i32, y: i32, width: i32, height: i32) {
        let qh = self.event_queue.lock().handle();
        let region = self.compositor.create_region(&qh, ());
        region.add(x, y, width, height);
        self.video_surface.set_opaque_region(Some(&region));
        region.destroy()
    }

    /// Get the surface handle for GStreamer waylandsink
    pub fn surface_handle(&self) -> usize {
        let handle = self.video_surface.id().as_ptr() as usize;

        log::debug!(
            "Returning surface handle: 0x{:x} (raw wl_surface for GStreamer)",
            handle
        );
        handle
    }

    /// Get the surface handle for GStreamer waylandsink
    pub fn subtitle_surface_handle(&self) -> usize {
        let handle = self.subtitle_surface.id().as_ptr() as usize;

        log::debug!(
            "Returning surface handle: 0x{:x} (raw wl_surface for GStreamer)",
            handle
        );
        handle
    }

    /// Flush any pending Wayland events
    pub fn flush(&self) -> Result<()> {
        self.event_queue
            .lock()
            .flush()
            .map_err(|e| Error::Wayland(format!("Failed to flush events: {}", e)))?;
        Ok(())
    }

    /// Force a full surface damage and commit (useful for debugging visibility)
    pub fn force_damage_and_commit(&self) {
        // Damage the entire surface to force a redraw
        self.video_surface.damage(0, 0, i32::MAX, i32::MAX);
        self.video_surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.video_surface.commit();
        self.background_surface.damage(0, 0, i32::MAX, i32::MAX);
        self.background_surface
            .damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.background_surface.commit();
        self.subtitle_surface.damage(0, 0, i32::MAX, i32::MAX);
        self.subtitle_surface
            .damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.subtitle_surface.commit();
        eprintln!("Forced full damage and commit on video surface");
    }

    /// Create or update the black background buffer
    fn ensure_background_buffer(&self) -> Result<()> {
        if self.shm.is_none() {
            let msg = "No wl_shm available, cannot create background buffer";
            return Err(Error::Wayland(msg.to_string()));
        }

        if self.background_buffer.lock().is_some() {
            return Ok(());
        }

        let shm = self.shm.as_ref().unwrap(); // We just checked that it's Some

        // Initially create a large buffer to ensure initial visibility
        let width = 4000;
        let height = 4000;
        let stride = width * 4;
        let size = (stride * height) as usize;

        // Create a temporary file for the shared memory
        let mut file =
            tempfile().map_err(|e| Error::Wayland(format!("Failed to create temp file: {}", e)))?;

        // Resize the file to the required size
        file.set_len(size as u64)
            .map_err(|e| Error::Wayland(format!("Failed to resize temp file: {}", e)))?;

        // Black
        let mut buffer = Vec::with_capacity(size);
        for _ in 0..(width * height) {
            buffer.push(0x0); // Blue
            buffer.push(0x0); // Green
            buffer.push(0x0); // Red
            buffer.push(0xFF); // Alpha
        }

        file.write_all(&buffer)
            .map_err(|e| Error::Wayland(format!("Failed to write buffer: {}", e)))?;
        file.sync_all()
            .map_err(|e| Error::Wayland(format!("Failed to sync file: {}", e)))?;

        // Create the shm pool
        let event_queue = self.event_queue.lock();
        let qh = event_queue.handle();
        let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());

        // Create a buffer from the pool
        let buffer = pool.create_buffer(
            0,                // offset
            width,            // width
            height,           // height
            stride,           // stride
            Format::Argb8888, // format
            &qh,
            (),
        );

        // Attach the buffer to the background surface
        self.background_surface.attach(Some(&buffer), 0, 0);
        self.background_surface.damage(0, 0, width, height);
        self.background_surface.commit();

        // Store the buffer and pool
        *self.background_buffer.lock() = Some(buffer);
        *self.background_pool.lock() = Some(pool);

        Ok(())
    }

    /// Update the background subsurface size
    pub fn update_background(&self, width: i32, height: i32) {
        log::debug!("Update_background called with {}x{}", width, height);

        // Ensure we have a red buffer
        if let Err(e) = self.ensure_background_buffer() {
            log::error!("Failed to create background buffer: {}", e);
            return;
        }

        // Update the background viewport
        if let Some(ref viewport) = self.background_viewport {
            viewport.set_destination(width, height);
            log::debug!("Background viewport set to {}x{}", width, height);
        } else {
            log::warn!("No background viewport available!");
        }

        // Update position to match video subsurface
        let (x, y) = *self.position.lock();
        self.background_subsurface.set_position(x, y);
        log::debug!("Background positioned at ({}, {})", x, y);

        //let qh = self.event_queue.lock().handle();
        //let bg_region = self.compositor.create_region(&qh, ());
        //bg_region.add(x, y, width, height);
        //self.background_surface.set_opaque_region(Some(&bg_region));

        self.background_surface.damage(0, 0, width, height);
        self.background_surface.commit();
        //bg_region.destroy();
        log::debug!("Background surface damaged and committed");
    }
}

impl Drop for WaylandSubsurfaceManager {
    fn drop(&mut self) {
        eprintln!("[WaylandVideoSubsurface] Beginning cleanup");

        // CRITICAL: Clear pre-commit hooks first to break reference cycles
        // This prevents the hooks from being called during cleanup
        self.integration.clear_pre_commit_hooks();
        eprintln!("[WaylandVideoSubsurface] Cleared pre-commit hooks");

        // Proper cleanup order per Wayland documentation:
        // 1. First unmap subsurfaces by attaching NULL buffers
        // 2. Commit the surfaces
        // 3. Destroy the subsurfaces (must be done BEFORE parent surface destruction)
        // 4. Finally destroy the surfaces

        // Unmap the video subsurface by attaching NULL buffer
        self.video_surface.attach(None, 0, 0);
        self.video_surface.commit();

        // Unmap the background subsurface by attaching NULL buffer
        self.background_surface.attach(None, 0, 0);
        self.background_surface.commit();

        // Unmap subtitle surface if present
        self.subtitle_surface.attach(None, 0, 0);
        self.subtitle_surface.commit();

        // Flush events to ensure unmapping is processed
        if let Err(e) = self.flush() {
            eprintln!(
                "[WaylandVideoSubsurface] Warning: Failed to flush during cleanup: {}",
                e
            );
        }

        // Clean up buffers and pools
        if let Some(buffer) = self.background_buffer.lock().take() {
            buffer.destroy();
        }
        if let Some(pool) = self.background_pool.lock().take() {
            pool.destroy();
        }
        if let Some(buffer) = self.subtitle_buffer.lock().take() {
            buffer.destroy();
        }
        if let Some(pool) = self.subtitle_pool.lock().take() {
            pool.destroy();
        }
        self.subtitle_file.lock().take();

        // Destroy viewports if they exist
        if let Some(ref viewport) = self.video_viewport {
            viewport.destroy();
        }
        if let Some(ref viewport) = self.background_viewport {
            viewport.destroy();
        }

        // Now destroy the subsurfaces (after unmapping)
        self.video_subsurface.destroy();
        self.background_subsurface.destroy();
        self.subtitle_subsurface.destroy();

        // Finally destroy the surfaces
        self.video_surface.destroy();
        self.background_surface.destroy();
        self.subtitle_surface.destroy();

        eprintln!("[WaylandVideoSubsurface] Cleanup completed");
    }
}

// Event dispatch implementation (minimal, as we don't need to handle many events)
impl Dispatch<WlSurface, ()> for State {
    fn event(
        _state: &mut Self,
        _surface: &WlSurface,
        event: <WlSurface as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // For subsurfaces, you usually don't need to handle these events
        // since they're secondary surfaces. Let Iced handle the main surface events.
        match event {
            Event::Enter { .. }
            | Event::Leave { .. }
            | Event::PreferredBufferScale { .. }
            | Event::PreferredBufferTransform { .. } => {
                // No action needed for subsurfaces in most cases
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSubsurface, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSubsurface,
        _event: <WlSubsurface as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Subsurface doesn't have client-side events
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Compositor doesn't have events
    }
}

impl Dispatch<WlRegion, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegion,
        _event: <WlRegion as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // No events to handle for wl_shm_pool
    }
}

impl Dispatch<WlSubcompositor, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSubcompositor,
        _event: <WlSubcompositor as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Subcompositor doesn't have events
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event;
        match event {
            Event::Global {
                name,
                interface,
                version,
            } => {
                state.globals.push((name, interface, version));
            }
            Event::GlobalRemove { name: _ } => {
                // We don't handle removal during initialization
            }
            _ => {}
        }
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        _event: <WlShm as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // No events to handle for wl_shm in this context
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlShmPool,
        _event: <WlShmPool as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // No events to handle for wl_shm_pool
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        event: <WlBuffer as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_buffer::Event;
        if let Event::Release = event {
            // Buffer has been released by compositor - it's now available for reuse
            // In a real video player, this would trigger the next frame
            // For our test, we just note it
            log::debug!("Buffer released by compositor - ready for reuse");
            // Note: We keep the buffer alive so the surface doesn't become empty
        }
    }
}

impl Dispatch<WpViewporter, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: <WpViewporter as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Viewporter doesn't have events
    }
}

impl Dispatch<WpViewport, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: <WpViewport as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Viewport doesn't have events
    }
}
