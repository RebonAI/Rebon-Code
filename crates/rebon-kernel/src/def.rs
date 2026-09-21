//! Assembly units and lifecycle events.
//!
//! A [`Plugin`](crate::Plugin) is one loaded instance; a [`PluginDef`] is the
//! thing that can make one — again after an unload, or never when its config
//! switch is off. A definition is `'static` data: a stable id, a kind, a
//! default, and a factory. Built-ins are listed once per binary and handed to
//! the [`PluginRegistry`](crate::PluginRegistry); external plugins arrive as
//! definitions too, so a single table and a single set of switches covers
//! both.
//!
//! The lifecycle events here are the seams a plugin subscribes to instead of
//! reaching into a host: a session opening (so the plugin can fork under it),
//! config files changing (so it can re-read its namespace), and a sibling
//! plugin changing state.

use std::path::PathBuf;
use std::sync::Arc;

use crate::{Context, Kernel, KernelError, Plugin};

/// What a plugin is to the host, which decides what the host may do to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    /// Part of the kernel itself. Always loaded; a request to disable it is
    /// refused, and a failure to load it fails the process.
    Core,
    /// Built-in behaviour behind a config switch. Loadable, unloadable,
    /// reloadable.
    Feature,
    /// Hosted out of process (a Node package) and proxied in by the host
    /// plugin. Same switches, same table.
    External,
}

/// What a factory gets. Deliberately narrow: the kernel to register against
/// and the config directory to read from. A plugin that needs more resolves
/// a service through its context.
#[derive(Clone)]
pub struct PluginHost {
    pub kernel: Arc<Kernel>,
    pub config_dir: PathBuf,
}

/// A plugin that can be instantiated on demand.
///
/// `factory` runs on every load and every reload; the previous instance is
/// gone by then, disposed with its scope. The instance's
/// [`PluginMeta::name`](crate::PluginMeta::name) must equal `id` — the
/// registry refuses one that does not, because the id is also the config
/// key and the name users see.
#[derive(Clone, Copy)]
pub struct PluginDef {
    pub id: &'static str,
    pub title: &'static str,
    pub kind: PluginKind,
    pub default_enabled: bool,
    pub factory: fn(&PluginHost) -> Result<Box<dyn Plugin>, KernelError>,
}

impl std::fmt::Debug for PluginDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginDef")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("default_enabled", &self.default_enabled)
            .finish_non_exhaustive()
    }
}

/// The factory half of a [`DynPluginDef`].
///
/// A closure can satisfy this where a `fn` pointer cannot: an external
/// definition's factory has to remember *which* package it is for.
pub type PluginFactory =
    Arc<dyn Fn(&PluginHost) -> Result<Box<dyn Plugin>, KernelError> + Send + Sync>;

/// A definition the binary could not have listed.
///
/// [`PluginDef`] is `'static` data because a built-in is known at compile
/// time. An external plugin is not: its id names a package installed on this
/// machine, and its factory has to carry that name. Same four fields, owned,
/// with a closure for a factory — and the registry treats the two alike, so
/// one table and one set of switches still covers both.
#[derive(Clone)]
pub struct DynPluginDef {
    pub id: String,
    pub title: String,
    pub kind: PluginKind,
    pub default_enabled: bool,
    pub factory: PluginFactory,
}

impl DynPluginDef {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        kind: PluginKind,
        default_enabled: bool,
        factory: impl Fn(&PluginHost) -> Result<Box<dyn Plugin>, KernelError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            kind,
            default_enabled,
            factory: Arc::new(factory),
        }
    }
}

impl From<&PluginDef> for DynPluginDef {
    fn from(def: &PluginDef) -> Self {
        let factory = def.factory;
        Self {
            id: def.id.to_string(),
            title: def.title.to_string(),
            kind: def.kind,
            default_enabled: def.default_enabled,
            factory: Arc::new(move |host| factory(host)),
        }
    }
}

impl std::fmt::Debug for DynPluginDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynPluginDef")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("default_enabled", &self.default_enabled)
            .finish_non_exhaustive()
    }
}

/// Where a plugin stands in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    /// Not loaded because its switch is off (or a provider it needs is).
    Disabled,
    Loaded,
    /// Its factory or `apply` failed; the message is what it said. Retried
    /// on the next reconcile.
    Failed(String),
}

/// Emitted on the kernel root after a plugin moves between states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStateChanged {
    pub id: String,
    pub from: PluginState,
    pub to: PluginState,
    /// The reconcile that made the change; monotone per registry.
    pub generation: u64,
}

/// Which on-disk file a [`ConfigChanged`] event is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFileKind {
    /// `config.json` — machine state and provider entries.
    Config,
    /// `settings.json` at any level.
    Settings,
    /// `credentials.json`.
    Credentials,
}

/// A config file was rewritten by this process. Plugins re-read the part
/// they own; the registry reconciles switches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChanged {
    pub kind: ConfigFileKind,
    pub path: PathBuf,
    /// Which plugin's `plugins.<id>` namespace changed, when the write went
    /// through the settings seat and touched exactly one.
    ///
    /// A subscriber filters on its own id and re-reads nothing when another
    /// plugin's keys move. `None` means "some part of this file changed" —
    /// a hand edit, a switch flip, anything not written key-by-key through
    /// the seat — and a plugin that cares has to re-read.
    pub namespace: Option<String>,
}

/// A session scope was forked under the kernel root. A session-aware
/// plugin forks under `ctx` and registers there; the fork goes with the
/// session, and with the plugin.
#[derive(Clone)]
pub struct SessionOpened {
    pub session_id: String,
    pub ctx: Context,
}

impl std::fmt::Debug for SessionOpened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionOpened")
            .field("session_id", &self.session_id)
            .field("scope", &self.ctx.label())
            .finish()
    }
}

/// The session's scope is being disposed. Registrations under it are
/// already on their way out; this is for plugins that keep state keyed by
/// session id elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionClosed {
    pub session_id: String,
}
