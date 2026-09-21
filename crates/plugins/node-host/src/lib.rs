//! `node-host`: the Node plugin plane as one kernel plugin.
//!
//! Everything a third party writes in JavaScript reaches rebon through a
//! single host process. This plugin owns that process's lifetime: the plane
//! runs because `node-host` is loaded and stops because `node-host` is
//! unloaded, so the decision lives in the registry rather than in whichever
//! binary happened to assemble a session first.
//!
//! # The two halves, and why the seam is here
//!
//! Starting a Node process, speaking the plugin protocol and wrapping what a
//! package reports into rebon's tools needs the engine, and lives outside this
//! crate, behind [`PlaneHost`]. What lives here is the part that has nothing
//! to do with Node: which entries should be running, who turned one off, and
//! what happens to them when the plugin goes away. Keeping the engine out of
//! it is what lets this be a plugin crate whose only dependency is the kernel.
//!
//! # One list, one set of switches
//!
//! An installed package that declares `capabilities.kernelPlugins` is not a
//! second kind of thing with a second kind of switch. It becomes a
//! [`DynPluginDef`] of kind [`PluginKind::External`] with the id
//! `node:<entry>`, sits in the same registry table as the built-in feature
//! plugins, and answers `plugins.node:<entry>.enabled` the same way. Loading
//! it puts its entry in the wanted set and settles the plane; unloading it
//! takes the entry out and settles again — and a settle is a diff, so an entry
//! whose configuration did not change keeps running.
//!
//! Turning `node-host` itself off takes every `node:*` with it: they declare
//! its service as a required `inject`, so the kernel's dependency gate
//! cascades, and the registry's report says which ones went.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use rebon_kernel::{
    Context, Disposer, DynPluginDef, KernelError, Plugin, PluginDef, PluginHost, PluginKind,
    PluginMeta, Service,
};

/// The registry id and the settings key: `plugins.node-host.enabled`.
pub const PLUGIN_ID: &str = "node-host";

/// The kernel service name the external rows inject.
pub const NODE_HOST_SERVICE: &str = "node-host";

/// The prefix an external plugin's id carries, so a package can never be
/// mistaken for a built-in and a built-in can never be addressed as a package.
pub const EXTERNAL_ID_PREFIX: &str = "node:";

/// One entry the plane could run, as the host side sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableEntry {
    /// The composition entry id. The registry row is `node:<id>`.
    pub id: String,
    /// What a person reads in `/kernel plugins` and the settings list.
    pub title: String,
    /// Whether the configuration lists it today.
    ///
    /// This is the switch's default, and it is what makes turning the plane
    /// into a registry a no-op for everyone: the set that loads by itself is
    /// exactly the set `kernelPlugins` already named.
    pub configured: bool,
}

/// The host half of the plane: what this plugin cannot do without the engine.
///
/// Every method is called with no kernel lock held, and may block — a
/// reconcile is worth as much as the plugins it starts and stops, and the
/// caller flipping a switch is waiting for the answer.
pub trait PlaneHost: Send + Sync + 'static {
    /// Every entry this machine offers: the ones the configuration names and
    /// the ones installed packages declare.
    fn available(&self) -> Vec<AvailableEntry>;

    /// Bring the running composition in line with `wanted`, which holds
    /// [`AvailableEntry::id`] values. A plane that has not been started yet
    /// records the set and starts with it.
    fn reconcile(&self, wanted: &BTreeSet<String>) -> Result<(), String>;

    /// Wind the composition and its host process down.
    fn shutdown(&self);
}

/// The service `node:*` plugins resolve to say what they want running.
pub struct NodeHostService;

impl Service for NodeHostService {
    type Interface = NodeHost;
    const NAME: &'static str = NODE_HOST_SERVICE;
}

/// The wanted set, and the plane it settles onto.
pub struct NodeHost {
    host: Arc<dyn PlaneHost>,
    wanted: Mutex<BTreeSet<String>>,
}

impl NodeHost {
    fn new(host: Arc<dyn PlaneHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            wanted: Mutex::new(BTreeSet::new()),
        })
    }

    /// What this machine offers, whether or not it is switched on.
    pub fn available(&self) -> Vec<AvailableEntry> {
        self.host.available()
    }

    /// The entries currently asked for.
    pub fn wanted(&self) -> BTreeSet<String> {
        self.wanted.lock().expect("node-host wanted set").clone()
    }

    /// Ask for one entry. Idempotent; settling is a separate step so a batch
    /// of external plugins loading together costs one reconcile, not one each.
    pub fn want(&self, entry: &str) {
        self.wanted
            .lock()
            .expect("node-host wanted set")
            .insert(entry.to_string());
    }

    /// Stop asking for one entry.
    pub fn unwant(&self, entry: &str) {
        self.wanted
            .lock()
            .expect("node-host wanted set")
            .remove(entry);
    }

    /// Bring the plane in line with the wanted set.
    pub fn settle(&self) -> Result<(), String> {
        let wanted = self.wanted();
        self.host.reconcile(&wanted)
    }
}

/// The plugin itself: it owns the plane's lifetime and nothing else.
pub struct NodeHostPlugin {
    host: Arc<dyn PlaneHost>,
}

impl NodeHostPlugin {
    pub fn new(host: Arc<dyn PlaneHost>) -> Self {
        Self { host }
    }
}

impl Plugin for NodeHostPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).provides(&[NODE_HOST_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let node_host = NodeHost::new(Arc::clone(&self.host));
        ctx.provide::<NodeHostService>(node_host)?;
        // The plane goes down with the plugin, whichever way the plugin goes
        // down: a switch flipped, a dependency cascade, the process ending.
        let host = Arc::clone(&self.host);
        ctx.effect_labeled("node plugin plane", move || {
            Disposer::new(move || host.shutdown())
        });
        Ok(())
    }
}

/// One external package's row.
///
/// It holds no state of its own. Being loaded *is* the statement that its
/// entry should run, and the effect it leaves behind is the statement that it
/// should stop — which is why a `node:*` plugin needs no unload protocol
/// beyond the one every plugin already has.
struct ExternalNodePlugin {
    id: String,
    entry: String,
}

impl Plugin for ExternalNodePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(self.id.clone()).inject(&[NODE_HOST_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let host = ctx.require::<NodeHostService>()?;
        host.want(&self.entry);
        {
            let host = Arc::clone(&host);
            let entry = self.entry.clone();
            ctx.effect_labeled(&format!("node entry({entry})"), move || {
                Disposer::new(move || {
                    host.unwant(&entry);
                    if let Err(error) = host.settle() {
                        tracing::warn!(%entry, %error, "node-host: unloading an entry failed");
                    }
                })
            });
        }
        host.settle().map_err(KernelError::Other)
    }
}

/// The registry row for one entry.
pub fn external_id(entry: &str) -> String {
    format!("{EXTERNAL_ID_PREFIX}{entry}")
}

/// Turn what the plane offers into registry definitions.
///
/// Handed whole to
/// [`PluginRegistry::set_external_defs`](rebon_kernel::PluginRegistry::set_external_defs):
/// "what this machine offers" is a fact that is re-read whole, and a set
/// difference is a safer thing to compute in one place than an add and a
/// remove computed in two.
pub fn external_plugin_defs(entries: &[AvailableEntry]) -> Vec<DynPluginDef> {
    entries
        .iter()
        .map(|entry| {
            let id = external_id(&entry.id);
            let plugin_id = id.clone();
            let module = entry.id.clone();
            DynPluginDef::new(
                id,
                entry.title.clone(),
                PluginKind::External,
                entry.configured,
                move |_host| {
                    Ok(Box::new(ExternalNodePlugin {
                        id: plugin_id.clone(),
                        entry: module.clone(),
                    }) as Box<dyn Plugin>)
                },
            )
        })
        .collect()
}

/// Build the definition for a binary's plugin list.
///
/// A `fn` pointer cannot carry a [`PlaneHost`], so the host side keeps its own
/// `factory` and calls this to make the plugin; the `PluginDef` itself is
/// still `'static` data, listed once beside the other built-ins.
pub const fn plugin_def(
    factory: fn(&PluginHost) -> Result<Box<dyn Plugin>, KernelError>,
) -> PluginDef {
    PluginDef {
        id: PLUGIN_ID,
        title: "Node plugin host (external plugins)",
        kind: PluginKind::Feature,
        // The plane has always started itself when a composition was
        // configured, and started nothing when one was not. Loading by
        // default keeps both halves of that true: with no entries wanted,
        // `reconcile` has nothing to start.
        default_enabled: true,
        factory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry, PluginState};

    #[derive(Default)]
    struct RecordingPlane {
        available: Vec<AvailableEntry>,
        settled: Mutex<Vec<BTreeSet<String>>>,
        shutdowns: Mutex<usize>,
    }

    impl PlaneHost for Arc<RecordingPlane> {
        fn available(&self) -> Vec<AvailableEntry> {
            self.available.clone()
        }
        fn reconcile(&self, wanted: &BTreeSet<String>) -> Result<(), String> {
            self.settled.lock().unwrap().push(wanted.clone());
            Ok(())
        }
        fn shutdown(&self) {
            *self.shutdowns.lock().unwrap() += 1;
        }
    }

    fn entry(id: &str, configured: bool) -> AvailableEntry {
        AvailableEntry {
            id: id.to_string(),
            title: id.to_string(),
            configured,
        }
    }

    /// The definition's factory is a `fn` pointer, which cannot carry a test's
    /// plane; a slot plus a lock is what stands in, and holding the lock for
    /// the whole test is what keeps two of them from swapping planes under
    /// each other.
    static PLANE: Mutex<Option<Arc<RecordingPlane>>> = Mutex::new(None);
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn boot(plane: Arc<RecordingPlane>) -> (Arc<Kernel>, Arc<PluginRegistry>) {
        *PLANE.lock().unwrap() = Some(plane);
        fn factory(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
            let plane = PLANE.lock().unwrap().clone().expect("plane installed");
            Ok(Box::new(NodeHostPlugin::new(Arc::new(plane))))
        }
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let defs = [plugin_def(factory)];
        let registry = PluginRegistry::new(kernel.clone(), &defs, host);
        registry.reconcile(&DesiredSet::new());
        (kernel, registry)
    }

    fn state(registry: &PluginRegistry, id: &str) -> PluginState {
        registry
            .snapshot()
            .into_iter()
            .find(|row| row.id == id)
            .map(|row| row.state)
            .unwrap_or(PluginState::Disabled)
    }

    /// The set that loads by itself is the set the configuration already
    /// named — an installed package nobody configured stays off.
    #[test]
    fn configured_entries_load_and_unconfigured_ones_wait_for_a_switch() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let plane = Arc::new(RecordingPlane {
            available: vec![entry("tool-todo", true), entry("acme-widgets", false)],
            ..Default::default()
        });
        let (_kernel, registry) = boot(Arc::clone(&plane));
        registry.set_external_defs(external_plugin_defs(&plane.available));

        assert_eq!(state(&registry, "node:tool-todo"), PluginState::Loaded);
        assert_eq!(state(&registry, "node:acme-widgets"), PluginState::Disabled);
        let settled = plane.settled.lock().unwrap().clone();
        assert_eq!(
            settled.last().expect("the plane was settled"),
            &BTreeSet::from(["tool-todo".to_string()])
        );

        registry
            .set_enabled("node:acme-widgets", true)
            .expect("an external row is switchable");
        let settled = plane.settled.lock().unwrap().clone();
        assert_eq!(
            settled.last().unwrap(),
            &BTreeSet::from(["acme-widgets".to_string(), "tool-todo".to_string()])
        );

        registry
            .set_enabled("node:tool-todo", false)
            .expect("switchable both ways");
        let settled = plane.settled.lock().unwrap().clone();
        assert_eq!(
            settled.last().unwrap(),
            &BTreeSet::from(["acme-widgets".to_string()])
        );
    }

    /// Turning the host off takes the packages with it and stops the process.
    #[test]
    fn disabling_the_host_cascades_to_every_external_row() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let plane = Arc::new(RecordingPlane {
            available: vec![entry("tool-todo", true)],
            ..Default::default()
        });
        let (_kernel, registry) = boot(Arc::clone(&plane));
        registry.set_external_defs(external_plugin_defs(&plane.available));
        assert_eq!(state(&registry, "node:tool-todo"), PluginState::Loaded);

        let report = registry
            .set_enabled(PLUGIN_ID, false)
            .expect("the host is a Feature plugin");
        assert!(
            report.unloaded.contains(&"node:tool-todo".to_string()),
            "{report:?}"
        );
        assert_eq!(state(&registry, "node:tool-todo"), PluginState::Disabled);
        assert_eq!(*plane.shutdowns.lock().unwrap(), 1);

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert_eq!(state(&registry, "node:tool-todo"), PluginState::Loaded);
    }
}
