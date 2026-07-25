use crate::{Error, Result, WaylandIntegration};
use parking_lot::Mutex;
use std::io::{Seek, SeekFrom, Write};
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

use crate::color_management::ColorManager;

/// Manages a Wayland subsurface for video rendering
pub struct WaylandSubsurfaceManager {
    /// The Wayland connection (shared with parent)
    _connection: Connection,

    // The Wayland integration data from Iced
    pub integration: WaylandIntegration,

    /// Event queue and dispatch state for handling buffer release events.
    event_queue: Mutex<EventQueue<State>>,
    dispatch_state: Mutex<State>,

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

    /// Viewport for background surface
    background_viewport: Option<WpViewport>,

    /// Viewport for subtitle surface
    subtitle_viewport: Option<WpViewport>,

    /// Current position relative to parent
    position: Arc<Mutex<(i32, i32)>>,

    /// Current size
    size: Arc<Mutex<(i32, i32)>>,

    /// Flag indicating we need to update on next parent commit
    needs_update: Arc<AtomicBool>,

    /// Shared memory object for creating surface buffers
    shm: Option<WlShm>,

    /// Transparent buffer that keeps the intermediary GStreamer host surface mapped.
    /// `waylandsink` creates its own subsurfaces beneath this surface, so Wayland
    /// requires this ancestor to have a non-null buffer before video can be visible.
    video_anchor_buffer: WlBuffer,
    video_anchor_pool: WlShmPool,

    /// Background buffer (black rectangle)
    background_buffer: Mutex<Option<WlBuffer>>,
    background_pool: Mutex<Option<WlShmPool>>,

    /// Release-aware subtitle frame buffers. A busy wl_shm buffer must never be
    /// modified until the compositor sends wl_buffer.release.
    subtitle_buffers: Mutex<Vec<SubtitleBufferSlot>>,

    /// Immutable, fully transparent buffers used instead of unmapping the
    /// subtitle surface. Keeping the surface mapped stabilizes Gamescope's HDR
    /// plane/composition graph across subtitle show/clear transitions.
    subtitle_clear_buffers: Mutex<Vec<SubtitleBufferSlot>>,
    subtitle_visible: AtomicBool,
    keep_subtitle_mapped: bool,

    /// Compositor color-management capability handle.
    ///
    /// The transparent host and subtitle surfaces deliberately remain untagged;
    /// GStreamer's `waylandsink` tags its nested video-content surface directly.
    color_manager: Mutex<Option<ColorManager>>,
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
            .field("subtitle_buffers", &self.subtitle_buffers.lock().len())
            .field(
                "subtitle_visible",
                &self.subtitle_visible.load(Ordering::Relaxed),
            )
            .field("keep_subtitle_mapped", &self.keep_subtitle_mapped)
            .finish()
    }
}

/// State for Wayland event dispatching
pub(crate) struct State {
    pub(crate) globals: Vec<(u32, String, u32)>, // (name, interface, version)
}

impl State {
    pub(crate) fn new() -> Self {
        Self {
            globals: Vec::new(),
        }
    }
}

fn is_gamescope_compositor(globals: &[(u32, String, u32)]) -> bool {
    globals
        .iter()
        .any(|(_, interface, _)| interface.starts_with("gamescope_"))
}

const VIDEO_ANCHOR_WIDTH: i32 = 1;
const VIDEO_ANCHOR_HEIGHT: i32 = 1;
const VIDEO_ANCHOR_STRIDE: i32 = VIDEO_ANCHOR_WIDTH * 4;
const VIDEO_ANCHOR_SIZE: usize = (VIDEO_ANCHOR_STRIDE * VIDEO_ANCHOR_HEIGHT) as usize;
const MAX_SUBTITLE_BUFFERS: usize = 2;

#[derive(Clone, Debug)]
struct SubtitleBufferData {
    released: Arc<AtomicBool>,
}

struct SubtitleBufferSlot {
    buffer: WlBuffer,
    pool: WlShmPool,
    file: std::fs::File,
    dimensions: (i32, i32, i32),
    released: Arc<AtomicBool>,
}

impl SubtitleBufferSlot {
    fn is_released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }
}

impl Drop for SubtitleBufferSlot {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

fn create_transparent_video_anchor(
    shm: &WlShm,
    qh: &QueueHandle<State>,
) -> Result<(WlBuffer, WlShmPool)> {
    let mut file = tempfile()
        .map_err(|e| Error::Wayland(format!("Failed to create video anchor tempfile: {e}")))?;
    file.set_len(VIDEO_ANCHOR_SIZE as u64)
        .map_err(|e| Error::Wayland(format!("Failed to resize video anchor tempfile: {e}")))?;

    // wl_shm ARGB8888 is premultiplied; zero alpha makes this pixel fully transparent.
    file.write_all(&[0; VIDEO_ANCHOR_SIZE])
        .map_err(|e| Error::Wayland(format!("Failed to write video anchor buffer: {e}")))?;

    let pool = shm.create_pool(file.as_fd(), VIDEO_ANCHOR_SIZE as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        VIDEO_ANCHOR_WIDTH,
        VIDEO_ANCHOR_HEIGHT,
        VIDEO_ANCHOR_STRIDE,
        Format::Argb8888,
        qh,
        (),
    );

    Ok((buffer, pool))
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

            let keep_subtitle_mapped = is_gamescope_compositor(&state.globals);
            if keep_subtitle_mapped {
                log::info!(
                    "[subs] Gamescope detected; keeping the subtitle surface permanently mapped"
                );
            }

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

            // Shared memory for the transparent video anchor, background, and subtitles.
            let shm = if let Some(shm_global) = state
                .globals
                .iter()
                .find(|(_, interface, _)| interface == "wl_shm")
            {
                let shm: WlShm = registry.bind(shm_global.0, shm_global.2.min(1), &qh, ());
                log::debug!("Found and bound wl_shm for surface buffers");
                shm
            } else {
                return Err(Error::Wayland(
                    "No wl_shm found; cannot map the GStreamer video host surface".into(),
                ));
            };

            // ── Color management (optional) ──────────────────────────────
            // Bind only to detect compositor support. The surface passed to
            // `waylandsink` is a transparent mapping ancestor, not video content,
            // so it must remain untagged. GStreamer tags its nested video surface.
            let color_manager = ColorManager::bind_if_available(&state.globals, &registry, &qh);
            if color_manager.is_some() {
                event_queue.roundtrip(&mut state).map_err(|e| {
                    Error::Wayland(format!("Failed to roundtrip for color-mgmt: {e}"))
                })?;
            }

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

            // Video subsurface: desync so GStreamer's waylandsink can commit
            // frames independently on its streaming thread.
            video_subsurface.set_desync();

            // Subtitle subsurface: SYNC mode.  Changes are applied atomically
            // when the parent surface (iced) commits.  This prevents the green
            // flash on Hyprland's HDR CM pipeline — desync subtitle commits
            // cause the compositor to re-composite asynchronously, triggering
            // a transient color-management rendering glitch.  mpv uses the
            // same approach: all surfaces are committed together.
            subtitle_subsurface.set_sync();

            // Background: sync with parent (only changes on resize).
            background_subsurface.set_sync();
            log::debug!("Subsurface modes: video=desync, subtitle=sync, background=sync");

            // Z-ordering: video below parent, subtitle above parent
            video_subsurface.place_below(&parent_surface);

            // IMPORTANT: Put subtitles above the parent so they truly overlay video
            subtitle_subsurface.place_above(&parent_surface);

            background_subsurface.place_below(&video_surface);

            // `waylandsink` treats the supplied surface as a parent and renders into
            // nested subsurfaces. Wayland hides the entire subtree until every
            // subsurface ancestor is mapped, so retain a transparent buffer here.
            let (video_anchor_buffer, video_anchor_pool) =
                create_transparent_video_anchor(&shm, &qh)?;
            video_surface.attach(Some(&video_anchor_buffer), 0, 0);
            video_surface.damage_buffer(0, 0, VIDEO_ANCHOR_WIDTH, VIDEO_ANCHOR_HEIGHT);
            log::debug!(
                "Mapped GStreamer video host with an untagged transparent 1x1 anchor buffer"
            );
            // Keep the anchor at 1x1: subsurfaces are not clipped to their parent,
            // so GStreamer's nested surfaces can cover the full render rectangle
            // without turning this inert mapping buffer into a full-screen layer.

            // Commit children so the compositor can pick up the roles, ordering,
            // and video host mapping on the next parent commit.
            background_surface.commit();
            video_surface.commit();
            subtitle_surface.commit();

            // All subsurfaces default to (0, 0) relative to the parent. Position
            // updates are only needed for layout changes or picture-in-picture.

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
                dispatch_state: Mutex::new(state),
                compositor,
                video_subsurface,
                background_subsurface,
                video_surface,
                background_surface,
                subtitle_subsurface,
                subtitle_surface,
                background_viewport,
                subtitle_viewport,
                position: Arc::new(Mutex::new((0, 0))),
                size: Arc::new(Mutex::new((0, 0))),
                needs_update: Arc::new(AtomicBool::new(false)),
                shm: Some(shm),
                video_anchor_buffer,
                video_anchor_pool,
                background_buffer: Mutex::new(None),
                background_pool: Mutex::new(None),
                subtitle_buffers: Mutex::new(Vec::new()),
                subtitle_clear_buffers: Mutex::new(Vec::new()),
                subtitle_visible: AtomicBool::new(false),
                keep_subtitle_mapped,
                color_manager: Mutex::new(color_manager),
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
            let subsurface_clone = subsurface_manager.video_subsurface.clone();
            let background_subsurface_clone = subsurface_manager.background_subsurface.clone();
            let background_surface_clone = subsurface_manager.background_surface.clone();
            let background_viewport_clone = subsurface_manager.background_viewport.clone();
            let subtitle_subsurface_clone = subsurface_manager.subtitle_subsurface.clone();
            let subtitle_surface_clone = subsurface_manager.subtitle_surface.clone();
            let subtitle_viewport_clone = subsurface_manager.subtitle_viewport.clone();

            integration.register_pre_commit_hook(move || {
                // Check weak references and bail early if they're gone
                let (needs_update, position, size) = match (
                    needs_update_weak.upgrade(),
                    position_weak.upgrade(),
                    size_weak.upgrade(),
                ) {
                    (Some(n), Some(p), Some(s)) => (n, p, s),
                    _ => return, // Subsurface has been dropped, nothing to do
                };

                if needs_update.swap(false, Ordering::Relaxed) {
                    let (x, y) = *position.lock();
                    let (dest_w, dest_h) = *size.lock();

                    // Position changes take effect with the imminent parent commit.
                    subsurface_clone.set_position(x, y);
                    background_subsurface_clone.set_position(x, y);
                    subtitle_subsurface_clone.set_position(x, y);

                    if dest_w <= 0 || dest_h <= 0 {
                        log::debug!(
                            "Skipping non-positive subsurface viewport size {}x{}",
                            dest_w,
                            dest_h
                        );
                        return;
                    }

                    if let Some(ref bg_viewport) = background_viewport_clone {
                        bg_viewport.set_destination(dest_w, dest_h);
                        background_surface_clone.damage(0, 0, dest_w, dest_h);
                        background_surface_clone.commit();
                        log::debug!("Background viewport updated to {}x{}", dest_w, dest_h);
                    } else {
                        log::error!("No background viewport in pre-commit hook");
                    }

                    if let Some(ref sub_viewport) = subtitle_viewport_clone {
                        sub_viewport.set_destination(dest_w, dest_h);
                        subtitle_surface_clone.damage(0, 0, dest_w, dest_h);
                        subtitle_surface_clone.commit();
                        log::debug!("Subtitle viewport updated to {}x{}", dest_w, dest_h);
                    } else {
                        log::error!("No subtitle viewport in pre-commit hook");
                    }

                    // The transparent video host remains an immutable 1x1 mapping
                    // anchor. GStreamer sizes and tags its nested content surfaces.
                }
            });

            Ok(subsurface_manager)
        }
    }

    fn subtitle_buffer_size(stride: i32, height: i32) -> Result<usize> {
        if stride <= 0 || height <= 0 {
            return Err(Error::Wayland(format!(
                "Invalid subtitle buffer dimensions: stride={stride}, height={height}"
            )));
        }

        (stride as usize)
            .checked_mul(height as usize)
            .filter(|size| *size <= i32::MAX as usize)
            .ok_or_else(|| Error::Wayland("Subtitle buffer size overflow".into()))
    }

    fn create_subtitle_buffer<U>(
        &self,
        width: i32,
        height: i32,
        stride: i32,
        user_data: U,
        released: Arc<AtomicBool>,
    ) -> Result<SubtitleBufferSlot>
    where
        State: Dispatch<WlBuffer, U>,
        U: Send + Sync + 'static,
    {
        let needed = Self::subtitle_buffer_size(stride, height)?;
        let file =
            tempfile().map_err(|error| Error::Wayland(format!("subtitle tempfile: {error}")))?;
        file.set_len(needed as u64)
            .map_err(|error| Error::Wayland(format!("subtitle resize: {error}")))?;

        let qh = self.event_queue.lock().handle();
        let shm = self
            .shm
            .as_ref()
            .ok_or_else(|| Error::Wayland("No wl_shm for subtitle".into()))?;
        let pool = shm.create_pool(file.as_fd(), needed as i32, &qh, ());
        let buffer = pool.create_buffer(0, width, height, stride, Format::Argb8888, &qh, user_data);

        Ok(SubtitleBufferSlot {
            buffer,
            pool,
            file,
            dimensions: (width, height, stride),
            released,
        })
    }

    fn dispatch_pending_events(&self) -> Result<()> {
        let mut event_queue = self.event_queue.lock();
        let mut state = self.dispatch_state.lock();
        event_queue.dispatch_pending(&mut state).map_err(|error| {
            Error::Wayland(format!("Failed to dispatch Wayland events: {error}"))
        })?;
        Ok(())
    }

    fn roundtrip_events(&self) -> Result<()> {
        let mut event_queue = self.event_queue.lock();
        let mut state = self.dispatch_state.lock();
        event_queue
            .roundtrip(&mut state)
            .map_err(|error| Error::Wayland(format!("Failed Wayland roundtrip: {error}")))?;
        Ok(())
    }

    fn acquire_subtitle_frame_buffer(
        &self,
        data: &[u8],
        width: i32,
        height: i32,
        stride: i32,
    ) -> Result<WlBuffer> {
        let dimensions = (width, height, stride);
        let needed = Self::subtitle_buffer_size(stride, height)?;
        if data.len() < needed {
            return Err(Error::Wayland(format!(
                "Subtitle data too small: {} < {} ({}x{} stride={})",
                data.len(),
                needed,
                width,
                height,
                stride
            )));
        }

        self.dispatch_pending_events()?;

        for attempt in 0..2 {
            let mut slots = self.subtitle_buffers.lock();
            let matching = slots
                .iter()
                .position(|slot| slot.dimensions == dimensions && slot.is_released());
            let reusable =
                matching.or_else(|| slots.iter().position(SubtitleBufferSlot::is_released));

            let index = if let Some(index) = reusable {
                if slots[index].dimensions != dimensions {
                    let old = slots.swap_remove(index);
                    drop(old);
                    let released = Arc::new(AtomicBool::new(true));
                    let user_data = SubtitleBufferData {
                        released: Arc::clone(&released),
                    };
                    slots.push(
                        self.create_subtitle_buffer(width, height, stride, user_data, released)?,
                    );
                    slots.len() - 1
                } else {
                    index
                }
            } else if slots.len() < MAX_SUBTITLE_BUFFERS {
                let released = Arc::new(AtomicBool::new(true));
                let user_data = SubtitleBufferData {
                    released: Arc::clone(&released),
                };
                slots
                    .push(self.create_subtitle_buffer(width, height, stride, user_data, released)?);
                slots.len() - 1
            } else {
                drop(slots);
                if attempt == 0 {
                    // A roundtrip guarantees that release events for previously
                    // replaced buffers are delivered before dropping a cue.
                    self.roundtrip_events()?;
                    continue;
                }
                return Err(Error::Wayland(
                    "No released subtitle buffer available after Wayland roundtrip".into(),
                ));
            };

            let slot = &mut slots[index];
            slot.file
                .seek(SeekFrom::Start(0))
                .map_err(|error| Error::Wayland(format!("subtitle seek: {error}")))?;
            slot.file
                .write_all(&data[..needed])
                .map_err(|error| Error::Wayland(format!("subtitle write: {error}")))?;
            slot.file
                .flush()
                .map_err(|error| Error::Wayland(format!("subtitle flush: {error}")))?;
            slot.released.store(false, Ordering::Release);
            return Ok(slot.buffer.clone());
        }

        Err(Error::Wayland("Failed to acquire subtitle buffer".into()))
    }

    fn transparent_subtitle_buffer(
        &self,
        width: i32,
        height: i32,
        stride: i32,
    ) -> Result<WlBuffer> {
        let dimensions = (width, height, stride);
        let mut slots = self.subtitle_clear_buffers.lock();
        if let Some(slot) = slots.iter().find(|slot| slot.dimensions == dimensions) {
            return Ok(slot.buffer.clone());
        }

        // A newly extended tempfile reads as all-zero premultiplied ARGB, so it
        // is a fully transparent immutable buffer and never needs release-based
        // reuse tracking.
        let released = Arc::new(AtomicBool::new(true));
        slots.push(self.create_subtitle_buffer(width, height, stride, (), released)?);
        Ok(slots
            .last()
            .expect("just inserted subtitle buffer")
            .buffer
            .clone())
    }

    fn map_transparent_subtitle(&self, width: i32, height: i32) -> Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| Error::Wayland("Subtitle stride overflow".into()))?;
        let buffer = self.transparent_subtitle_buffer(width, height, stride)?;

        self.subtitle_surface.attach(Some(&buffer), 0, 0);
        self.subtitle_surface.damage_buffer(0, 0, width, height);
        self.subtitle_surface.commit();
        Ok(())
    }

    /// Attach a rendered ARGB32 subtitle frame to the subtitle surface and commit.
    pub fn attach_subtitle_frame(
        &self,
        data: &[u8],
        width: i32,
        height: i32,
        stride: i32,
    ) -> Result<()> {
        if width <= 0 || height <= 0 || stride <= 0 {
            return Ok(());
        }

        let buffer = self.acquire_subtitle_frame_buffer(data, width, height, stride)?;
        log::debug!(
            "[subs] Attaching release-safe subtitle buffer {}x{} stride={}",
            width,
            height,
            stride
        );
        self.subtitle_surface.attach(Some(&buffer), 0, 0);
        self.subtitle_surface.damage_buffer(0, 0, width, height);
        self.subtitle_surface.commit();
        self.subtitle_visible.store(true, Ordering::Release);
        Ok(())
    }

    /// Clear subtitles. Gamescope retains an immutable transparent buffer to
    /// avoid HDR plane transitions; other compositors keep the normal unmap path.
    pub fn clear_subtitle(&self) -> Result<()> {
        let (width, height) = self.get_size();
        log::debug!(
            "[subs] Clearing subtitle (stable_mapping={}, size={}x{})",
            self.keep_subtitle_mapped,
            width.max(1),
            height.max(1)
        );
        self.subtitle_visible.store(false, Ordering::Release);

        if self.keep_subtitle_mapped {
            if let Err(error) = self.map_transparent_subtitle(width, height) {
                // Never leave stale subtitle pixels visible if allocating the stable
                // clear buffer fails. Unmapping is a less stable but safe fallback.
                self.subtitle_surface.attach(None, 0, 0);
                self.subtitle_surface.commit();
                return Err(error);
            }
        } else {
            self.subtitle_surface.attach(None, 0, 0);
            self.subtitle_surface.commit();
        }

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
        let changed = {
            let mut size = self.size.lock();
            if *size == (w, h) {
                false
            } else {
                *size = (w, h);
                true
            }
        };
        if !changed {
            return;
        }

        log::info!("[subs] WaylandSubsurfaceManager::set_size -> {}x{}", w, h);
        self.needs_update.store(true, Ordering::Relaxed);

        // Map the transparent subtitle plane before the first cue and keep it
        // mapped between cues. If a cue is active, retain it and let the
        // viewport scale it until the next subtitle update.
        if self.keep_subtitle_mapped && !self.subtitle_visible.load(Ordering::Acquire) {
            if let Err(error) = self.map_transparent_subtitle(w, h) {
                log::warn!("[subs] Failed to map transparent subtitle buffer: {error}");
            }
        }
    }

    /// Get the current position
    pub fn get_position(&self) -> (i32, i32) {
        *self.position.lock()
    }

    /// Get the current size
    pub fn get_size(&self) -> (i32, i32) {
        *self.size.lock()
    }

    // Do we have use for this function?
    pub fn set_buffer_offset(&self, x: i32, y: i32) {
        self.video_surface.offset(x, y);

        // Mark the entire surface as damaged when size changes
        self.video_surface.damage_buffer(0, 0, x, y);
        self.video_surface.commit();
        log::debug!("Buffer offset changed to {}x{}, surface committed", x, y,);
    }

    /// Update the logical video area without scaling the intermediary host.
    ///
    /// The source rectangle is retained for API compatibility but ignored.
    /// GStreamer owns cropping and sizing on its nested content surfaces, while
    /// the host must remain an inert 1x1 mapping anchor.
    pub fn set_video_viewport(
        &self,
        source: Option<(i32, i32, i32, i32)>,
        dest: Option<(i32, i32)>,
    ) {
        if source.is_some() {
            log::debug!("Ignoring host viewport source; GStreamer owns video source cropping");
        }

        if let Some((width, height)) = dest {
            if width <= 0 || height <= 0 {
                log::warn!("Ignoring non-positive video viewport {width}x{height}");
            } else {
                self.set_size(width, height);
            }
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

    /// Returns `true` if the compositor supports `wp-color-management-v1`.
    pub fn has_color_management(&self) -> bool {
        self.color_manager.lock().is_some()
    }

    /// Returns whether Subwave tagged the intermediary host as HDR.
    ///
    /// This is always false: the host carries only a transparent mapping pixel.
    /// GStreamer's nested video surface owns the actual HDR image description.
    pub fn is_video_tagged_hdr(&self) -> bool {
        false
    }

    /// Observe video colorimetry without applying it to the mapping anchor.
    ///
    /// Retained for API compatibility. GStreamer 1.28+ applies this metadata to
    /// the nested surface that actually carries video pixels.
    pub fn notify_video_colorimetry(
        &self,
        colorimetry: &str,
        _metadata: Option<&crate::color_management::HdrMetadata>,
    ) {
        log::debug!("[color-mgmt] Delegating {colorimetry} to waylandsink; host remains untagged");
    }

    /// Dispatch queued events and flush pending Wayland requests.
    pub fn flush(&self) -> Result<()> {
        self.dispatch_pending_events()?;
        self.event_queue
            .lock()
            .flush()
            .map_err(|e| Error::Wayland(format!("Failed to flush events: {}", e)))?;
        Ok(())
    }

    /// Force the mutable overlay surfaces to redraw.
    ///
    /// The video host is intentionally omitted because its transparent 1x1
    /// mapping anchor is immutable after construction.
    pub fn force_damage_and_commit(&self) {
        self.background_surface.damage(0, 0, i32::MAX, i32::MAX);
        self.background_surface
            .damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.background_surface.commit();
        self.subtitle_surface.damage(0, 0, i32::MAX, i32::MAX);
        self.subtitle_surface
            .damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.subtitle_surface.commit();
        log::debug!("Forced full damage and commit on overlay surfaces");
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
        self.video_anchor_buffer.destroy();
        self.video_anchor_pool.destroy();
        if let Some(buffer) = self.background_buffer.lock().take() {
            buffer.destroy();
        }
        if let Some(pool) = self.background_pool.lock().take() {
            pool.destroy();
        }
        self.subtitle_buffers.lock().clear();
        self.subtitle_clear_buffers.lock().clear();

        // Destroy color management resources before surfaces
        if let Some(mut cm) = self.color_manager.lock().take() {
            cm.destroy();
        }

        // Destroy viewports if they exist
        if let Some(ref viewport) = self.background_viewport {
            viewport.destroy();
        }
        if let Some(ref viewport) = self.subtitle_viewport {
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
            // Immutable anchors/backgrounds do not need release-based reuse.
            log::debug!("Immutable Wayland buffer released by compositor");
        }
    }
}

impl Dispatch<WlBuffer, SubtitleBufferData> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        event: <WlBuffer as Proxy>::Event,
        data: &SubtitleBufferData,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_buffer::Event;
        if let Event::Release = event {
            data.released.store(true, Ordering::Release);
            log::debug!("Subtitle buffer released by compositor");
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

#[cfg(test)]
mod tests {
    use super::is_gamescope_compositor;

    #[test]
    fn detects_gamescope_specific_globals() {
        let gamescope_globals = vec![
            (1, "wl_compositor".to_string(), 6),
            (2, "gamescope_control".to_string(), 6),
        ];
        let generic_globals = vec![
            (1, "wl_compositor".to_string(), 6),
            (2, "wp_color_manager_v1".to_string(), 1),
        ];

        assert!(is_gamescope_compositor(&gamescope_globals));
        assert!(!is_gamescope_compositor(&generic_globals));
    }
}
