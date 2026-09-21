use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rebon_core::permission::KernelContextLeaseResolver;
use rebon_kernel::{Context, KernelError, PluginState, PluginStateChanged, Service};

use crate::runtime::TaskRegistry;
use crate::PLUGIN_ID;

/// Stable typed service name for one session's task runtime.
pub const TASK_REGISTRY_SERVICE: &str = "task-registry";

/// The typed service definition resolved from an exact session [`Context`].
pub struct TaskRegistryService;

impl Service for TaskRegistryService {
    type Interface = TaskRegistrySeat;
    const NAME: &'static str = TASK_REGISTRY_SERVICE;
}

/// Session binding for a [`TaskRegistry`].
///
/// The state Arc belongs to the session scope rather than to one load of the
/// feature plugin. Disabling `tasks` closes new access while already-running
/// holders finish with ordinary Rust Arc semantics; re-enabling opens this
/// same binding instead of allocating a replacement registry.
pub struct TaskRegistrySeat {
    registry: Arc<TaskRegistry>,
    available: AtomicBool,
}

impl TaskRegistrySeat {
    fn new(registry: Arc<TaskRegistry>) -> Arc<Self> {
        Arc::new(Self {
            registry,
            available: AtomicBool::new(false),
        })
    }

    pub(crate) fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Release);
    }

    /// Resolve the registry for a new consumer. Plugin absence is a hard
    /// failure; callers must not manufacture a fallback registry.
    pub fn registry(&self) -> Result<Arc<TaskRegistry>, KernelError> {
        if !self.available.load(Ordering::Acquire) {
            return Err(KernelError::Other(
                "task-registry is unavailable because the tasks plugin is not loaded".into(),
            ));
        }
        Ok(self.registry.clone())
    }

    /// Host ownership handle. This does not grant plugin consumption: it
    /// exists so a session host can retain state while the feature is disabled
    /// and bind the same Arc again when replacing a scope generation.
    pub fn host_registry(&self) -> Arc<TaskRegistry> {
        self.registry.clone()
    }
}

/// Bind the one registry chosen by the session host before `SessionOpened`.
///
/// The binding listens to plugin state changes on its own session scope, so a
/// loaded plugin can be disabled and re-enabled without dropping task state or
/// exposing a fallback registry while unavailable.
pub fn provide_task_registry(
    ctx: &Context,
    registry: Arc<TaskRegistry>,
) -> Result<Arc<TaskRegistrySeat>, KernelError> {
    let seat = TaskRegistrySeat::new(registry);
    ctx.provide::<TaskRegistryService>(seat.clone())?;

    let watched = seat.clone();
    ctx.on::<PluginStateChanged>(move |event| {
        if event.id == PLUGIN_ID {
            watched.set_available(matches!(event.to, PluginState::Loaded));
        }
    });
    Ok(seat)
}

/// Resolve the available registry from this exact session context.
pub fn require_task_registry(ctx: &Context) -> Result<Arc<TaskRegistry>, KernelError> {
    ctx.require::<TaskRegistryService>()?.registry()
}

/// Session-id resolver used at non-kernel execution boundaries.
///
/// It acquires the exact scope generation, resolves `task-registry` there, and
/// returns only the registry Arc. A missing/disposed/disabled scope is an error,
/// never a request to create another registry.
#[derive(Clone)]
pub struct TaskRegistryResolver {
    kernel: KernelContextLeaseResolver,
}

impl fmt::Debug for TaskRegistryResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskRegistryResolver")
            .finish_non_exhaustive()
    }
}

impl TaskRegistryResolver {
    pub fn new(kernel: KernelContextLeaseResolver) -> Self {
        Self { kernel }
    }

    pub fn resolve(&self, session_id: &str) -> Result<Arc<TaskRegistry>, String> {
        if session_id.trim().is_empty() {
            return Err("task-registry resolution requires a non-empty session id".into());
        }
        let lease = (self.kernel)(session_id)
            .ok_or_else(|| format!("no live kernel scope for session `{session_id}`"))?;
        require_task_registry(lease.context()).map_err(|error| error.to_string())
    }
}
