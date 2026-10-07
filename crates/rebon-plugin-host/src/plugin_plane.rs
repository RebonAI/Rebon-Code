//! The composition, on the plugin plane.
//!
//! This is the embedder half of what `runtimes/node/compose-runtime` does on the other
//! side of the pipe: rebon starts a plugin host, loads the composition control
//! plugin, then loads every configured entry as its own `plugin/load` — and
//! turns what each load reports into registrations on rebon's own seats.
//!
//! # Why one load per entry
//!
//! A composition cannot register into rebon from the inside, because the
//! plane is across a process boundary: registering that way needs synchronous
//! calls, Cordis puts unregistration in disposers, disposers cannot await, and
//! an answer cannot be had without waiting. So the direction is inverted. A
//! load *reports* what an entry provides, rebon registers it here, and unload
//! is what withdraws it — which is also what makes single-plugin unload,
//! generation and reload protocol operations rather than a control service the
//! composition serves itself.
//!
//! # What the plane exposes, and to whom
//!
//! Three seams point back into rebon, and each is gated before any embedder
//! code runs — the plugin's manifest first, this build's exposed set second:
//!
//! * [`SeatDispatcher`] — kernel seats (`credentials`, `logger`, `settings`,
//!   …), answered straight off the JSON plane.
//! * [`ToolInvoker`] — rebon's own tools, through the same permission broker a
//!   session uses. A plugin never gets a shortcut around it.
//! * [`EventPublisher`] — the kernel event plane. The identity riding an event
//!   is injected by the host, so attribution is not something a plugin reports
//!   about itself.
//!
//! # What rebon registers on the way back
//!
//! * tools → the process `tool-registry` seat, dispatching through `tool/call`;
//! * model routes → the `model-router` seat, plus a stream host per provider so
//!   `llm/stream` serves the ordinary model client path;
//! * prompt sections → the `system-prompt` seat.
//!
//! None of those seats change. That is the point: the composition's runtime
//! moved, and the kernel's view of what a composition provides did not.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rebon_command_seat::{
    Category, CommandArgs, CommandHandler, CommandKind, CommandSeatService, CommandSpec, Surface,
    Surfaces,
};
use rebon_core::tool_seat::{Priority, ToolSeatService};
use rebon_kernel::{
    Context, JsonService, KernelError, PluginLifecycleChanged, SharedLifecycleSink,
};
use rebon_plugin_protocol::{
    CommandInvokeRequest, Payload, PluginCommandDefinition, PluginCommandKind,
    PluginCommandSurface, PluginLoadRequest, PluginReadyReport, RegistryError,
};
use rebon_plugin_supervisor::{
    whole_seconds, EventPublisher, HostCallError, HostConfig, PluginHostSupervisor, PublishedEvent,
    SeatDispatcher, SeatInvocation, SupervisorError, ToolInvoker, ToolRefusal,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use rebon_kernel_seats::kernel_compose_tools::{ComposeToolDispatch, ComposeToolRegistry};
use rebon_provider::kernel_llm_dispatch::{register_llm_host, unregister_llm_host, LlmRoute};

/// The reserved plugin id of the composition control plugin.
pub const COMPOSE_PLUGIN_ID: &str = "rebon:compose";

/// Wall-clock budget for the control plugin's own report call.
const REPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// The scope rebon's own dealings with a composition happen on, by default.
///
/// A scoped call needs an open scope, and the plane has work that belongs to no
/// user session: asking an entry what it registered, running a composition tool
/// on rebon's behalf, streaming a model turn for a caller who has no session of
/// its own. So every plugin gets one when it loads.
///
/// A plane whose whole reason for existing *is* one session names it after that
/// session instead — a per-session agent loop does — so that what its plugins
/// publish is stamped with a scope that says whose it is. The kernel's event
/// plane is process-wide, and two loops in one rebon process would otherwise be
/// indistinguishable to a subscriber.
const DEFAULT_PLANE_SCOPE: &str = "rebon:plane";

/// One entry of the composition, as rebon loads it.
///
/// `root` and `entry` are a package: for a vendored dsh plugin, the payload
/// directory and the module inside it. `config` is the entry's own
/// configuration — the dsh `config` block — carried by the load rather than
/// living in the package, because the same package is loaded with different
/// configuration by different installations.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ComposeEntry {
    pub id: String,
    pub root: String,
    pub entry: String,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub event_topics: Vec<String>,
    #[serde(default)]
    pub published_topics: Vec<String>,
    #[serde(default)]
    pub llm_providers: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub invokable_tools: Vec<String>,
    #[serde(default)]
    pub seats: Vec<String>,
    /// Settings keys the entry owns under `plugins.<id>`, from its manifest.
    ///
    /// Declared on the `settings` seat before the module is loaded, because a
    /// plugin may read its own settings inside `activate` and a declaration
    /// that arrives afterwards is one its defaults were missing from.
    #[serde(default)]
    pub settings: Vec<rebon_types::KernelPluginSettingKey>,
    /// Whether what this entry registers becomes rebon's, or stays the
    /// composition's.
    ///
    /// The successor to the old host's `announce: false`. A per-session loop's
    /// entries provide routes and tools for that loop alone; publishing them
    /// would put a session's private registration in a process-wide table, and
    /// tearing the loop down would then remove something another session is
    /// still served by. Reporting still happens either way — rebon always knows
    /// what a plugin registered; `publish` is only about what it does with it.
    #[serde(default = "publish_by_default")]
    pub publish: bool,
    /// The container this entry runs in, when it does not run in the shared
    /// host: someone else's code, confined (see [`crate::container`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<crate::container::ContainerSpec>,
    /// Where the entry's package came from, as its installer recorded it: an
    /// opaque key, stamped on every run of the entry as
    /// [`PluginIncarnation::source`]. `None` for anything with no install
    /// record — a package rebon ships, a mod, an explicit module path.
    ///
    /// Part of the entry's equality, so a package whose source was replaced
    /// is restarted by a reload rather than kept running as the old one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

fn publish_by_default() -> bool {
    true
}

impl Default for ComposeEntry {
    fn default() -> Self {
        Self {
            id: String::new(),
            root: String::new(),
            entry: String::new(),
            config: Value::Null,
            services: Vec::new(),
            event_topics: Vec::new(),
            published_topics: Vec::new(),
            llm_providers: Vec::new(),
            tools: Vec::new(),
            commands: Vec::new(),
            invokable_tools: Vec::new(),
            seats: Vec::new(),
            settings: Vec::new(),
            publish: true,
            container: None,
            source: None,
        }
    }
}

impl ComposeEntry {
    pub(crate) fn load_request(&self) -> PluginLoadRequest {
        PluginLoadRequest {
            plugin_id: self.id.clone(),
            root: self.root.clone(),
            entry: self.entry.clone(),
            services: self.services.clone(),
            event_topics: self.event_topics.clone(),
            published_topics: self.published_topics.clone(),
            llm_providers: self.llm_providers.clone(),
            tools: self.tools.clone(),
            commands: self.commands.clone(),
            invokable_tools: self.invokable_tools.clone(),
            seats: self.seats.clone(),
            config: Payload::from(self.config.clone()),
        }
    }
}

/// How the composition is shaped: which entries are grouped, and which groups
/// narrow a service into a realm of their own.
///
/// Structure is not a plugin fact — a group is a container that is not a
/// module, and an isolated realm is a relationship between entries — so it is
/// stated once, when the composition is created, naming only ids and placement.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ComposeNode {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolate: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<Vec<ComposeNode>>,
}

/// Everything the plane needs to start.
#[derive(Clone, Debug)]
pub struct PluginPlaneConfig {
    /// Absolute Node executable, from `rebon_node_runtime::NodeRuntimeResolver`.
    pub node: PathBuf,
    /// Absolute path to the host's `cli.mjs`.
    pub host_script: PathBuf,
    /// Absolute path to the composition loader's `index.mjs`.
    pub loader: PathBuf,
    /// The composition runtime's package directory, which is also the control
    /// plugin's package root.
    pub compose_root: PathBuf,
    /// Where the vendored JS payload lives, when it is not where the runtime
    /// would look by itself.
    pub payload_dir: Option<PathBuf>,
    /// The composition's shape.
    pub structure: Vec<ComposeNode>,
    /// The `web` configuration section, handed to the composition's web seat.
    pub web: Value,
    /// Extra bare-specifier mappings for modules outside the payload.
    pub modules: BTreeMap<String, String>,
    /// The closed set of rebon tools this plane exposes at all.
    pub exposed_tools: Vec<String>,
    /// The closed set of kernel seats this plane exposes at all.
    pub exposed_seats: Vec<String>,
    /// What rebon offers a model, as the definitions a loop hands to its
    /// prompt assembly. A snapshot: what rebon offers is settled when the
    /// composition is built.
    pub tool_catalog: Value,
    /// The scope this plane's own calls travel on. Defaults to
    /// [`DEFAULT_PLANE_SCOPE`]; a plane that exists for one session names it.
    pub scope_id: Option<String>,
    pub working_directory: PathBuf,
    /// How long a caller waits behind one `command/invoke` or unary
    /// `service/call` before being told the plugin did not answer. `None`
    /// takes [`PluginPlane::UNARY_CALL_TIMEOUT`]; a test names a short one so
    /// it can watch the bound fire without waiting out the real number.
    pub unary_call_timeout: Option<std::time::Duration>,
    /// How long an unload waits for a plugin's in-flight calls to finish
    /// before deciding it will not. `None` is [`PluginPlane::DRAIN_DEADLINE`].
    pub drain_deadline: Option<std::time::Duration>,
    /// Where lifecycle facts are committed before the plane's table changes
    /// and the change is announced. `None` records nowhere but the table.
    pub lifecycle_sink: Option<SharedLifecycleSink>,
}

impl PluginPlaneConfig {
    fn control_config(&self) -> Value {
        serde_json::json!({
            "payloadDir": self.payload_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "entries": self.structure,
            "web": self.web,
            "modules": self.modules,
        })
    }
}

/// What one entry reported, protocol fields and composition extras together.
#[derive(Clone, Debug)]
pub struct EntryReport {
    pub ready: PluginReadyReport,
    /// Model catalogs, prompt sections and web providers — the facts the ready
    /// report has no field for, fetched from the composition's own service.
    pub extras: Value,
}

/// A running composition.
pub struct PluginPlane {
    /// The shared host and the containers, and which plugin each serves.
    hosts: PlaneHosts,
    /// The containers running now, by container id.
    containers: tokio::sync::Mutex<BTreeMap<String, ContainerHost>>,
    /// What starting a container needs that the shared host was started with.
    template: ContainerTemplate,
    ctx: Context,
    /// The workspace the plane's own scope names.
    workspace_root: String,
    /// The scope this plane's own calls travel on.
    scope: String,
    /// The deadline the two unary proxies put on a plugin call.
    unary_call_timeout: std::time::Duration,
    /// See [`PluginPlaneConfig::drain_deadline`].
    drain_deadline: std::time::Duration,
    /// Every plugin this plane has tried to run, and where each one stands.
    ///
    /// Kept after an unload, because that is when it is most needed: a plugin
    /// that would not drain is a fact about the *next* load of its id, and the
    /// generation a reload starts from has to survive the unload in between.
    lifecycle: Mutex<BTreeMap<String, LifecycleRecord>>,
    /// See [`PluginPlaneConfig::lifecycle_sink`].
    lifecycle_sink: Option<SharedLifecycleSink>,
    /// Held across commit, table change and announcement, so facts reach the
    /// sink, the table and listeners in one order. A listener must not drive a
    /// lifecycle change itself (an unload, a load) from inside its handler.
    lifecycle_order: Mutex<()>,
    /// The last commit the sink refused, if any.
    lifecycle_sink_error: Mutex<Option<String>>,
    /// The table its tools dispatch through, and the read plane a plugin on
    /// the other side of the pipe lists from. Kept because the plane's
    /// lifetime has to cover it: a registry outliving the plane would offer
    /// tools with nothing behind them.
    tools: Arc<ComposeToolRegistry>,
    /// What each entry registered on rebon's seats, so unloading it can
    /// withdraw exactly that and nothing else.
    registered: Mutex<BTreeMap<String, Registrations>>,
    /// Plugins on this host that are not part of the composition — a
    /// package's model provider, loaded when something first selects it.
    /// Kept apart from [`Self::loaded`] because a reload's diff is about the
    /// composition and must not take one of these down.
    standalone: Mutex<BTreeSet<String>>,
    /// One standalone load at a time.
    standalone_gate: tokio::sync::Mutex<()>,
    /// What each entry was loaded *from*, so a reload can tell what changed.
    ///
    /// The entry itself rather than a hash of it: the comparison is equality on
    /// a value rebon built, and a digest would only add a way to be wrong about
    /// two entries being the same.
    loaded: Mutex<BTreeMap<String, ComposeEntry>>,
    /// Bumped once per reload that changed something.
    ///
    /// Monotone, and never a content hash — a composition can go back to a
    /// shape it held before, and a caller comparing hashes would read that as
    /// "nothing happened" (the ABA the kernel's own reconciler was given a
    /// counter to avoid).
    generation: AtomicU64,
    /// The runtime this plane was started on.
    ///
    /// A slash command's handler is a synchronous `Fn` — that is the command
    /// seat's contract, and every built-in satisfies it — while reaching a
    /// plugin is a call that has to be awaited. The proxy therefore blocks,
    /// and blocking needs a runtime to block *on* that is not the one the
    /// caller is running on.
    runtime: tokio::runtime::Handle,
    /// One reconcile at a time.
    ///
    /// Two interleaved ones would unload an id the other just loaded, and the
    /// second would report a diff computed against a composition that no longer
    /// exists. Async, because it is held across the loads and unloads.
    reconcile: tokio::sync::Mutex<()>,
    /// The Claude Code mods loaded on this plane, once [`Self::install_mods`]
    /// has run. Shared with the seat dispatcher, which routes the `mods` seat
    /// here; empty on a plane that never installed it (the tests' own).
    mods: Arc<std::sync::OnceLock<Arc<crate::mods::ModsRegistry>>>,
}

pub use rebon_kernel::{LifecycleRecord, PluginIncarnation, PluginLifecycle};

/// How one unload ended. The run's lifecycle record says the same thing, in
/// the kernel's vocabulary, once the ending is true.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnloadOutcome {
    /// Drained, withdrawn, gone.
    Clean,
    /// Ended with its container. `taken_down` names the other plugins that
    /// were in it, which went with it: a container is one process, so it
    /// cannot be ended for one of its plugins and kept for the rest.
    Forced {
        outstanding: Vec<String>,
        reason: String,
        taken_down: Vec<String>,
    },
    /// See [`PluginLifecycle::Stuck`].
    Stuck {
        outstanding: Vec<String>,
        reason: String,
    },
}

impl UnloadOutcome {
    /// Whether the plugin drained and left nothing behind.
    pub fn is_clean(&self) -> bool {
        matches!(self, Self::Clean)
    }
}

/// How an unload of a run that already ended reads, or `None` if it has not.
fn ended_outcome(state: &PluginLifecycle) -> Option<UnloadOutcome> {
    match state {
        PluginLifecycle::Unloaded | PluginLifecycle::Failed { .. } => Some(UnloadOutcome::Clean),
        PluginLifecycle::Forced {
            outstanding,
            reason,
        } => Some(UnloadOutcome::Forced {
            outstanding: outstanding.clone(),
            reason: reason.clone(),
            taken_down: Vec::new(),
        }),
        PluginLifecycle::Stuck {
            outstanding,
            reason,
        } => Some(UnloadOutcome::Stuck {
            outstanding: outstanding.clone(),
            reason: reason.clone(),
        }),
        PluginLifecycle::Loading | PluginLifecycle::Ready | PluginLifecycle::Draining => None,
    }
}

/// What a reload did, by name, so a person can see it rather than infer it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReloadOutcome {
    /// The composition's generation after this reload. Unchanged if nothing was.
    pub generation: u64,
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    /// Entries whose configuration is identical, left running untouched. Not
    /// restarting these is the point of diffing at all.
    pub unchanged: Vec<String>,
    /// Entries that could not be brought up, and why.
    ///
    /// The rest of the composition is still reconciled: one plugin that fails
    /// to start is not a reason to leave four working ones in whatever state
    /// the reload had reached.
    pub failed: Vec<(String, String)>,
    /// Removed or restarted entries that did not drain in time and were taken
    /// down with their container. Their details are in [`PluginPlane::lifecycle`].
    pub forced: Vec<String>,
    /// Removed or restarted entries that did not drain in time and could not be
    /// taken down; loading them again needs a restart.
    pub stuck: Vec<String>,
}

impl ReloadOutcome {
    pub fn touched_anything(&self) -> bool {
        !self.added.is_empty() || !self.changed.is_empty() || !self.removed.is_empty()
    }
}

/// What one reload has to do, worked out before any of it is done.
#[derive(Debug, Default, PartialEq)]
struct ReloadPlan {
    added: Vec<String>,
    changed: Vec<String>,
    removed: Vec<String>,
    unchanged: Vec<String>,
    /// Ids to stop, in the order to stop them.
    stop: Vec<String>,
    /// Indices into `wanted`, in the order to start them.
    start: Vec<usize>,
}

/// The classification half of a reload, with nothing to do the work.
///
/// Split out because it is the whole rule — what counts as changed, and in what
/// order things move — and because a rule that needs a Node host to test is a
/// rule that gets tested once.
fn classify(current: &BTreeMap<String, ComposeEntry>, wanted: &[ComposeEntry]) -> ReloadPlan {
    let mut plan = ReloadPlan::default();
    for (index, entry) in wanted.iter().enumerate() {
        match current.get(&entry.id) {
            // Equality on the entry rebon built: same package, same module,
            // same configuration, same declared ceiling.
            Some(running) if running == entry => plan.unchanged.push(entry.id.clone()),
            Some(_) => {
                plan.changed.push(entry.id.clone());
                plan.start.push(index);
            }
            None => {
                plan.added.push(entry.id.clone());
                plan.start.push(index);
            }
        }
    }
    let wanted_ids: BTreeSet<&str> = wanted.iter().map(|entry| entry.id.as_str()).collect();
    for id in current.keys() {
        if !wanted_ids.contains(id.as_str()) {
            plan.removed.push(id.clone());
        }
    }

    // Everything that has to come down, in reverse of the order it went up —
    // so a plugin is never stopped before something that may still be calling
    // it. A changed entry is a stop and a start, because an id cannot be
    // loaded twice.
    plan.stop = plan
        .removed
        .iter()
        .chain(plan.changed.iter())
        .cloned()
        .collect();
    plan.stop.sort();
    plan.stop.reverse();
    plan
}

/// Which host serves a plugin: the shared one, or the container it runs in.
///
/// One table every caller routes through — the tool dispatch, the mods
/// registry, the plane itself — so "who answers this plugin" has one answer
/// however the call arrived.
#[derive(Clone)]
pub struct PlaneHosts {
    main: Arc<PluginHostSupervisor>,
    contained: Arc<Mutex<BTreeMap<String, Arc<PluginHostSupervisor>>>>,
}

impl PlaneHosts {
    fn new(main: Arc<PluginHostSupervisor>) -> Self {
        Self {
            main,
            contained: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// The shared host.
    pub fn main(&self) -> &Arc<PluginHostSupervisor> {
        &self.main
    }

    /// The host that serves `plugin_id`.
    pub fn for_plugin(&self, plugin_id: &str) -> Arc<PluginHostSupervisor> {
        self.contained
            .lock()
            .expect("plane hosts poisoned")
            .get(plugin_id)
            .cloned()
            .unwrap_or_else(|| Arc::clone(&self.main))
    }

    /// Whether `plugin_id` runs in a container.
    pub fn is_contained(&self, plugin_id: &str) -> bool {
        self.contained
            .lock()
            .expect("plane hosts poisoned")
            .contains_key(plugin_id)
    }

    fn assign(&self, plugin_id: &str, host: &Arc<PluginHostSupervisor>) {
        self.contained
            .lock()
            .expect("plane hosts poisoned")
            .insert(plugin_id.to_owned(), Arc::clone(host));
    }

    fn release(&self, plugin_id: &str) {
        self.contained
            .lock()
            .expect("plane hosts poisoned")
            .remove(plugin_id);
    }
}

/// One running container.
struct ContainerHost {
    supervisor: Arc<PluginHostSupervisor>,
    /// The entries loaded in it; the host goes when the last one leaves.
    members: BTreeSet<String>,
    /// What lives as long as the host (the network proxy).
    _keep_alive: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

/// What a container's host is started with, copied from the shared host's
/// start so a container is the same plane in a smaller box.
/// Hands out host epochs: a random base per plane, one step per host it
/// spawns.
///
/// A host is told apart by its epoch on every call identity, and the plane's
/// own counter would start over with the plane. The base makes two planes —
/// or one rebon started twice — unlikely to collide; the step leaves room for
/// a supervisor's own restarts (+1 each) before the next host's range. Every
/// epoch stays below 2^52, a safe JSON integer, which the wire requires.
struct HostEpochs {
    base: u64,
    next: AtomicU64,
}

impl HostEpochs {
    const STEP_BITS: u32 = 10;

    fn new() -> Self {
        use std::hash::{BuildHasher, Hasher};
        let random = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        Self {
            base: (random & ((1 << 21) - 1)) << 31,
            next: AtomicU64::new(1),
        }
    }

    fn next(&self) -> u64 {
        self.base | (self.next.fetch_add(1, Ordering::Relaxed) << Self::STEP_BITS)
    }
}

struct ContainerTemplate {
    /// Shared with the plane's own host, so no two hosts share an epoch.
    epochs: Arc<HostEpochs>,
    node: PathBuf,
    host_script: PathBuf,
    loader: PathBuf,
    compose_root: PathBuf,
    payload_dir: Option<PathBuf>,
    web: Value,
    modules: BTreeMap<String, String>,
    invoker: Arc<dyn ToolInvoker>,
    seats: Arc<KernelSeats>,
    events: Arc<KernelEvents>,
    exposed_tools: Vec<String>,
    exposed_seats: Vec<String>,
}

#[derive(Default)]
struct Registrations {
    /// Whether these reached rebon's seats. A private entry is recorded all the
    /// same — the plane still has to know who serves what to route a call at it.
    published: bool,
    tools: Vec<String>,
    commands: Vec<String>,
    services: Vec<String>,
    providers: Vec<String>,
    sections: Vec<(String, u64)>,
    /// The kernel scope this entry's seat registrations live on. Disposing it
    /// is what takes its tools off the seat — one statement instead of a
    /// token-guarded sweep per tool.
    scope: Option<Context>,
}

impl std::fmt::Debug for PluginPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginPlane").finish_non_exhaustive()
    }
}

impl PluginPlane {
    /// Starts the host, creates the realm, and returns a plane with no entries
    /// loaded yet.
    ///
    /// The invoker is handed in rather than built here; the seat dispatcher and
    /// event publisher are this layer's own thin adapters. What a tool
    /// invocation is permitted to do is the embedder's question, and a plane
    /// that answered it itself would be a second permission system.
    pub async fn start(
        config: PluginPlaneConfig,
        ctx: Context,
        tools: Arc<ComposeToolRegistry>,
        invoker: Arc<dyn ToolInvoker>,
    ) -> Result<Arc<Self>, SupervisorError> {
        let mods = Arc::new(std::sync::OnceLock::new());
        let seats = Arc::new(KernelSeats {
            ctx: ctx.clone(),
            mods: Arc::clone(&mods),
        });
        let events = Arc::new(KernelEvents { ctx: ctx.clone() });
        let epochs = Arc::new(HostEpochs::new());
        let template = ContainerTemplate {
            epochs: Arc::clone(&epochs),
            node: config.node.clone(),
            host_script: config.host_script.clone(),
            loader: config.loader.clone(),
            compose_root: config.compose_root.clone(),
            payload_dir: config.payload_dir.clone(),
            web: config.web.clone(),
            modules: config.modules.clone(),
            invoker: Arc::clone(&invoker),
            seats: Arc::clone(&seats),
            events: Arc::clone(&events),
            exposed_tools: config.exposed_tools.clone(),
            exposed_seats: config.exposed_seats.clone(),
        };
        let host = HostConfig::new(config.node.clone(), config.host_script.clone())
            .with_host_epoch(epochs.next())
            .with_loader(&config.loader)
            .with_working_directory(config.working_directory.clone())
            .with_tool_invoker(invoker, config.exposed_tools.iter().cloned())
            .with_seat_dispatcher(seats, config.exposed_seats.iter().cloned())
            .with_event_publisher(events);
        let supervisor = Arc::new(PluginHostSupervisor::start(host).await?);

        // The control plugin creates the realm every entry mounts into, so it
        // is loaded before any of them — and a failure here is the whole
        // composition failing rather than one entry.
        let control = PluginLoadRequest {
            plugin_id: COMPOSE_PLUGIN_ID.to_owned(),
            root: config.compose_root.to_string_lossy().into_owned(),
            entry: "src/plugin.mjs".to_owned(),
            services: vec!["compose".to_owned()],
            config: Payload::from(config.control_config()),
            event_topics: Vec::new(),
            published_topics: Vec::new(),
            llm_providers: Vec::new(),
            tools: Vec::new(),
            commands: Vec::new(),
            invokable_tools: Vec::new(),
            seats: Vec::new(),
        };
        supervisor.load_plugin(&control).await?;
        let workspace_root = config.working_directory.to_string_lossy().into_owned();
        let scope = config
            .scope_id
            .clone()
            .unwrap_or_else(|| DEFAULT_PLANE_SCOPE.to_owned());
        supervisor
            .open_scope(COMPOSE_PLUGIN_ID, &scope, &workspace_root)
            .await?;

        let hosts = PlaneHosts::new(Arc::clone(&supervisor));
        let plane = Arc::new(Self {
            hosts: hosts.clone(),
            containers: tokio::sync::Mutex::new(BTreeMap::new()),
            template,
            ctx,
            workspace_root,
            scope,
            unary_call_timeout: config
                .unary_call_timeout
                .unwrap_or(Self::UNARY_CALL_TIMEOUT),
            drain_deadline: config.drain_deadline.unwrap_or(Self::DRAIN_DEADLINE),
            lifecycle: Mutex::new(BTreeMap::new()),
            lifecycle_sink: config.lifecycle_sink.clone(),
            lifecycle_order: Mutex::new(()),
            lifecycle_sink_error: Mutex::new(None),
            tools: Arc::clone(&tools),
            registered: Mutex::new(BTreeMap::new()),
            runtime: tokio::runtime::Handle::current(),
            standalone: Mutex::new(BTreeSet::new()),
            standalone_gate: tokio::sync::Mutex::new(()),
            loaded: Mutex::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
            reconcile: tokio::sync::Mutex::new(()),
            mods,
        });
        tools.bind_plane(Arc::new(PlaneToolDispatch {
            hosts,
            owner: Arc::downgrade(&plane),
        }));
        Ok(plane)
    }

    /// The shared host.
    pub fn supervisor(&self) -> &Arc<PluginHostSupervisor> {
        self.hosts.main()
    }

    /// Every host this plane runs, and which plugin each serves.
    pub fn hosts(&self) -> &PlaneHosts {
        &self.hosts
    }

    /// The containers running now, by id, with the entries in each.
    pub async fn containers(&self) -> BTreeMap<String, Vec<String>> {
        self.containers
            .lock()
            .await
            .iter()
            .map(|(id, host)| (id.clone(), host.members.iter().cloned().collect()))
            .collect()
    }

    /// The host an entry loads on: its container's, started if this is the
    /// container's first entry, or the shared one.
    async fn host_for_entry(
        &self,
        entry: &ComposeEntry,
    ) -> Result<Arc<PluginHostSupervisor>, HostCallError> {
        let Some(spec) = &entry.container else {
            return Ok(Arc::clone(self.hosts.main()));
        };
        // Held across the start: two entries of one bundle loading at once
        // must not start the container twice.
        let mut containers = self.containers.lock().await;
        if let Some(running) = containers.get_mut(&spec.id) {
            running.members.insert(entry.id.clone());
            self.hosts.assign(&entry.id, &running.supervisor);
            return Ok(Arc::clone(&running.supervisor));
        }
        let (supervisor, keep_alive) = self.start_container(spec).await?;
        containers.insert(
            spec.id.clone(),
            ContainerHost {
                supervisor: Arc::clone(&supervisor),
                members: BTreeSet::from([entry.id.clone()]),
                _keep_alive: keep_alive,
            },
        );
        self.hosts.assign(&entry.id, &supervisor);
        Ok(supervisor)
    }

    /// Starts one container's host and its realm.
    async fn start_container(
        &self,
        spec: &crate::container::ContainerSpec,
    ) -> Result<
        (
            Arc<PluginHostSupervisor>,
            Option<Arc<dyn std::any::Any + Send + Sync>>,
        ),
        HostCallError,
    > {
        let refuse = |why: String| {
            HostCallError::Malformed(format!("[CONTAINER_REFUSED] {}: {why}", spec.id))
        };
        let data = PathBuf::from(&spec.data_dir);
        std::fs::create_dir_all(data.join("tmp"))
            .map_err(|error| refuse(format!("creating {}: {error}", data.display())))?;
        let template = &self.template;
        let mut extra_read = vec![template.compose_root.clone()];
        extra_read.extend(template.payload_dir.iter().cloned());
        let read: Vec<PathBuf> = extra_read
            .iter()
            .cloned()
            .chain(spec.read.iter().map(PathBuf::from))
            .chain(crate::container::runtime_root(&template.host_script))
            .collect();
        // Node's half first: the OS layer wraps the command line it makes.
        let node_half = crate::container::container_launch(
            spec,
            &template.host_script,
            &extra_read,
            &|key| std::env::var_os(key),
            &[],
        );
        let mut probe_config = HostConfig::new(template.node.clone(), template.host_script.clone())
            .with_host_epoch(template.epochs.next())
            .with_loader(&template.loader);
        probe_config.node_args = node_half.node_args.clone();
        let argv = rebon_plugin_supervisor::node_argv(&probe_config);
        let confinement = match self.ctx.get::<rebon_tool::ContainerSandboxService>() {
            Some(sandbox) => sandbox
                .confine(&rebon_tool::ConfineRequest {
                    container: spec.id.clone(),
                    node: template.node.clone(),
                    argv,
                    environment: node_half.environment.clone(),
                    read,
                    write: data.clone(),
                    network: spec.network.clone(),
                })
                .map_err(refuse)?,
            None => {
                let mut confinement = rebon_tool::ContainerConfinement::default();
                confinement.notes.push(
                    "no container sandbox in this build: the host runs under Node's \
                     permission model only, and its network is not restricted"
                        .to_owned(),
                );
                confinement
            }
        };
        for note in &confinement.notes {
            tracing::warn!(container = %spec.id, "{note}");
        }
        let launch = crate::container::container_launch(
            spec,
            &template.host_script,
            &extra_read,
            &|key| std::env::var_os(key),
            &confinement.environment,
        );
        let mut host = HostConfig::new(template.node.clone(), template.host_script.clone())
            .with_host_epoch(template.epochs.next())
            .with_loader(&template.loader)
            .with_working_directory(launch.working_directory.clone())
            .with_tool_invoker(
                Arc::clone(&template.invoker),
                template.exposed_tools.iter().cloned(),
            )
            .with_seat_dispatcher(
                Arc::clone(&template.seats) as Arc<dyn SeatDispatcher>,
                template.exposed_seats.iter().cloned(),
            )
            .with_event_publisher(Arc::clone(&template.events) as Arc<dyn EventPublisher>);
        host.node_args = launch.node_args;
        host.environment = Some(launch.environment);
        host.launcher = confinement
            .launcher
            .as_ref()
            .map(crate::container::host_launcher);
        let supervisor = Arc::new(
            PluginHostSupervisor::start(host)
                .await
                .map_err(|error| refuse(format!("starting its host: {error}")))?,
        );
        let control = PluginLoadRequest {
            plugin_id: COMPOSE_PLUGIN_ID.to_owned(),
            root: template.compose_root.to_string_lossy().into_owned(),
            entry: "src/plugin.mjs".to_owned(),
            services: vec!["compose".to_owned()],
            config: Payload::from(serde_json::json!({
                "payloadDir": template.payload_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
                "entries": [],
                "web": template.web,
                "modules": template.modules,
            })),
            event_topics: Vec::new(),
            published_topics: Vec::new(),
            llm_providers: Vec::new(),
            tools: Vec::new(),
            commands: Vec::new(),
            invokable_tools: Vec::new(),
            seats: Vec::new(),
        };
        let realm = async {
            supervisor.load_plugin(&control).await?;
            supervisor
                .open_scope(COMPOSE_PLUGIN_ID, &self.scope, &self.workspace_root)
                .await
        };
        if let Err(error) = realm.await {
            let _ = supervisor.shutdown().await;
            return Err(error);
        }
        Ok((supervisor, confinement.keep_alive))
    }

    /// Takes an entry out of its container, and the container down with its
    /// last entry: the process goes, and everything the plugin held with it.
    async fn leave_container(&self, plugin_id: &str) {
        if !self.hosts.is_contained(plugin_id) {
            return;
        }
        self.hosts.release(plugin_id);
        let emptied = {
            let mut containers = self.containers.lock().await;
            let Some(id) = containers
                .iter()
                .find(|(_, host)| host.members.contains(plugin_id))
                .map(|(id, _)| id.clone())
            else {
                return;
            };
            let host = containers.get_mut(&id).expect("found above");
            host.members.remove(plugin_id);
            if host.members.is_empty() {
                containers.remove(&id)
            } else {
                None
            }
        };
        if let Some(host) = emptied {
            // `shutdown` reaps the process whether or not the host answered,
            // so an error here is about manners, not about the process being
            // left behind — but it is still said, not swallowed.
            if let Err(error) = host.supervisor.shutdown().await {
                tracing::warn!(
                    plugin = plugin_id,
                    %error,
                    "the container did not shut down cleanly; it was ended"
                );
            }
        }
    }

    /// Puts the mods registry on this plane: the `mods` seat answers, the
    /// policy seat hears the mods, and every mod entry loaded from now on is
    /// attached. Once per plane; a second call is a no-op.
    pub fn install_mods(
        &self,
        kernel: &Arc<rebon_kernel::Kernel>,
        config_dir: PathBuf,
    ) -> Arc<crate::mods::ModsRegistry> {
        let registry = self.mods.get_or_init(|| {
            let registry = crate::mods::ModsRegistry::new(
                self.hosts.clone(),
                self.scope.clone(),
                kernel.context().clone(),
                self.ctx.clone(),
                Arc::clone(&self.tools),
                self.runtime.clone(),
                self.unary_call_timeout,
                config_dir,
            );
            registry.install(kernel);
            registry.set_tool_invoker(Arc::clone(&self.template.invoker));
            registry
        });
        Arc::clone(registry)
    }

    /// The mods registry, when one was installed.
    pub fn mods(&self) -> Option<Arc<crate::mods::ModsRegistry>> {
        self.mods.get().cloned()
    }

    /// The scope this plane's own calls travel on.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// The workspace this plane's scopes name.
    pub fn workspace_root(&self) -> &str {
        &self.workspace_root
    }

    /// The dispatch a registration face runs its entries on.
    ///
    /// Both the tool registry and the web seat hold one of these: neither cares
    /// which runtime is behind it, and handing them the same one is what keeps
    /// "who serves this name" a single answer.
    pub fn tool_dispatch(self: &Arc<Self>) -> Arc<dyn ComposeToolDispatch> {
        Arc::new(PlaneToolDispatch {
            hosts: self.hosts.clone(),
            owner: Arc::downgrade(self),
        })
    }

    /// Loads one entry and registers what it reported on rebon's seats.
    pub async fn load_entry(&self, entry: &ComposeEntry) -> Result<EntryReport, HostCallError> {
        self.refuse_if_stuck(&entry.id)?;
        let host = self.host_for_entry(entry).await?;
        if let Err(error) = self.begin_attempt(&host, entry).await {
            self.leave_container(&entry.id).await;
            return Err(error);
        }
        match self.load_entry_on(&host, entry).await {
            Ok(report) => {
                self.record_lifecycle(&entry.id, PluginLifecycle::Ready);
                Ok(report)
            }
            Err(error) => {
                self.leave_container(&entry.id).await;
                self.record_lifecycle(
                    &entry.id,
                    PluginLifecycle::Failed {
                        reason: error.to_string(),
                    },
                );
                Err(error)
            }
        }
    }

    async fn load_entry_on(
        &self,
        host: &Arc<PluginHostSupervisor>,
        entry: &ComposeEntry,
    ) -> Result<EntryReport, HostCallError> {
        self.declare_settings(entry);
        let ready = match host.load_plugin(&entry.load_request()).await {
            Ok(ready) => ready,
            Err(error) => {
                self.undeclare_settings(&entry.id);
                return Err(error);
            }
        };
        // A mod joins the registry before its scope opens: opening the scope
        // runs its `session.start`, whose first `$` calls (a status, a
        // command's description) reach the `mods` seat, and the seat answers
        // for the mods it knows.
        if crate::mods::ModsRegistry::entry_is_mod(entry) {
            match self.mods() {
                Some(mods) => {
                    if let Err(error) = mods.attach(entry) {
                        // Its commands or tools could not be seated: the same
                        // ending a failed registration gets.
                        self.undeclare_settings(&entry.id);
                        self.unload_best_effort(host, &entry.id).await;
                        return Err(HostCallError::Malformed(error));
                    }
                }
                None => tracing::warn!(
                    entry = %entry.id,
                    "a mod loaded on a plane with no mods registry; its hooks, commands and tools are not reachable"
                ),
            }
        }
        // Before anything is asked of it: the plane's own scope is what a
        // report, a tool call or a model turn rebon initiates travels on.
        if let Err(error) = host
            .open_scope(&entry.id, &self.scope, &self.workspace_root)
            .await
        {
            self.withdraw(&entry.id);
            self.undeclare_settings(&entry.id);
            self.unload_best_effort(host, &entry.id).await;
            return Err(error);
        }
        let extras = match self.report_for(host, &entry.id).await {
            Ok(extras) => extras,
            // Not every composition entry is a Cordis plugin. A module
            // exporting `activate` is a plugin in rebon's own shape, and the
            // composition loader hands it to the host's own loader rather than
            // mounting it in the realm — which is deliberate, and which is the
            // shape of every plugin that offers commands or tools. The control
            // plugin has nothing to say about such an entry, and saying so is
            // not a refusal: the extras only a mounted entry has are model
            // catalogs and prompt sections, and this one has none. Everything
            // it *did* register is already in the ready report.
            Err(error) if entry_was_not_mounted_by_the_composition(&error) => Value::Null,
            Err(error) => {
                // The same ending a failed registration gets, for the same
                // reason: an entry rebon will not route to must not be left
                // running on the host, where a reload of the same id would
                // then be refused as already loaded.
                self.withdraw(&entry.id);
                self.undeclare_settings(&entry.id);
                self.unload_best_effort(host, &entry.id).await;
                return Err(error);
            }
        };
        if let Err(error) = self.register(
            host,
            &entry.id,
            &ready,
            &extras,
            entry.publish,
            crate::mods::ModsRegistry::entry_is_mod(entry),
        ) {
            // Both sides have to agree about what is loaded. An entry rebon
            // could not finish registering is one rebon will not route to, so
            // leaving it mounted would be a plugin running for nobody — and a
            // reload of the same id would then be refused as already loaded.
            self.withdraw(&entry.id);
            self.undeclare_settings(&entry.id);
            self.unload_best_effort(host, &entry.id).await;
            return Err(error);
        }
        // Recorded only once the entry is fully up, so a failed load leaves
        // nothing for a later reload to diff against.
        self.loaded
            .lock()
            .expect("plane loaded table")
            .insert(entry.id.clone(), entry.clone());
        Ok(EntryReport { ready, extras })
    }

    /// Put the entry's manifest declaration on the settings seat.
    ///
    /// Before the module loads, because `activate` may read its own settings
    /// and a declaration arriving after it is one whose defaults were missing
    /// when they were wanted. The plane speaks for the plugin here for the
    /// same reason the kernel does for a Rust one: the id is the namespace,
    /// and the host is what assigns the id.
    ///
    /// No seat is not an error — a plane can be assembled against a kernel
    /// with no settings — and a refusal is warned about rather than failing
    /// the load: what it costs is a plugin whose writes are refused, which is
    /// the failure the plugin will report itself, with its own name on it.
    fn declare_settings(&self, entry: &ComposeEntry) {
        if entry.settings.is_empty() {
            return;
        }
        let keys: Vec<Value> = entry
            .settings
            .iter()
            .map(|key| {
                let mut declared = serde_json::json!({ "name": key.name });
                if let Some(ty) = &key.ty {
                    declared["type"] = Value::String(ty.clone());
                }
                if let Some(default) = &key.default {
                    declared["default"] = default.clone();
                }
                declared
            })
            .collect();
        if let Err(error) = self.ctx.call_json(
            rebon_kernel::SETTINGS_SERVICE,
            "declare",
            serde_json::json!({
                rebon_kernel_seats::kernel_config_seats::CALLER_PLUGIN_ID: entry.id,
                "namespace": entry.id,
                "keys": keys,
            }),
        ) {
            tracing::warn!(
                plugin = %entry.id,
                %error,
                "plane: settings declaration refused; this plugin's writes will be too"
            );
        }
    }

    /// Take the declaration back out when the entry goes.
    fn undeclare_settings(&self, plugin_id: &str) {
        let _ = self.ctx.call_json(
            rebon_kernel::SETTINGS_SERVICE,
            "undeclare",
            serde_json::json!({
                rebon_kernel_seats::kernel_config_seats::CALLER_PLUGIN_ID: plugin_id,
                "namespace": plugin_id,
            }),
        );
    }

    /// Drains one entry and withdraws exactly what it registered.
    ///
    /// Withdrawal happens after the drain rather than before: a call still in
    /// flight is answered by the tool it is already inside, and pulling the
    /// registration first would only make the answer arrive for a tool rebon
    /// says does not exist.
    ///
    /// "After the drain" means after it *finishes*: `plugin/unload` answers
    /// when the drain begins, so the calls still running are waited on here.
    /// One deadline covers both the request and the wait. Past it — or if the
    /// host will not take the request at all — a contained entry is ended with
    /// its whole container ([`UnloadOutcome::Forced`]), and an entry on the
    /// shared host, which cannot be ended for it, is withdrawn and marked
    /// [`UnloadOutcome::Stuck`], so its id is refused until a restart rather
    /// than loaded beside the run that is still answering.
    ///
    /// The run's terminal state is recorded last: after its registrations are
    /// withdrawn and, where a container was ended, after the process is gone.
    /// A contained entry leaving cleanly can take up to twice the host's
    /// shutdown window when it is the container's last: once for the request,
    /// once for the process to exit before it is killed.
    ///
    /// A run that already ended — forced out with another plugin's container,
    /// failed with its host — is not unloaded again: its record says how it
    /// ended, and that is what is returned.
    pub async fn unload_entry(&self, plugin_id: &str) -> UnloadOutcome {
        if let Some(ended) = self
            .lifecycle(plugin_id)
            .and_then(|record| ended_outcome(&record.state))
        {
            return ended;
        }
        let host = self.hosts.for_plugin(plugin_id);
        let contained = self.hosts.is_contained(plugin_id);
        self.record_lifecycle(plugin_id, PluginLifecycle::Draining);

        let until = tokio::time::Instant::now() + self.drain_deadline;
        let remaining = || until.saturating_duration_since(tokio::time::Instant::now());
        // `Ok` when nothing of the plugin is left running; `Err` with what is
        // still running (possibly unknown) and why it did not finish.
        let drained: Result<(), (Vec<String>, String)> =
            match host.unload_plugin_within(plugin_id, remaining()).await {
                Ok(_) => host
                    .await_drain(plugin_id, remaining())
                    .await
                    .map_err(|outstanding| {
                        (
                            outstanding,
                            format!(
                                "did not finish its calls within {} seconds",
                                whole_seconds(self.drain_deadline)
                            ),
                        )
                    }),
                // A host that never heard of the id runs nothing of it.
                Err(HostCallError::Registry {
                    source: RegistryError::UnknownPlugin { .. },
                }) => Ok(()),
                // A dead host runs nothing, whichever layer said so.
                Err(_) if !host.is_alive().await => Ok(()),
                Err(error) => Err((
                    host.in_flight(plugin_id).await,
                    format!("the unload was not completed: {error}"),
                )),
            };

        match drained {
            Ok(()) => {
                self.forget(plugin_id);
                // Before the container is left: a dead host's container is
                // taken out of the table with every plugin still in it.
                if !host.is_alive().await {
                    self.note_dead_host(&host, Some(plugin_id)).await;
                }
                self.leave_container(plugin_id).await;
                self.record_lifecycle(plugin_id, PluginLifecycle::Unloaded);
                UnloadOutcome::Clean
            }
            Err((outstanding, reason)) if contained => {
                tracing::warn!(
                    plugin = plugin_id,
                    ?outstanding,
                    %reason,
                    "ending the plugin's container"
                );
                let taken_down = self.force_container(plugin_id, &reason).await;
                self.record_lifecycle(
                    plugin_id,
                    PluginLifecycle::Forced {
                        outstanding: outstanding.clone(),
                        reason: reason.clone(),
                    },
                );
                UnloadOutcome::Forced {
                    outstanding,
                    reason,
                    taken_down,
                }
            }
            Err((outstanding, reason)) => {
                tracing::warn!(
                    plugin = plugin_id,
                    ?outstanding,
                    %reason,
                    "plugin did not drain and cannot be ended; its id is refused until restart"
                );
                self.forget(plugin_id);
                self.record_lifecycle(
                    plugin_id,
                    PluginLifecycle::Stuck {
                        outstanding: outstanding.clone(),
                        reason: reason.clone(),
                    },
                );
                UnloadOutcome::Stuck {
                    outstanding,
                    reason,
                }
            }
        }
    }

    /// Takes everything one entry put into rebon back out of it, and forgets
    /// the entry was loaded.
    fn forget(&self, plugin_id: &str) {
        self.withdraw(plugin_id);
        self.undeclare_settings(plugin_id);
        self.loaded
            .lock()
            .expect("plane loaded table")
            .remove(plugin_id);
        // A standalone plugin is forgotten here too, so selecting it again
        // loads it again rather than being refused as already up.
        self.standalone
            .lock()
            .expect("plane standalone table")
            .remove(plugin_id);
    }

    /// Ends the container `plugin_id` runs in, with every plugin in it.
    ///
    /// Each member is withdrawn first, then the process is ended and waited
    /// on, and only then are the other members recorded as forced — so no
    /// record says a run is over while its process still is not. Returns the
    /// other members, which the caller may want to start again.
    async fn force_container(&self, plugin_id: &str, reason: &str) -> Vec<String> {
        let removed = {
            let mut containers = self.containers.lock().await;
            let id = containers
                .iter()
                .find(|(_, host)| host.members.contains(plugin_id))
                .map(|(id, _)| id.clone());
            id.and_then(|id| containers.remove(&id))
        };
        let Some(container) = removed else {
            self.forget(plugin_id);
            self.hosts.release(plugin_id);
            return Vec::new();
        };
        let mut others = Vec::new();
        for member in &container.members {
            if member != plugin_id {
                others.push((member.clone(), container.supervisor.in_flight(member).await));
            }
            self.forget(member);
            self.hosts.release(member);
        }
        container.supervisor.terminate().await;
        for (member, outstanding) in &others {
            self.record_lifecycle(
                member,
                PluginLifecycle::Forced {
                    outstanding: outstanding.clone(),
                    reason: format!(
                        "its container was ended because {plugin_id} had to be ended: {reason}"
                    ),
                },
            );
        }
        others.into_iter().map(|(member, _)| member).collect()
    }

    /// How long an unload waits for a plugin's in-flight calls by default:
    /// long enough for a tool mid-write to finish, short enough that a
    /// person uninstalling something is not left wondering.
    pub const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

    /// Where one plugin's latest run stands, if this plane has ever tried to
    /// run it.
    pub fn lifecycle(&self, plugin_id: &str) -> Option<LifecycleRecord> {
        self.lifecycle
            .lock()
            .expect("plane lifecycle table")
            .get(plugin_id)
            .cloned()
    }

    /// Every plugin this plane has tried to run, and where each stands.
    pub fn lifecycles(&self) -> BTreeMap<String, LifecycleRecord> {
        self.lifecycle
            .lock()
            .expect("plane lifecycle table")
            .clone()
    }

    /// Refuses an id whose last run never drained.
    fn refuse_if_stuck(&self, plugin_id: &str) -> Result<(), HostCallError> {
        match self.lifecycle(plugin_id).map(|record| record.state) {
            Some(PluginLifecycle::Stuck { outstanding, .. }) => Err(HostCallError::Stuck {
                plugin_id: plugin_id.to_owned(),
                count: outstanding.len(),
            }),
            _ => Ok(()),
        }
    }

    /// Opens a new run of the plugin, before its load is sent, so that
    /// anything it does while it activates already has a run to belong to.
    ///
    /// The one lifecycle change that is refused when the sink refuses it:
    /// nothing has happened yet, so not starting is the honest answer, and
    /// the attempt consumes no generation.
    async fn begin_attempt(
        &self,
        host: &Arc<PluginHostSupervisor>,
        entry: &ComposeEntry,
    ) -> Result<PluginIncarnation, HostCallError> {
        let plugin_id = entry.id.as_str();
        let host_epoch = host.host_epoch().await;
        let _order = self.lifecycle_order.lock().expect("plane lifecycle order");
        let generation = self
            .lifecycle(plugin_id)
            .map_or(1, |record| record.incarnation.generation + 1);
        let fact = PluginLifecycleChanged {
            incarnation: PluginIncarnation {
                source: entry.source.clone(),
                plugin_id: plugin_id.to_owned(),
                host_epoch,
                generation,
            },
            from: None,
            to: PluginLifecycle::Loading,
        };
        self.commit_lifecycle(&fact)
            .map_err(|reason| HostCallError::LifecycleUnrecorded {
                plugin_id: plugin_id.to_owned(),
                reason,
            })?;
        self.lifecycle
            .lock()
            .expect("plane lifecycle table")
            .insert(
                plugin_id.to_owned(),
                LifecycleRecord {
                    incarnation: fact.incarnation.clone(),
                    state: fact.to.clone(),
                },
            );
        self.ctx.emit(&fact);
        Ok(fact.incarnation)
    }

    /// Moves the plugin's current run to `to`: committed to the sink, then
    /// the table, then announced on the kernel's event plane. The one place a
    /// run's state changes after it opened.
    ///
    /// Every change made here is already true by the time it is recorded — a
    /// load answered, a drain begun, registrations withdrawn, a host ended —
    /// so a refused commit does not undo it: the table keeps agreeing with
    /// what happened, the change is still announced, and the refusal is
    /// logged and kept for [`Self::lifecycle_sink_error`].
    fn record_lifecycle(&self, plugin_id: &str, to: PluginLifecycle) {
        let _order = self.lifecycle_order.lock().expect("plane lifecycle order");
        let Some(current) = self.lifecycle(plugin_id) else {
            return;
        };
        let fact = PluginLifecycleChanged {
            incarnation: current.incarnation,
            from: Some(current.state),
            to,
        };
        if let Err(reason) = self.commit_lifecycle(&fact) {
            tracing::error!(
                plugin = plugin_id,
                to = ?fact.to,
                %reason,
                "a lifecycle fact could not be recorded; the change stands"
            );
        }
        if let Some(record) = self
            .lifecycle
            .lock()
            .expect("plane lifecycle table")
            .get_mut(plugin_id)
        {
            record.state = fact.to.clone();
        }
        self.ctx.emit(&fact);
    }

    fn commit_lifecycle(&self, fact: &PluginLifecycleChanged) -> Result<(), String> {
        let Some(sink) = &self.lifecycle_sink else {
            return Ok(());
        };
        sink.0.commit(fact).inspect_err(|reason| {
            *self
                .lifecycle_sink_error
                .lock()
                .expect("plane lifecycle sink error") = Some(reason.clone());
        })
    }

    /// The last lifecycle fact the sink refused, if it ever refused one.
    pub fn lifecycle_sink_error(&self) -> Option<String> {
        self.lifecycle_sink_error
            .lock()
            .expect("plane lifecycle sink error")
            .clone()
    }

    /// Records every other run on a host that died as failed, and takes them
    /// out of rebon.
    ///
    /// A dead host is noticed when something reaches for it — an unload, the
    /// start of a reload — not the moment it exits; until then its plugins
    /// still read as ready. (Hearing of the exit as it happens needs the
    /// supervisor to say so, which is S3's.)
    async fn note_dead_host(&self, host: &Arc<PluginHostSupervisor>, except: Option<&str>) {
        let members: Vec<String> = self
            .lifecycles()
            .into_iter()
            .filter(|(id, record)| {
                !record.state.is_terminal()
                    && Some(id.as_str()) != except
                    && Arc::ptr_eq(&self.hosts.for_plugin(id), host)
            })
            .map(|(id, _)| id)
            .collect();
        let container = {
            let mut containers = self.containers.lock().await;
            let id = containers
                .iter()
                .find(|(_, running)| Arc::ptr_eq(&running.supervisor, host))
                .map(|(id, _)| id.clone());
            id.and_then(|id| containers.remove(&id))
        };
        for member in &members {
            self.forget(member);
            self.hosts.release(member);
        }
        if let Some(container) = container {
            for member in &container.members {
                self.hosts.release(member);
            }
            // Reaps the dead process; there is nothing left to ask.
            let _ = container.supervisor.shutdown().await;
        }
        for member in members {
            self.record_lifecycle(
                &member,
                PluginLifecycle::Failed {
                    reason: "its plugin host exited".to_owned(),
                },
            );
        }
    }

    /// Notices every host that died since the plane last looked.
    async fn sweep_dead_hosts(&self) {
        let main = Arc::clone(self.hosts.main());
        if !main.is_alive().await {
            self.note_dead_host(&main, None).await;
        }
        let containers: Vec<Arc<PluginHostSupervisor>> = self
            .containers
            .lock()
            .await
            .values()
            .map(|running| Arc::clone(&running.supervisor))
            .collect();
        for host in containers {
            if !host.is_alive().await {
                self.note_dead_host(&host, None).await;
            }
        }
    }

    /// How long a session waits for one package to load before being told the
    /// host did not answer. Long enough for a cold Node importing a package,
    /// short enough that a person is still looking at the screen.
    const STANDALONE_LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

    /// Take a half-loaded entry back out, and do not let that wait forever.
    ///
    /// The answer is discarded -- the caller already has its verdict and is on
    /// its way to report it -- so the only thing waiting here can cost is the
    /// caller. It cost exactly that: a host that answered `plugin/load` and
    /// then never answered `plugin/unload` parked a starting session for good,
    /// with the refusal it was about to print already in hand.
    async fn unload_best_effort(&self, host: &Arc<PluginHostSupervisor>, plugin_id: &str) {
        if tokio::time::timeout(Self::STANDALONE_LOAD_TIMEOUT, host.unload_plugin(plugin_id))
            .await
            .is_err()
        {
            tracing::warn!(
                plugin = plugin_id,
                "the plugin host did not answer the unload; leaving it to the plane's shutdown"
            );
        }
    }

    /// How long a person waits behind one plugin call before being told the
    /// plugin did not answer.
    ///
    /// A `prompt` command and a `service/call` are both "someone typed
    /// something and is looking at the screen", so both get the same bound and
    /// the same number as the plane's other waits. A tool the model invoked
    /// deliberately gets none: how long a tool runs is the tool's business.
    ///
    /// A call timing out does **not** mark the plane failed. A boot that fails
    /// says the plane is unusable and every later caller should be spared the
    /// cost; one slow command says nothing about the other plugins, their
    /// tools, or their providers.
    pub const UNARY_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

    /// How much longer than the inner bound the borrowed thread gets. The
    /// inner bound is what answers a slow plugin; reaching this one means the
    /// thread itself is stuck, which is a defect in this process rather than
    /// in the plugin.
    const PROXY_JOIN_EXTRA: std::time::Duration = std::time::Duration::from_secs(5);

    /// Loads one plugin that is not part of the composition.
    ///
    /// A package's model provider is a plugin on this host but not a member of
    /// the composition: it mounts into no realm and the control plugin has
    /// never heard of it. So it skips the one step in [`Self::load_entry`]
    /// that only a composition can answer — asking the control plugin to
    /// report on it — and does everything else: load, open a scope to be
    /// called on, and register what the ready report carries.
    ///
    /// How much of that registration reaches rebon's seats is the entry's own
    /// `publish` flag rather than a property of being standalone. Its one
    /// caller loads a model provider with `publish` off, so what it registers
    /// is routing only: rebon learns which plugin answers to which name and
    /// nothing joins a menu.
    ///
    /// Idempotent: asking for one that is already up is how a second session
    /// reaches the same adapter rather than a second copy of it.
    pub async fn load_standalone(&self, entry: &ComposeEntry) -> Result<(), HostCallError> {
        // One at a time, and checked under the same lock the insert takes:
        // two sessions resolving the same provider at once must not both send
        // a `plugin/load` for it, because the second one is refused as already
        // loaded and the session that sent it would read that as a failure.
        let _one_at_a_time = self.standalone_gate.lock().await;
        if self
            .standalone
            .lock()
            .expect("plane standalone table")
            .contains(&entry.id)
        {
            return Ok(());
        }
        self.refuse_if_stuck(&entry.id)?;
        let host = self.host_for_entry(entry).await?;
        if let Err(error) = self.begin_attempt(&host, entry).await {
            self.leave_container(&entry.id).await;
            return Err(error);
        }
        let outcome = self.load_standalone_on(&host, entry).await;
        match &outcome {
            Ok(()) => self.record_lifecycle(&entry.id, PluginLifecycle::Ready),
            Err(error) => {
                self.leave_container(&entry.id).await;
                self.record_lifecycle(
                    &entry.id,
                    PluginLifecycle::Failed {
                        reason: error.to_string(),
                    },
                );
            }
        }
        outcome
    }

    async fn load_standalone_on(
        &self,
        host: &Arc<PluginHostSupervisor>,
        entry: &ComposeEntry,
    ) -> Result<(), HostCallError> {
        self.declare_settings(entry);
        // Bounded, because the caller is a session starting and the thing it
        // is waiting on is a separate process. A host that takes the request
        // and never answers used to park that caller for good: no CPU, no
        // connection, no message -- just a rebon whose only remaining job was
        // to explain itself, waiting on a reply that was never coming.
        let ready = match tokio::time::timeout(
            Self::STANDALONE_LOAD_TIMEOUT,
            host.load_plugin(&entry.load_request()),
        )
        .await
        {
            Ok(ready) => ready?,
            Err(_) => {
                self.undeclare_settings(&entry.id);
                return Err(HostCallError::HostUnanswered {
                    plugin_id: entry.id.clone(),
                    seconds: Self::STANDALONE_LOAD_TIMEOUT.as_secs(),
                });
            }
        };
        // A standalone entry is loaded for one reason: something selected the
        // provider it carries. `ready.llm_providers` is the wire layer's one
        // answer about an adapter — may this plugin serve this provider — so a
        // declared name missing from it means `activate` registered nothing to
        // call. Refused here, at the load a session is waiting on, rather than
        // bound into a client whose first turn dies with `[UNDECLARED_ADAPTER]`
        // after the person already watched a turn start.
        if let Some(missing) = entry
            .llm_providers
            .iter()
            .find(|declared| !ready.llm_providers.contains(*declared))
        {
            self.undeclare_settings(&entry.id);
            self.unload_best_effort(host, &entry.id).await;
            return Err(HostCallError::UnregisteredProvider {
                plugin_id: entry.id.clone(),
                provider: missing.clone(),
            });
        }
        host.open_scope(&entry.id, &self.scope, &self.workspace_root)
            .await?;
        // The same registration a composition entry gets, minus the half that
        // only a composition can answer: model catalogs and prompt sections
        // come from the control plugin's own report, and there is no control
        // plugin behind this one. Everything a plugin reports for itself —
        // tools, commands, services — lands on the same seats either way,
        // which is the point of the plugin model being one model.
        if let Err(error) = self.register(
            host,
            &entry.id,
            &ready,
            &Value::Null,
            entry.publish,
            crate::mods::ModsRegistry::entry_is_mod(entry),
        ) {
            self.withdraw(&entry.id);
            self.undeclare_settings(&entry.id);
            self.unload_best_effort(host, &entry.id).await;
            return Err(error);
        }
        self.standalone
            .lock()
            .expect("plane standalone table")
            .insert(entry.id.clone());
        Ok(())
    }

    /// The slash commands one loaded entry registered, by name.
    pub fn registered_commands(&self, plugin_id: &str) -> Vec<String> {
        self.registered
            .lock()
            .expect("plane registrations poisoned")
            .get(plugin_id)
            .map(|record| record.commands.clone())
            .unwrap_or_default()
    }

    /// The services one loaded entry published, by name.
    pub fn registered_services(&self, plugin_id: &str) -> Vec<String> {
        self.registered
            .lock()
            .expect("plane registrations poisoned")
            .get(plugin_id)
            .map(|record| record.services.clone())
            .unwrap_or_default()
    }

    /// The entries this plane currently has up, in load order.
    pub fn loaded_entries(&self) -> Vec<ComposeEntry> {
        self.loaded
            .lock()
            .expect("plane loaded table")
            .values()
            .cloned()
            .collect()
    }

    /// The composition's generation. Bumped by every reload that changed
    /// something.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Brings the running composition in line with `wanted`.
    ///
    /// Restarts exactly what changed. An entry whose configuration is identical
    /// keeps running — not restarting it is why this diffs rather than
    /// reloading everything, because a restart costs a plugin whatever state it
    /// was holding.
    ///
    /// Order is the same order a first boot uses, for the same reason: removals
    /// go in reverse so a plugin is never taken down before something that
    /// might still be calling it, and additions go in configuration order so a
    /// dependency is up before its dependent.
    ///
    /// There is no atomic two-generation swap, and there cannot be one across a
    /// process boundary: a candidate composition would need its own realm and
    /// its own host. What this promises instead is what the kernel's reconciler
    /// promised — one mutator at a time, a monotone generation, and a report of
    /// exactly what moved.
    pub async fn reload(&self, wanted: &[ComposeEntry]) -> Result<ReloadOutcome, HostCallError> {
        let _one_at_a_time = self.reconcile.lock().await;
        // A plugin on a host that died is no longer running, whatever the
        // table says; the diff has to see that to start it again.
        self.sweep_dead_hosts().await;

        let current = self.loaded.lock().expect("plane loaded table").clone();
        // Classify first, act second: the diff is computed against one
        // consistent view rather than against a table being mutated underneath.
        let plan = classify(&current, wanted);
        let mut outcome = ReloadOutcome {
            generation: self.generation(),
            added: plan.added,
            changed: plan.changed,
            removed: plan.removed,
            unchanged: plan.unchanged,
            failed: Vec::new(),
            forced: Vec::new(),
            stuck: Vec::new(),
        };
        let mut to_start: Vec<&ComposeEntry> =
            plan.start.iter().map(|index| &wanted[*index]).collect();

        for id in &plan.stop {
            // Already ended with another plugin's container, earlier in this
            // same loop; it is reported once, as forced.
            if outcome.forced.contains(id) {
                continue;
            }
            // A plugin that will not drain is not a reason to leave the rest
            // of the composition half-reconciled: each ending is reported and
            // the reload goes on.
            match self.unload_entry(id).await {
                UnloadOutcome::Clean => {}
                UnloadOutcome::Forced { taken_down, .. } => {
                    outcome.forced.push(id.clone());
                    // Its container's other plugins went with it. Those the
                    // composition still wants come back up, in a new one, so
                    // none of them was left running untouched.
                    for other in taken_down {
                        outcome.unchanged.retain(|unchanged| *unchanged != other);
                        if let Some(entry) = wanted.iter().find(|entry| entry.id == other) {
                            if !to_start.iter().any(|start| start.id == other) {
                                to_start.push(entry);
                            }
                        }
                        outcome.forced.push(other);
                    }
                }
                UnloadOutcome::Stuck { .. } => outcome.stuck.push(id.clone()),
            }
        }

        for entry in to_start {
            if let Err(error) = self.load_entry(entry).await {
                outcome
                    .failed
                    .push((entry.id.clone(), format!("load failed: {error}")));
            }
        }

        if outcome.touched_anything() {
            outcome.generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        }
        Ok(outcome)
    }

    /// Opens one session for every loaded plugin.
    ///
    /// Every plugin, not only the ones that ask: a plugin acting on its own
    /// schedule — an agent loop running a turn, an adapter fetching a
    /// credential mid-turn — speaks for the session it is attached to, and
    /// without one it has no identity to speak with.
    pub async fn open_session(
        &self,
        scope_id: &str,
        workspace_root: &str,
    ) -> Result<(), HostCallError> {
        for plugin_id in self.loaded() {
            self.hosts
                .for_plugin(&plugin_id)
                .open_scope(&plugin_id, scope_id, workspace_root)
                .await?;
        }
        Ok(())
    }

    /// Every plugin currently loaded, control plugin first.
    pub fn loaded(&self) -> Vec<String> {
        let mut ids = vec![COMPOSE_PLUGIN_ID.to_owned()];
        ids.extend(
            self.registered
                .lock()
                .expect("plane registrations poisoned")
                .keys()
                .cloned(),
        );
        ids
    }

    /// Winds the host down and withdraws every registration the composition
    /// made, whichever way it ends.
    pub async fn shutdown(&self) {
        let _ = self.hosts.main().shutdown().await;
        let containers: Vec<ContainerHost> = std::mem::take(&mut *self.containers.lock().await)
            .into_values()
            .collect();
        for container in containers {
            let _ = container.supervisor.shutdown().await;
        }
        // Bound to a local first. A guard created inside the `for` expression
        // lives until the loop ends, and `withdraw` takes the same lock — which
        // is a deadlock rather than an error, because `std::sync::Mutex` is not
        // reentrant and simply parks.
        let loaded: Vec<String> = self
            .registered
            .lock()
            .expect("plane registrations poisoned")
            .keys()
            .cloned()
            .collect();
        for plugin_id in loaded {
            self.withdraw(&plugin_id);
        }
    }

    /// Asks the composition for the facts the ready report has no field for.
    async fn report_for(
        &self,
        host: &Arc<PluginHostSupervisor>,
        plugin_id: &str,
    ) -> Result<Value, HostCallError> {
        let payload = tokio::time::timeout(
            REPORT_TIMEOUT,
            host.call_service(
                COMPOSE_PLUGIN_ID,
                &self.scope,
                "compose",
                Payload::from(serde_json::json!({ "kind": "report", "pluginId": plugin_id })),
            ),
        )
        .await
        .map_err(|_| {
            HostCallError::Malformed(format!(
                "the composition did not report on {plugin_id} in time"
            ))
        })??;
        payload.to_value().map_err(|error| {
            HostCallError::Malformed(format!("composition report is not JSON: {error}"))
        })
    }

    /// `is_mod` says the entry is a Claude Code mod: its tools and commands
    /// are seated by the mods registry rather than here, which re-seats them
    /// when a `$.tool.register` or `$.command.register` refines one at run
    /// time; the plane still routes them.
    #[allow(clippy::too_many_arguments)]
    fn register(
        &self,
        host: &Arc<PluginHostSupervisor>,
        plugin_id: &str,
        ready: &PluginReadyReport,
        extras: &Value,
        publish: bool,
        is_mod: bool,
    ) -> Result<(), HostCallError> {
        let mut record = Registrations {
            published: publish,
            ..Registrations::default()
        };
        if !publish {
            // Routing still needs to know who provides what; rebon's tables do
            // not learn about it.
            record.tools = ready.tools.iter().map(|tool| tool.name.clone()).collect();
            // Routing by name still needs to know what this entry answers to.
            record.services = ready.services.clone();
            record.providers = extras
                .get("providers")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|p| p.get("provider").and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            self.registered
                .lock()
                .expect("plane registrations poisoned")
                .insert(plugin_id.to_owned(), record);
            return Ok(());
        }

        // Recorded whichever way this goes, and that is the point: a
        // registration that failed half way through has a scope, some
        // declared tools and maybe a route behind it, and `withdraw` can only
        // undo what it can find. Returning before recording would leak
        // exactly those — which is what the old token path did.
        let outcome = self.register_published(host, plugin_id, ready, extras, &mut record, is_mod);
        self.registered
            .lock()
            .expect("plane registrations poisoned")
            .insert(plugin_id.to_owned(), record);
        outcome
    }

    /// The published half: the entry's scope, its tools on the kernel seat,
    /// its model routes and its prompt sections.
    fn register_published(
        &self,
        host: &Arc<PluginHostSupervisor>,
        plugin_id: &str,
        ready: &PluginReadyReport,
        extras: &Value,
        record: &mut Registrations,
        is_mod: bool,
    ) -> Result<(), HostCallError> {
        // One scope per entry, so what it registered leaves in one statement
        // when it unloads — the kernel's own answer to the question the old
        // `token` field was invented for.
        let scope = self.ctx.fork(&format!("node/{plugin_id}"));
        record.scope = Some(scope.clone());
        let mut proxies: Vec<Arc<dyn rebon_tool::Tool>> = Vec::with_capacity(ready.tools.len());
        for tool in &ready.tools {
            // Read-only is not a field a ready report has, so it is `false`
            // here for the same reason it was `false` before: an undeclared
            // stance routes the call through Ask.
            let Some(proxy) = self.tools.declare(
                &tool.name,
                tool.description.clone(),
                tool.input_schema.to_value().ok(),
                false,
            ) else {
                return Err(HostCallError::Malformed(format!(
                    "{plugin_id} reported a tool with no name"
                )));
            };
            proxies.push(proxy);
            record.tools.push(tool.name.clone());
        }
        if !proxies.is_empty() && !is_mod {
            match self.ctx.get::<ToolSeatService>() {
                Some(seat) => seat
                    .register_tools(
                        &scope,
                        &format!("node/{plugin_id}"),
                        Priority::Plugin,
                        proxies,
                    )
                    .map_err(|error| {
                        HostCallError::Malformed(format!(
                            "registering the tools of {plugin_id}: {error}"
                        ))
                    })?,
                // A kernel with no `core-tools` plugin has no process seat —
                // the plane's own tests assemble one that way. The table still
                // holds what was declared, which is what dispatch reads.
                None => tracing::debug!(
                    plugin = plugin_id,
                    "no process tool seat; composition tools stay in the plane's table only"
                ),
            }
        }

        // A mod's one service is the registry's line to it, called by plugin
        // id (`ModsRegistry::call_mod`); published on the kernel it would
        // answer to no one, and every mod after the first would collide on
        // the name.
        if !is_mod {
            self.register_commands(host, plugin_id, ready, &scope, record)?;
            self.register_services(host, plugin_id, ready, &scope, record);
        }

        for provider in extras
            .get("providers")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let Some(name) = provider.get("provider").and_then(Value::as_str) else {
                continue;
            };
            // Host first, route second, and that order is the contract: the
            // route is what a resolution looks up, so publishing it last means
            // "route visible" implies "host registered" for every reader,
            // including one on another thread landing between the two. The
            // other way round left a window a resolution had to sleep through.
            // `withdraw` is the mirror image -- route out first, host after --
            // so neither direction ever shows a route with nothing behind it.
            register_llm_host(
                name.to_owned(),
                Arc::new(LlmRoute {
                    supervisor: Arc::clone(host),
                    plugin_id: plugin_id.to_owned(),
                    provider: name.to_owned(),
                    scope: self.scope.clone(),
                }),
            );
            if let Err(error) = self.ctx.call_json(
                "model-router",
                "register",
                serde_json::json!({
                    "provider": name,
                    "models": provider.get("models").cloned().unwrap_or(Value::Array(Vec::new())),
                    "defaultModel": provider
                        .get("defaultModel")
                        .and_then(Value::as_str)
                        .unwrap_or("default"),
                }),
            ) {
                // The host went up first, so it has to come back down before
                // this returns: a registered host with no route is invisible,
                // but it is still a route table entry nobody will ever remove.
                unregister_llm_host(name);
                return Err(HostCallError::Malformed(format!(
                    "registering provider {name}: {error}"
                )));
            }
            record.providers.push(name.to_owned());
        }

        for section in extras
            .get("sections")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let Some(name) = section.get("name").and_then(Value::as_str) else {
                continue;
            };
            let answer = self
                .ctx
                .call_json("system-prompt", "register", section.clone())
                .map_err(|error| {
                    HostCallError::Malformed(format!("registering prompt section {name}: {error}"))
                })?;
            let token = answer.get("token").and_then(Value::as_u64).unwrap_or(0);
            record.sections.push((name.to_owned(), token));
        }
        Ok(())
    }

    /// Puts a plugin's slash commands on the kernel's command seat.
    ///
    /// A `prompt` command becomes a proxy that asks the plugin what to send;
    /// the other two answered at registration and carry their answer with
    /// them. All three ride the entry's scope, so unloading the entry takes
    /// them out of every menu without a front end being told.
    ///
    /// A name a built-in already answers to is refused, and the refusal takes
    /// the whole entry down. Built-ins win because a plugin must not be able
    /// to redefine `/help`; the entry goes rather than the one command
    /// because a half-registered plugin is exactly what the load path exists
    /// to prevent.
    fn register_commands(
        &self,
        host: &Arc<PluginHostSupervisor>,
        plugin_id: &str,
        ready: &PluginReadyReport,
        scope: &Context,
        record: &mut Registrations,
    ) -> Result<(), HostCallError> {
        if ready.commands.is_empty() {
            return Ok(());
        }
        let Some(seat) = self.ctx.get::<CommandSeatService>() else {
            // A kernel with no `core-commands` plugin has no command seat —
            // the plane's own tests assemble one that way — and a plugin's
            // commands then have nowhere to go.
            tracing::debug!(
                plugin = plugin_id,
                "no command seat; the plugin's commands are not registered"
            );
            return Ok(());
        };
        for definition in &ready.commands {
            let handler = match &definition.kind {
                PluginCommandKind::Explain { text } => {
                    CommandHandler::Explain(Cow::Owned(text.clone()))
                }
                PluginCommandKind::Panel { dialog } => {
                    CommandHandler::Panel(Cow::Owned(dialog.clone()))
                }
                // Both ask the plugin; the spec's kind says what the answer
                // is (a turn, or text shown and kept from the model).
                PluginCommandKind::Prompt | PluginCommandKind::Output => CommandHandler::Prompt(
                    self.command_proxy(host, plugin_id.to_owned(), definition.name.clone()),
                ),
            };
            seat.register(scope, command_spec(definition), handler)
                .map_err(|error| {
                    HostCallError::Malformed(format!(
                        "[COMMAND_NAME_TAKEN] {plugin_id} registers command /{} which is \
                         already taken: {error}",
                        definition.name
                    ))
                })?;
            record.commands.push(definition.name.clone());
        }
        Ok(())
    }

    /// The proxy behind a `prompt` command.
    ///
    /// Synchronous by contract and asynchronous by nature, so it blocks on a
    /// thread of its own: a command typed in the TUI arrives on a blocking
    /// worker where `block_on` is legal, one from the app's IPC path arrives
    /// on a runtime worker where it panics, and a thread of its own may block
    /// from either. A failure becomes the text the person sees, because a
    /// command that silently expanded to nothing would look like rebon
    /// ignoring them.
    fn command_proxy(
        &self,
        host: &Arc<PluginHostSupervisor>,
        plugin_id: String,
        name: String,
    ) -> Arc<dyn Fn(&CommandArgs) -> Result<String, String> + Send + Sync> {
        // The four handles the call needs, cloned once rather than a handle on
        // the plane: a proxy that held the plane would keep the host alive for
        // as long as any menu remembered the command.
        command_proxy(
            Arc::clone(host),
            self.scope.clone(),
            self.runtime.clone(),
            self.unary_call_timeout,
            plugin_id,
            name,
        )
    }
}

/// The proxy behind a plugin's `prompt` command.
///
/// Shared by the plane (every plugin's commands) and the mods registry (a
/// mod's, which it re-seats): one way of asking a plugin what a command
/// expands to.
pub(crate) fn command_proxy(
    supervisor: Arc<PluginHostSupervisor>,
    plane_scope: String,
    runtime: tokio::runtime::Handle,
    bound: Duration,
    plugin_id: String,
    name: String,
) -> Arc<dyn Fn(&CommandArgs) -> Result<String, String> + Send + Sync> {
    {
        Arc::new(move |args: &CommandArgs| {
            let request = CommandInvokeRequest {
                name: name.clone(),
                raw: args.raw.clone(),
                rest: args.rest.clone(),
                surface: wire_surface(args.surface),
            };
            let supervisor = Arc::clone(&supervisor);
            let scope = plane_scope.clone();
            let plugin = plugin_id.clone();
            let runtime = runtime.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let outcome = runtime.block_on(supervisor.invoke_command_bounded(
                    &plugin,
                    &scope,
                    request,
                    Some(bound),
                ));
                let _ = tx.send(outcome);
            });
            match rx.recv_timeout(bound + PluginPlane::PROXY_JOIN_EXTRA) {
                Ok(Ok(payload)) => match payload.to_value() {
                    Ok(Value::String(text)) => Ok(text),
                    Ok(Value::Null) => Ok(String::new()),
                    Ok(other) => Ok(other.to_string()),
                    Err(error) => Err(format!(
                        "/{name} answered with something unreadable: {error}"
                    )),
                },
                Ok(Err(error)) => Err(format!("/{name} failed: {error}")),
                Err(_) => {
                    // The inner bound should have answered long before this.
                    // Reaching here means the borrowed thread never got that
                    // far -- a runtime this process is holding shut, not a slow
                    // plugin -- so it is logged as the defect it is.
                    tracing::error!(
                        command = %name,
                        "the plugin call thread did not return within the grace period"
                    );
                    Err(format!(
                        "/{name} failed: {} the call did not return.",
                        rebon_plugin_supervisor::HOST_UNANSWERED_CODE
                    ))
                }
            }
        })
    }
}

impl PluginPlane {
    /// Publishes a plugin's services on the entry's scope.
    ///
    /// On the entry's own fork rather than the kernel root, which is the
    /// conservative half of "the same model as a built-in": the registration
    /// is real and disposes with the entry, and it cannot shadow a kernel
    /// service, because a name already answered on an ancestor layer is
    /// refused here and said out loud rather than silently winning.
    fn register_services(
        &self,
        host: &Arc<PluginHostSupervisor>,
        plugin_id: &str,
        ready: &PluginReadyReport,
        scope: &Context,
        record: &mut Registrations,
    ) {
        for name in &ready.services {
            let proxy = Arc::new(PlaneServiceProxy {
                bound: self.unary_call_timeout,
                supervisor: Arc::clone(host),
                plugin_id: plugin_id.to_owned(),
                scope: self.scope.clone(),
                service: name.clone(),
                runtime: self.runtime.clone(),
            });
            match scope.provide_json(name, proxy) {
                Ok(()) => record.services.push(name.clone()),
                Err(error) => tracing::warn!(
                    plugin = plugin_id,
                    service = %name,
                    %error,
                    "plugin service not published; the name is taken"
                ),
            }
        }
    }

    /// Best-effort throughout: a composition is going away either way, and one
    /// seat refusing a withdrawal must not strand the others.
    fn withdraw(&self, plugin_id: &str) {
        if let Some(mods) = self.mods() {
            mods.detach(plugin_id);
        }
        let Some(record) = self
            .registered
            .lock()
            .expect("plane registrations poisoned")
            .remove(plugin_id)
        else {
            return;
        };
        if !record.published {
            // Nothing of this entry ever reached rebon's tables, so there is
            // nothing to take out of them.
            return;
        }
        // The scope first, and it is most of the work: the entry's tools, its
        // slash commands and its services were all registered on it, so one
        // dispose takes all three out. A call already inside one of those is
        // answered by what it is inside, and the seat's own guard turns the
        // next resolution into `[STALE_PROVIDER]` rather than a silence.
        if let Some(scope) = record.scope {
            scope.dispose();
        }
        for name in record.tools {
            self.tools.withdraw(&name);
        }
        for provider in record.providers {
            let _ = self.ctx.call_json(
                "model-router",
                "unregister",
                serde_json::json!({ "provider": provider }),
            );
            unregister_llm_host(&provider);
        }
        for (name, token) in record.sections {
            let _ = self.ctx.call_json(
                "system-prompt",
                "unregister",
                serde_json::json!({ "name": name, "token": token }),
            );
        }
    }

    /// Who serves a target, and by which method.
    ///
    /// One lookup for both because the composition's dispatchable surface is
    /// one namespace to a caller: a dsh tool and a web provider are both "a
    /// name the composition answers to", and only the plane needs to know that
    /// one is `tool/call` and the other `service/call`.
    fn owner_of(&self, target: &str) -> Option<(String, Route)> {
        let registered = self
            .registered
            .lock()
            .expect("plane registrations poisoned");
        for (plugin_id, record) in registered.iter() {
            if record.tools.iter().any(|name| name == target) {
                return Some((plugin_id.clone(), Route::Tool));
            }
            if record.services.iter().any(|name| name == target) {
                return Some((plugin_id.clone(), Route::Service));
            }
        }
        None
    }

    /// Opens one session for the plugins named, rather than for all of them.
    ///
    /// A per-session loop's entries belong to that session and no other; giving
    /// them every session's scope would let one session's events reach another.
    pub async fn open_session_for(
        &self,
        plugin_ids: &[String],
        scope_id: &str,
        workspace_root: &str,
    ) -> Result<(), HostCallError> {
        for plugin_id in plugin_ids {
            self.hosts
                .for_plugin(plugin_id)
                .open_scope(plugin_id, scope_id, workspace_root)
                .await?;
        }
        Ok(())
    }

    /// Calls one service a composition entry registered.
    ///
    /// This is how rebon drives a composition: `loop:control` for an agent
    /// loop, `compose` for the control plugin, whatever else an entry declared.
    pub async fn call_service(
        &self,
        plugin_id: &str,
        scope_id: &str,
        service: &str,
        request: Value,
    ) -> Result<Value, HostCallError> {
        let payload = self
            .hosts
            .for_plugin(plugin_id)
            .call_service(plugin_id, scope_id, service, Payload::from(request))
            .await?;
        payload.to_value().map_err(|error| {
            HostCallError::Malformed(format!("service answer is not JSON: {error}"))
        })
    }
}

/// One plugin command definition as the command seat's own type.
///
/// A straight field-for-field move, which is the point: the wire shape was
/// drawn from `CommandSpec` so that turning one into the other could never
/// become a place where a plugin's command quietly means something else than
/// a built-in's.
fn command_spec(definition: &PluginCommandDefinition) -> CommandSpec {
    let mut spec = CommandSpec::new(definition.name.clone(), definition.description.clone())
        .aliases(definition.aliases.iter().cloned())
        .zh_aliases(definition.zh_aliases.iter().cloned())
        .category(match definition.category {
            rebon_plugin_protocol::PluginCommandCategory::Command => Category::Command,
            rebon_plugin_protocol::PluginCommandCategory::Agent => Category::Agent,
        })
        .kind(match definition.kind {
            PluginCommandKind::Prompt => CommandKind::Prompt,
            PluginCommandKind::Explain { .. } => CommandKind::Explain,
            PluginCommandKind::Panel { .. } => CommandKind::Panel,
            // The kind a mod's command has: the answer is the command's
            // output, shown where it was typed.
            PluginCommandKind::Output => CommandKind::Session,
        });
    if let Some(hint) = &definition.hint {
        spec = spec.hint(hint.clone());
    }
    // An empty list means the default a built-in takes, rather than "nowhere":
    // a command that works on no surface is one nobody can run, and no plugin
    // means that by leaving the field out.
    if !definition.surfaces.is_empty() {
        let surfaces = definition
            .surfaces
            .iter()
            .fold(Surfaces::NONE, |acc, surface| {
                acc.with(surface_set(*surface))
            });
        spec = spec.surfaces(surfaces);
    }
    spec
}

/// One wire surface as a one-element set.
fn surface_set(surface: PluginCommandSurface) -> Surfaces {
    match surface {
        PluginCommandSurface::Tui => Surfaces::TUI_ONLY,
        PluginCommandSurface::Desktop => Surfaces::DESKTOP_ONLY,
        PluginCommandSurface::Acp => Surfaces::ACP_ONLY,
        PluginCommandSurface::Web => Surfaces::WEB,
        PluginCommandSurface::Mobile => Surfaces::MOBILE,
    }
}

/// The surface a command was typed on, as the wire names it.
///
/// `SessionControl` is not a front end — it is the set a mirror forwards — so
/// it has no wire spelling and reads as the terminal, which is the surface a
/// forwarded command was typed on.
fn wire_surface(surface: Surface) -> PluginCommandSurface {
    match surface {
        Surface::Tui | Surface::SessionControl => PluginCommandSurface::Tui,
        Surface::Desktop => PluginCommandSurface::Desktop,
        Surface::Acp => PluginCommandSurface::Acp,
        Surface::Web => PluginCommandSurface::Web,
        Surface::Mobile => PluginCommandSurface::Mobile,
    }
}

/// Whether a failed composition report means "the realm never mounted this".
///
/// Two things can answer `[UNKNOWN_PLUGIN]` to a report, and only one of them
/// is harmless:
///
///   * the **host** answered, refusing the call the composition control made
///     on rebon's behalf ([`HostCallError::Rejected`]) — the entry loaded and
///     is simply not a Cordis plugin, which is the case this exists for;
///   * **rebon's own registry** refused to send the call at all
///     ([`HostCallError::Registry`]) — the composition control itself is not
///     loaded, which is a broken plane and must not be waved through.
///
/// Both carry the same code, so the variant is what separates them; the code
/// is then matched rather than the message, which is written for a person and
/// may be reworded without warning.
fn entry_was_not_mounted_by_the_composition(error: &HostCallError) -> bool {
    let HostCallError::Rejected { payload, .. } = error else {
        return false;
    };
    payload
        .to_value()
        .ok()
        .and_then(|value| value.get("code").and_then(Value::as_str).map(str::to_owned))
        .as_deref()
        == Some(rebon_plugin_protocol::UNKNOWN_PLUGIN_CODE)
}

/// A service a plugin registered, answered off the JSON plane.
///
/// Blocking for the same reason the command proxy is: `JsonService::call` is
/// synchronous and reaching a plugin is not.
struct PlaneServiceProxy {
    /// The deadline this proxy puts on one call; see
    /// [`PluginPlane::UNARY_CALL_TIMEOUT`].
    bound: std::time::Duration,
    supervisor: Arc<PluginHostSupervisor>,
    plugin_id: String,
    scope: String,
    service: String,
    runtime: tokio::runtime::Handle,
}

impl JsonService for PlaneServiceProxy {
    fn call(&self, method: &str, params: Value) -> Result<Value, KernelError> {
        let supervisor = Arc::clone(&self.supervisor);
        let plugin_id = self.plugin_id.clone();
        let scope = self.scope.clone();
        let service = self.service.clone();
        let runtime = self.runtime.clone();
        let bound = self.bound;
        // The method rides the request rather than the routing key: a plugin
        // service is one handler, and which of its methods is being called is
        // that handler's business, exactly as it is for `service/call`.
        let request = Payload::from(serde_json::json!({ "method": method, "params": params }));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = runtime.block_on(supervisor.call_service_bounded(
                &plugin_id,
                &scope,
                &service,
                request,
                Some(bound),
            ));
            let _ = tx.send(outcome);
        });
        match rx.recv_timeout(self.bound + PluginPlane::PROXY_JOIN_EXTRA) {
            Ok(Ok(payload)) => payload
                .to_value()
                .map_err(|error| KernelError::Other(format!("{error}"))),
            Ok(Err(error)) => Err(KernelError::Other(format!("{error}"))),
            Err(_) => {
                // See the command proxy: the inner bound answers a slow plugin,
                // so this one only fires on a stuck thread of our own.
                tracing::error!(
                    service = %self.service,
                    "the plugin service thread did not return within the grace period"
                );
                Err(KernelError::Other(format!(
                    "{} the service call did not return.",
                    rebon_plugin_supervisor::HOST_UNANSWERED_CODE
                )))
            }
        }
    }
}

/// Which method reaches a dispatchable name.
#[derive(Clone, Copy, Debug)]
enum Route {
    Tool,
    Service,
}

/// The seats this build lets a plugin call.
///
/// A closed set, and a short one: the registration faces (`tool-registry`,
/// `model-router`, `system-prompt`) are not seats a plugin calls any more —
/// what it provides is reported, and rebon registers it. What is left is what a
/// plugin genuinely consumes.
pub fn default_exposed_seats() -> Vec<String> {
    vec![
        "credentials".to_owned(),
        "logger".to_owned(),
        "settings".to_owned(),
        rebon_types::MODS_SEAT.to_owned(),
    ]
}

/// Kernel seats, answered off the JSON plane.
///
/// Two gates have already run by the time this is called — the plugin declared
/// the seat, and this build exposes it — so what is left is resolving the
/// service and calling it. A seat that does not exist is a refusal rather than
/// a panic: which seats a kernel provides depends on how it was assembled.
struct KernelSeats {
    ctx: Context,
    /// The mods registry, which answers the `mods` seat itself: its methods
    /// run child processes and HTTP requests, which a synchronous JSON
    /// service on the reader's thread could not.
    mods: Arc<std::sync::OnceLock<Arc<crate::mods::ModsRegistry>>>,
}

impl SeatDispatcher for KernelSeats {
    fn call(
        &self,
        invocation: SeatInvocation,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Payload, ToolRefusal>> + Send + '_>,
    > {
        let mut params = invocation.params.to_value().unwrap_or(Value::Null);
        if invocation.seat == rebon_types::MODS_SEAT {
            return match self.mods.get() {
                Some(mods) => {
                    mods.seat_call(&invocation.identity.plugin_id, &invocation.method, params)
                }
                None => Box::pin(async move {
                    Err(ToolRefusal::new(
                        "[UNAVAILABLE_SEAT]",
                        "the mods seat is not installed on this plane",
                    ))
                }),
            };
        }
        // Who is calling is the host's answer, not the plugin's. Written over
        // whatever the params carried, so a plugin cannot reach another
        // plugin's settings namespace by naming it here — see
        // `kernel_config_seats::CALLER_PLUGIN_ID`.
        if !params.is_object() {
            params = Value::Object(serde_json::Map::new());
        }
        params[rebon_kernel_seats::kernel_config_seats::CALLER_PLUGIN_ID] =
            Value::String(invocation.identity.plugin_id.clone());
        let outcome = self
            .ctx
            .call_json(&invocation.seat, &invocation.method, params);
        Box::pin(async move {
            match outcome {
                Ok(value) => Ok(Payload::from(value)),
                Err(error) => Err(ToolRefusal::new("[SEAT_FAILED]", error.to_string())),
            }
        })
    }
}

/// Published events, put on the kernel's event plane.
///
/// The topic is namespaced by the publishing plugin so a composition entry
/// cannot announce something a kernel component would mistake for its own.
struct KernelEvents {
    ctx: Context,
}

impl EventPublisher for KernelEvents {
    fn publish(
        &self,
        event: PublishedEvent,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Payload, ToolRefusal>> + Send + '_>,
    > {
        let payload = serde_json::json!({
            "pluginId": event.identity.plugin_id,
            "scopeId": event.identity.scope_id,
            "event": event.event.to_value().unwrap_or(Value::Null),
        });
        self.ctx.emit_json(&event.topic, &payload);
        Box::pin(async move { Ok(Payload::from(serde_json::json!({ "published": true }))) })
    }
}

/// Runs a composition tool through `tool/call`.
struct PlaneToolDispatch {
    hosts: PlaneHosts,
    owner: std::sync::Weak<PluginPlane>,
}

#[async_trait]
impl ComposeToolDispatch for PlaneToolDispatch {
    async fn serve(&self, tool: &str, input: Value, timeout: Duration) -> Result<Value, String> {
        let Some(plane) = self.owner.upgrade() else {
            return Err("the plugin plane serving this tool is gone".to_owned());
        };
        let (owner, route) = plane
            .owner_of(tool)
            .ok_or_else(|| format!("no composition entry provides {tool}"))?;
        let payload = Payload::from(input);
        let scope = plane.scope().to_owned();
        let host = self.hosts.for_plugin(&owner);
        let call: std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>> = match route {
            Route::Tool => Box::pin(host.call_tool(&owner, &scope, tool, payload)),
            // A composition tool reached through a service: the caller's own
            // `timeout` below is this call's bound, and a tool's length is the
            // tool's business.
            Route::Service => Box::pin(host.call_service(&owner, &scope, tool, payload)),
        };
        match tokio::time::timeout(timeout, call).await {
            Err(_) => Err(format!("composition tool {tool} did not answer in time")),
            Ok(Ok(payload)) => Ok(serde_json::json!({
                "ok": payload.to_value().map_err(|error| error.to_string())?
            })),
            // The plugin's own code and message travel in the payload; a caller
            // reading "the call failed" learns nothing it can act on.
            Ok(Err(error)) => Err(error.to_string()),
        }
    }

    fn has_exited(&self) -> bool {
        // A supervisor that has failed answers everything loudly on its own, so
        // this only reports the case where nothing is left to ask.
        self.owner.upgrade().is_none()
    }
}

#[cfg(test)]
mod report_failure_tests {
    use super::*;
    use rebon_plugin_protocol::{RegistryError, TerminalStatus, UNKNOWN_PLUGIN_CODE};

    fn host_refused(code: &str) -> HostCallError {
        HostCallError::Rejected {
            status: TerminalStatus::Error,
            payload: Payload::from(serde_json::json!({
                "code": code,
                "message": "whatever the other side felt like saying",
            })),
        }
    }

    /// The case the whole branch exists for: a composition entry that is a
    /// plugin in rebon's own shape rather than a Cordis one. The host loaded
    /// it, the realm never mounted it, and the control plugin says so.
    #[test]
    fn a_host_native_entry_is_recognised_by_the_code_not_the_prose() {
        assert!(entry_was_not_mounted_by_the_composition(&host_refused(
            UNKNOWN_PLUGIN_CODE
        )));
    }

    /// Any other refusal is a refusal. The composition answered and said no
    /// for a reason that has nothing to do with which loader took the entry.
    #[test]
    fn another_refusal_from_the_composition_is_still_a_failure() {
        assert!(!entry_was_not_mounted_by_the_composition(&host_refused(
            "[UNKNOWN_CONTROL]"
        )));
        assert!(!entry_was_not_mounted_by_the_composition(&host_refused("")));
    }

    /// The trap: rebon's own registry answers the same code, and it means the
    /// opposite. `[UNKNOWN_PLUGIN]` from there is the *control plugin* missing
    /// — the call never left this process — which is a plane with nothing
    /// behind it, not an entry that simply is not Cordis.
    #[test]
    fn the_same_code_from_rebons_own_registry_is_not_waved_through() {
        let never_sent = HostCallError::Registry {
            source: RegistryError::UnknownPlugin {
                plugin_id: COMPOSE_PLUGIN_ID.to_string(),
            },
        };
        assert!(
            never_sent.to_string().contains(UNKNOWN_PLUGIN_CODE),
            "the premise of this test: both carry the same code -- {never_sent}"
        );
        assert!(!entry_was_not_mounted_by_the_composition(&never_sent));
    }

    /// A rejection with no readable code is not a licence to continue.
    #[test]
    fn a_refusal_that_carries_no_code_is_a_failure() {
        let shapeless = HostCallError::Rejected {
            status: TerminalStatus::Error,
            payload: Payload::from(serde_json::json!("just a string")),
        };
        assert!(!entry_was_not_mounted_by_the_composition(&shapeless));
    }
}

#[cfg(test)]
mod reload_tests {
    use super::*;

    fn entry(id: &str, config: Value) -> ComposeEntry {
        ComposeEntry {
            id: id.to_string(),
            root: "/packages/demo".to_string(),
            entry: "index.mjs".to_string(),
            config,
            services: vec!["echo".to_string()],
            event_topics: Vec::new(),
            published_topics: Vec::new(),
            llm_providers: Vec::new(),
            tools: Vec::new(),
            commands: Vec::new(),
            invokable_tools: Vec::new(),
            seats: Vec::new(),
            settings: Vec::new(),
            publish: true,
            container: None,
            source: None,
        }
    }

    fn running(entries: &[ComposeEntry]) -> BTreeMap<String, ComposeEntry> {
        entries
            .iter()
            .map(|entry| (entry.id.clone(), entry.clone()))
            .collect()
    }

    /// The point of diffing at all: an entry whose configuration is identical
    /// keeps running. Restarting it would cost the plugin whatever state it was
    /// holding, for no reason.
    #[test]
    fn an_identical_composition_moves_nothing() {
        let entries = vec![entry("a", Value::Null), entry("b", Value::Null)];
        let plan = classify(&running(&entries), &entries);

        assert_eq!(plan.unchanged, vec!["a".to_string(), "b".to_string()]);
        assert!(plan.added.is_empty());
        assert!(plan.changed.is_empty());
        assert!(plan.removed.is_empty());
        assert!(plan.stop.is_empty());
        assert!(plan.start.is_empty());
    }

    #[test]
    fn a_first_reload_onto_nothing_starts_everything_in_configuration_order() {
        let wanted = vec![entry("a", Value::Null), entry("b", Value::Null)];
        let plan = classify(&BTreeMap::new(), &wanted);

        assert_eq!(plan.added, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(plan.start, vec![0, 1]);
        assert!(plan.stop.is_empty());
    }

    /// A changed entry is a stop *and* a start: an id cannot be loaded twice,
    /// so there is no in-place update to make.
    #[test]
    fn a_changed_configuration_stops_and_starts_the_same_id() {
        let before = vec![entry("a", serde_json::json!({"n": 1}))];
        let after = vec![entry("a", serde_json::json!({"n": 2}))];
        let plan = classify(&running(&before), &after);

        assert_eq!(plan.changed, vec!["a".to_string()]);
        assert_eq!(plan.stop, vec!["a".to_string()]);
        assert_eq!(plan.start, vec![0]);
        assert!(plan.unchanged.is_empty());
    }

    #[test]
    fn an_entry_that_left_the_configuration_is_stopped_and_not_restarted() {
        let before = vec![entry("a", Value::Null), entry("gone", Value::Null)];
        let after = vec![entry("a", Value::Null)];
        let plan = classify(&running(&before), &after);

        assert_eq!(plan.removed, vec!["gone".to_string()]);
        assert_eq!(plan.stop, vec!["gone".to_string()]);
        assert_eq!(plan.start, Vec::<usize>::new());
        assert_eq!(plan.unchanged, vec!["a".to_string()]);
    }

    /// Down in reverse of the order things went up, so a plugin is never
    /// stopped before something that might still be calling it.
    #[test]
    fn everything_coming_down_comes_down_in_reverse() {
        let before = vec![
            entry("a", Value::Null),
            entry("b", serde_json::json!({"n": 1})),
            entry("c", Value::Null),
        ];
        let after = vec![entry("b", serde_json::json!({"n": 2}))];
        let plan = classify(&running(&before), &after);

        assert_eq!(plan.changed, vec!["b".to_string()]);
        assert_eq!(plan.removed, vec!["a".to_string(), "c".to_string()]);
        assert_eq!(
            plan.stop,
            vec!["c".to_string(), "b".to_string(), "a".to_string()],
            "removals and the changed entry all come down, latest first"
        );
        assert_eq!(plan.start, vec![0], "and only the changed one goes back up");
    }

    /// One reload can do all four things at once, and each id lands in exactly
    /// one bucket.
    #[test]
    fn added_changed_removed_and_untouched_all_in_one_reload() {
        let before = vec![
            entry("keep", Value::Null),
            entry("edit", serde_json::json!({"n": 1})),
            entry("drop", Value::Null),
        ];
        let after = vec![
            entry("keep", Value::Null),
            entry("edit", serde_json::json!({"n": 2})),
            entry("new", Value::Null),
        ];
        let plan = classify(&running(&before), &after);

        assert_eq!(plan.unchanged, vec!["keep".to_string()]);
        assert_eq!(plan.changed, vec!["edit".to_string()]);
        assert_eq!(plan.added, vec!["new".to_string()]);
        assert_eq!(plan.removed, vec!["drop".to_string()]);

        let mut every = plan.unchanged.clone();
        every.extend(plan.changed.clone());
        every.extend(plan.added.clone());
        every.extend(plan.removed.clone());
        let unique: BTreeSet<&String> = every.iter().collect();
        assert_eq!(unique.len(), every.len(), "an id lands in one bucket only");
    }

    /// `publish` is part of what an entry *is* — a private entry promoted to a
    /// published one has to restart, or its registrations never reach rebon's
    /// seats.
    #[test]
    fn a_container_whose_grants_changed_is_restarted() {
        let spec = crate::container::ContainerSpec {
            id: "snake@rebon".into(),
            data_dir: "/data/snake".into(),
            ..Default::default()
        };
        let mut before = entry("snake", Value::Null);
        before.container = Some(spec.clone());
        let mut after = before.clone();
        after.container = Some(crate::container::ContainerSpec {
            network: vec!["api.example.com".into()],
            ..spec
        });
        let plan = classify(&running(&[before.clone()]), &[after]);
        assert_eq!(plan.changed, vec!["snake".to_string()]);
        // Moving an entry into a container is a change too: it has to leave
        // the shared host for its own.
        let mut shared = before.clone();
        shared.container = None;
        let plan = classify(&running(&[shared]), &[before]);
        assert_eq!(plan.changed, vec!["snake".to_string()]);
    }

    #[test]
    fn publish_is_part_of_the_comparison() {
        let before = vec![entry("a", Value::Null)];
        let mut promoted = entry("a", Value::Null);
        promoted.publish = false;
        let plan = classify(&running(&before), &[promoted]);

        assert_eq!(plan.changed, vec!["a".to_string()]);
    }

    /// A package reinstalled from somewhere else is another plugin under the
    /// same id: it restarts, so its runs say where they came from.
    #[test]
    fn a_source_that_changed_is_restarted() {
        let mut before = entry("a", Value::Null);
        before.source = Some("old".into());
        let mut after = before.clone();
        after.source = Some("new".into());
        let plan = classify(&running(&[before.clone()]), &[after]);
        assert_eq!(plan.changed, vec!["a".to_string()]);
        let plan = classify(&running(&[before.clone()]), &[before]);
        assert_eq!(plan.unchanged, vec!["a".to_string()]);
    }

    /// A reload that changed nothing must not advance the generation: the
    /// number is what a caller compares to know whether the composition moved.
    #[test]
    fn an_outcome_that_touched_nothing_says_so() {
        let quiet = ReloadOutcome {
            generation: 7,
            unchanged: vec!["a".to_string()],
            ..ReloadOutcome::default()
        };
        assert!(!quiet.touched_anything());

        let moved = ReloadOutcome {
            generation: 8,
            changed: vec!["a".to_string()],
            ..ReloadOutcome::default()
        };
        assert!(moved.touched_anything());
    }
}
