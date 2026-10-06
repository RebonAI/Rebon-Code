//! The `container-sandbox` seat: confining a plugin container's host.
//!
//! A container is an installed plugin running in a Node host of its own
//! (`rebon-plugin-host`'s `container` module). Node's permission model keeps
//! it to its own files; what Node cannot do is cut the network, and that is
//! this seat's job: the sandbox plugin answers with a proxy admitting exactly
//! the hosts the plugin declared and, where the OS sandbox is usable, a
//! launcher the host runs under with every other connection refused.
//!
//! One provider, as with [`crate::SessionSandboxService`]: a security
//! decision must not depend on which of two providers registered last.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

/// Stable typed name of the seat.
pub const CONTAINER_SANDBOX_SERVICE: &str = "container-sandbox";

/// What a container asks of the OS layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfineRequest {
    /// The container's name, for notes and the proxy's log tag.
    pub container: String,
    /// Node itself, which the host must be allowed to execute.
    pub node: PathBuf,
    /// The host's whole command line, Node first: what the OS layer wraps.
    pub argv: Vec<OsString>,
    /// The environment the host runs with, before the proxy's variables.
    /// An OS layer that does not pass its own environment through (Windows
    /// runs the host as another account) hands these over itself.
    pub environment: Vec<(OsString, OsString)>,
    /// Everything the host reads.
    pub read: Vec<PathBuf>,
    /// The one directory it writes.
    pub write: PathBuf,
    /// The hosts it may reach; empty means none.
    pub network: Vec<String>,
}

/// The host's command line as the OS sandbox runs it: the complete argv,
/// not a prefix — a backend may fold the command into a shell line of its
/// own (bwrap's network bridge does).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfinedLauncher {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env_set: Vec<(OsString, OsString)>,
    pub env_unset: Vec<OsString>,
    /// Which backend did it, for the note a container starts with.
    pub backend: String,
}

/// What the OS layer adds around one container's host.
#[derive(Default)]
pub struct ContainerConfinement {
    /// The OS sandbox the host runs under, when one is usable.
    pub launcher: Option<ConfinedLauncher>,
    /// Variables the host needs to reach the network through the proxy.
    pub environment: Vec<(OsString, OsString)>,
    /// What must live as long as the host (the proxy).
    pub keep_alive: Option<Arc<dyn std::any::Any + Send + Sync>>,
    /// What could not be applied, said where the container starts.
    pub notes: Vec<String>,
}

impl std::fmt::Debug for ContainerConfinement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerConfinement")
            .field("launcher", &self.launcher)
            .field("environment", &self.environment)
            .field("keep_alive", &self.keep_alive.is_some())
            .field("notes", &self.notes)
            .finish()
    }
}

/// The provider behind the seat.
pub trait ContainerSandboxSource: Send + Sync {
    /// Confines one container's host, or says why it cannot be started at all.
    ///
    /// An unusable OS sandbox is not an error — the host still runs under
    /// Node's permission model, and the confinement says what is missing in
    /// `notes`. An error is a container that must not start: hosts declared
    /// and no proxy to admit them through.
    fn confine(&self, request: &ConfineRequest) -> Result<ContainerConfinement, String>;
}

/// Typed definition for the kernel's `container-sandbox` seat.
pub struct ContainerSandboxService;

impl rebon_kernel::Service for ContainerSandboxService {
    type Interface = dyn ContainerSandboxSource;
    const NAME: &'static str = CONTAINER_SANDBOX_SERVICE;
}
