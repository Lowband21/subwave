use parking_lot::Mutex;
use std::sync::Arc;

type PreCommitHook = Box<dyn Fn() + Send + Sync>;

/// Integration point for exposing Wayland handles from iced
/// This struct will be provided by the modified iced fork
#[derive(Clone)]
pub struct WaylandIntegration {
    /// Raw pointer to the parent Wayland surface
    pub surface: *mut std::ffi::c_void,

    /// Raw pointer to the Wayland display connection
    pub display: *mut std::ffi::c_void,

    /// Callback invoked before the parent surface commits
    /// Used to synchronize subsurface position updates
    pub pre_commit_hooks: Arc<Mutex<Vec<PreCommitHook>>>,
}

impl WaylandIntegration {
    /// Create a new WaylandIntegration with the given handles
    pub fn new(surface: *mut std::ffi::c_void, display: *mut std::ffi::c_void) -> Self {
        Self {
            surface,
            display,
            pre_commit_hooks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a callback to be invoked before the parent surface commits
    pub fn register_pre_commit_hook(&self, hook: impl Fn() + Send + Sync + 'static) {
        self.pre_commit_hooks.lock().push(Box::new(hook));
    }

    /// Invoke all registered pre-commit hooks
    /// This will be called by the iced fork before committing the parent surface
    pub fn trigger_pre_commit_hooks(&self) {
        for hook in self.pre_commit_hooks.lock().iter() {
            hook();
        }
    }

    /// Clear all registered pre-commit hooks
    /// Call this during cleanup to break reference cycles
    pub fn clear_pre_commit_hooks(&self) {
        self.pre_commit_hooks.lock().clear();
    }
}
