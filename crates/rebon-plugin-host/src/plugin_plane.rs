//! The composition, on the plugin plane.
//!
//! This is the embedder half of what `runtimes/node/compose-runtime` does on the other
//! side of the pipe: rebon starts a plugin host, loads the composition control
//! plugin, then loads every configured entry as its own `plugin/load` — and
//! turns what each load reports into registrations on rebon's own seats.
//!
//! # Why one load per entry
//!
//! The composition used to be a single opaque thing that registered into rebon
//! from the inside, over synchronous calls that only worked because it lived in
//! this process. Across a process boundary none of that survives: the
//! unregistrations lived in Cordis disposers, disposers cannot await, and an
//! answer cannot be had without waiting. So the direction is inverted. A load
//! *reports* what an entry provides, rebon registers it here, and unload is
//! what withdraws it — which also makes single-plugin unload, generation and
//! reload protocol operations rather than a control service the composition
//! serves itself.
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
use rebon_kernel::{Context, JsonService, KernelError};
use rebon_plugin_protocol::{
    CommandInvokeRequest, Payload, PluginCommandDefinition, PluginCommandKind,
    PluginCommandSurface, PluginLoadRequest, PluginReadyReport,
};
use rebon_plugin_supervisor::{
    EventPublisher, HostCallError, HostConfig, PluginHostSupervisor, PublishedEvent,
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
    supervisor: Arc<PluginHostSupervisor>,
    ctx: Context,
    /// The workspace the plane's own scope names.
    workspace_root: String,
    /// The scope this plane's own calls travel on.
    scope: String,
    /// The deadline the two unary proxies put on a plugin call.
    unary_call_timeout: std::time::Duration,
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
        let seats = Arc::new(KernelSeats { ctx: ctx.clone() });
        let events = Arc::new(KernelEvents { ctx: ctx.clone() });
        let host = HostConfig::new(config.node.clone(), config.host_script.clone())
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

        let plane = Arc::new(Self {
            supervisor: Arc::clone(&supervisor),
            ctx,
            workspace_root,
            scope,
            unary_call_timeout: config
                .unary_call_timeout
                .unwrap_or(Self::UNARY_CALL_TIMEOUT),
            tools: Arc::clone(&tools),
            registered: Mutex::new(BTreeMap::new()),
            runtime: tokio::runtime::Handle::current(),
            standalone: Mutex::new(BTreeSet::new()),
            standalone_gate: tokio::sync::Mutex::new(()),
            loaded: Mutex::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
            reconcile: tokio::sync::Mutex::new(()),
        });
        tools.bind_plane(Arc::new(PlaneToolDispatch {
            supervisor,
            owner: Arc::downgrade(&plane),
        }));
        Ok(plane)
    }

    pub fn supervisor(&self) -> &Arc<PluginHostSupervisor> {
        &self.supervisor
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
            supervisor: Arc::clone(&self.supervisor),
            owner: Arc::downgrade(self),
        })
    }

    /// Loads one entry and registers what it reported on rebon's seats.
    pub async fn load_entry(&self, entry: &ComposeEntry) -> Result<EntryReport, HostCallError> {
        self.declare_settings(entry);
        let ready = match self.supervisor.load_plugin(&entry.load_request()).await {
            Ok(ready) => ready,
            Err(error) => {
                self.undeclare_settings(&entry.id);
                return Err(error);
            }
        };
        // Before anything is asked of it: the plane's own scope is what a
        // report, a tool call or a model turn rebon initiates travels on.
        self.supervisor
            .open_scope(&entry.id, &self.scope, &self.workspace_root)
            .await?;
        let extras = match self.report_for(&entry.id).await {
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
                self.unload_best_effort(&entry.id).await;
                return Err(error);
            }
        };
        if let Err(error) = self.register(&entry.id, &ready, &extras, entry.publish) {
            // Both sides have to agree about what is loaded. An entry rebon
            // could not finish registering is one rebon will not route to, so
            // leaving it mounted would be a plugin running for nobody — and a
            // reload of the same id would then be refused as already loaded.
            self.withdraw(&entry.id);
            self.undeclare_settings(&entry.id);
            self.unload_best_effort(&entry.id).await;
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
    pub async fn unload_entry(&self, plugin_id: &str) -> Result<(), HostCallError> {
        self.supervisor.unload_plugin(plugin_id).await?;
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
        Ok(())
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
    async fn unload_best_effort(&self, plugin_id: &str) {
        if tokio::time::timeout(
            Self::STANDALONE_LOAD_TIMEOUT,
            self.supervisor.unload_plugin(plugin_id),
        )
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
        self.declare_settings(entry);
        // Bounded, because the caller is a session starting and the thing it
        // is waiting on is a separate process. A host that takes the request
        // and never answers used to park that caller for good: no CPU, no
        // connection, no message -- just a rebon whose only remaining job was
        // to explain itself, waiting on a reply that was never coming.
        let ready = match tokio::time::timeout(
            Self::STANDALONE_LOAD_TIMEOUT,
            self.supervisor.load_plugin(&entry.load_request()),
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
            self.unload_best_effort(&entry.id).await;
            return Err(HostCallError::UnregisteredProvider {
                plugin_id: entry.id.clone(),
                provider: missing.clone(),
            });
        }
        self.supervisor
            .open_scope(&entry.id, &self.scope, &self.workspace_root)
            .await?;
        // The same registration a composition entry gets, minus the half that
        // only a composition can answer: model catalogs and prompt sections
        // come from the control plugin's own report, and there is no control
        // plugin behind this one. Everything a plugin reports for itself —
        // tools, commands, services — lands on the same seats either way,
        // which is the point of the plugin model being one model.
        if let Err(error) = self.register(&entry.id, &ready, &Value::Null, entry.publish) {
            self.withdraw(&entry.id);
            self.undeclare_settings(&entry.id);
            self.unload_best_effort(&entry.id).await;
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
        };
        let to_start: Vec<&ComposeEntry> = plan.start.iter().map(|index| &wanted[*index]).collect();

        for id in &plan.stop {
            if let Err(error) = self.unload_entry(id).await {
                // Report it and keep going: a plugin that will not drain is not
                // a reason to leave the rest of the composition half-reconciled.
                outcome
                    .failed
                    .push((id.clone(), format!("unload failed: {error}")));
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
            self.supervisor
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
        let _ = self.supervisor.shutdown().await;
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
    async fn report_for(&self, plugin_id: &str) -> Result<Value, HostCallError> {
        let payload = tokio::time::timeout(
            REPORT_TIMEOUT,
            self.supervisor.call_service(
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

    #[allow(clippy::too_many_arguments)]
    fn register(
        &self,
        plugin_id: &str,
        ready: &PluginReadyReport,
        extras: &Value,
        publish: bool,
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
        let outcome = self.register_published(plugin_id, ready, extras, &mut record);
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
        plugin_id: &str,
        ready: &PluginReadyReport,
        extras: &Value,
        record: &mut Registrations,
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
        if !proxies.is_empty() {
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

        self.register_commands(plugin_id, ready, &scope, record)?;
        self.register_services(plugin_id, ready, &scope, record);

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
                    supervisor: Arc::clone(&self.supervisor),
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
                PluginCommandKind::Prompt => CommandHandler::Prompt(
                    self.command_proxy(plugin_id.to_owned(), definition.name.clone()),
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
        plugin_id: String,
        name: String,
    ) -> Arc<dyn Fn(&CommandArgs) -> Result<String, String> + Send + Sync> {
        // The four handles the call needs, cloned once rather than a handle on
        // the plane: a proxy that held the plane would keep the host alive for
        // as long as any menu remembered the command.
        let supervisor = Arc::clone(&self.supervisor);
        let plane_scope = self.scope.clone();
        let runtime = self.runtime.clone();
        let bound = self.unary_call_timeout;
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
            match rx.recv_timeout(bound + Self::PROXY_JOIN_EXTRA) {
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

    /// Publishes a plugin's services on the entry's scope.
    ///
    /// On the entry's own fork rather than the kernel root, which is the
    /// conservative half of "the same model as a built-in": the registration
    /// is real and disposes with the entry, and it cannot shadow a kernel
    /// service, because a name already answered on an ancestor layer is
    /// refused here and said out loud rather than silently winning.
    fn register_services(
        &self,
        plugin_id: &str,
        ready: &PluginReadyReport,
        scope: &Context,
        record: &mut Registrations,
    ) {
        for name in &ready.services {
            let proxy = Arc::new(PlaneServiceProxy {
                bound: self.unary_call_timeout,
                supervisor: Arc::clone(&self.supervisor),
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
            self.supervisor
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
            .supervisor
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
}

impl SeatDispatcher for KernelSeats {
    fn call(
        &self,
        invocation: SeatInvocation,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Payload, ToolRefusal>> + Send + '_>,
    > {
        let mut params = invocation.params.to_value().unwrap_or(Value::Null);
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
    supervisor: Arc<PluginHostSupervisor>,
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
        let call: std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>> = match route {
            Route::Tool => Box::pin(self.supervisor.call_tool(&owner, &scope, tool, payload)),
            // A composition tool reached through a service: the caller's own
            // `timeout` below is this call's bound, and a tool's length is the
            // tool's business.
            Route::Service => Box::pin(self.supervisor.call_service(&owner, &scope, tool, payload)),
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
    fn publish_is_part_of_the_comparison() {
        let before = vec![entry("a", Value::Null)];
        let mut promoted = entry("a", Value::Null);
        promoted.publish = false;
        let plan = classify(&running(&before), &[promoted]);

        assert_eq!(plan.changed, vec!["a".to_string()]);
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
