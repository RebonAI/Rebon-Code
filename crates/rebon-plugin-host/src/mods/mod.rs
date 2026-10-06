//! Claude Code mods on the plugin plane: the rebon half.
//!
//! A mod is a plane plugin the `mods-runtime` loader built from a Claude Code
//! plugin folder (`compose`). Once the plane has loaded one, the registry
//! here holds its record — what the scan said it hooks and names, its
//! `$.state` values, its environment overlay — and answers for it in three
//! directions:
//!
//! * **down**, as the `mods` seat (`seat`): every `$` call that leaves Node;
//! * **up**, as the policy subscriber (`policy`): every hook event rebon
//!   raises reaches the mods that listen to it, and their answers come back
//!   as the effects a settings hook would produce;
//! * **sideways**, to the surfaces (`ui`): what the mods have put on screen,
//!   and the calls a surface makes to draw a pane or deliver a press.
//!
//! One registry per plane, reachable process-wide through [`process_mods`]
//! for as long as the plane runs — the same slot discipline the plane
//! itself keeps, cleared by an effect on the plane's fork.

pub mod compose;
pub(crate) mod contained;
pub mod policy;
pub mod remote;
pub mod seat;
pub mod ui;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rebon_command_seat::{CommandHandler, CommandKind, CommandSeatService, CommandSpec};
use rebon_core::tool_seat::{Priority, ToolSeatService};
use rebon_kernel::Context;
use rebon_kernel_seats::kernel_compose_tools::ComposeToolRegistry;
use rebon_plugin_protocol::Payload;
use rebon_plugin_supervisor::{HostCallError, ToolRefusal};
use rebon_tool::ToolResolver;
use rebon_types::{
    validate_ui_tree, ModCommandDecl, ModToolDecl, ModUiNode, ModUiSurface, MOD_MARKER, MOD_SERVICE,
};
use serde_json::{json, Value};

use crate::plugin_plane::ComposeEntry;
pub use remote::{
    LinkedModCommands, ModCommandAnswer, ModCommandRow, ModsLink, ModsView, RemoteMods,
};
pub use ui::{
    ModFill, ModFocusRequest, ModLogLine, ModPane, ModPrompt, ModToast, ModUiSnapshot, ModUiState,
};

/// The session facts a mod's `$.session` answers from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModFacts {
    pub cwd: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub surface: Option<String>,
}

/// One loaded mod, as the registry knows it.
pub struct ModRecord {
    /// The composition entry id; the plane's name for it.
    pub id: String,
    /// The manifest's name; what the mod's hooks are reported under.
    pub name: String,
    pub version: Option<String>,
    pub root: String,
    /// The patterns its `on(...)` calls name.
    pub events: Vec<String>,
    /// What it calls on `$`.
    pub calls: Vec<String>,
    /// The commands it may register, as scanned, with the latest spec a
    /// `$.command.register` refined.
    commands: Mutex<Vec<ModCommandDecl>>,
    /// The tools it may register, as scanned, with the latest description
    /// and schema a `$.tool.register` refined.
    tools: Mutex<Vec<(ModToolDecl, Value)>>,
    state: Mutex<BTreeMap<String, (Value, u64)>>,
    env: Mutex<BTreeMap<String, Option<String>>>,
    facts: Mutex<ModFacts>,
    /// The fork the mod's commands and tools are registered on; replaced
    /// whole when a registration changes.
    registrations: Mutex<Option<Context>>,
    /// Set while a person's own action (a command, a press) runs through
    /// the mod, so a pane it opens then counts as asked for.
    in_person_action: std::sync::atomic::AtomicUsize,
    /// The container it runs in, when it is not trusted: what its seat calls
    /// to the machine are held to (see [`contained`]).
    pub container: Option<crate::container::ContainerSpec>,
}

impl ModRecord {
    fn from_entry(entry: &ComposeEntry) -> Option<Self> {
        let marker = entry.config.get(MOD_MARKER)?.as_object()?;
        let name = marker
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&entry.id)
            .to_owned();
        let list = |key: &str| -> Vec<String> {
            marker
                .get(key)
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let commands: Vec<ModCommandDecl> = marker
            .get("commands")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        let tools: Vec<ModToolDecl> = marker
            .get("tools")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        Some(Self {
            id: entry.id.clone(),
            name,
            version: marker
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned),
            root: entry.root.clone(),
            events: list("events"),
            calls: list("calls"),
            commands: Mutex::new(commands),
            tools: Mutex::new(
                tools
                    .into_iter()
                    .map(|tool| (tool, json!({ "type": "object" })))
                    .collect(),
            ),
            state: Mutex::new(BTreeMap::new()),
            env: Mutex::new(BTreeMap::new()),
            facts: Mutex::new(ModFacts {
                cwd: entry.root.clone(),
                ..ModFacts::default()
            }),
            registrations: Mutex::new(None),
            in_person_action: std::sync::atomic::AtomicUsize::new(0),
            container: entry.container.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(name: &str, events: &[&str]) -> Self {
        Self {
            id: name.to_owned(),
            name: name.to_owned(),
            version: None,
            root: String::new(),
            events: events.iter().map(|e| (*e).to_owned()).collect(),
            calls: Vec::new(),
            commands: Mutex::new(Vec::new()),
            tools: Mutex::new(Vec::new()),
            state: Mutex::new(BTreeMap::new()),
            env: Mutex::new(BTreeMap::new()),
            facts: Mutex::new(ModFacts::default()),
            registrations: Mutex::new(None),
            in_person_action: std::sync::atomic::AtomicUsize::new(0),
            container: None,
        }
    }

    pub fn hooks_event(&self, event: &str) -> bool {
        self.events
            .iter()
            .any(|pattern| rebon_types::pattern_selects(pattern, event))
    }

    pub fn commands(&self) -> Vec<ModCommandDecl> {
        self.commands.lock().expect("mod commands poisoned").clone()
    }

    pub fn tools(&self) -> Vec<ModToolDecl> {
        self.tools
            .lock()
            .expect("mod tools poisoned")
            .iter()
            .map(|(tool, _)| tool.clone())
            .collect()
    }

    pub fn facts(&self) -> ModFacts {
        self.facts.lock().expect("mod facts poisoned").clone()
    }

    pub fn set_facts(&self, update: impl FnOnce(&mut ModFacts)) {
        update(&mut self.facts.lock().expect("mod facts poisoned"));
    }

    fn state_get(&self, key: &str) -> (Value, u64) {
        self.state
            .lock()
            .expect("mod state poisoned")
            .get(key)
            .cloned()
            .unwrap_or((Value::Null, 0))
    }

    /// Writes a value, refusing when `if_version` names another version
    /// than the one held; either way answers what is held after.
    fn state_set(
        &self,
        key: &str,
        value: Value,
        if_version: Option<u64>,
    ) -> Result<(Value, u64), (Value, u64)> {
        let mut state = self.state.lock().expect("mod state poisoned");
        let held = state.get(key).cloned().unwrap_or((Value::Null, 0));
        if if_version.is_some_and(|wanted| wanted != held.1) {
            return Err(held);
        }
        let next = (value, held.1 + 1);
        state.insert(key.to_owned(), next.clone());
        Ok(next)
    }

    fn env_get(&self, name: &str) -> Option<String> {
        self.env
            .lock()
            .expect("mod env poisoned")
            .get(name)
            .cloned()
            .flatten()
    }

    fn env_set(&self, name: String, value: Option<String>) {
        self.env
            .lock()
            .expect("mod env poisoned")
            .insert(name, value);
    }

    fn env_overlay(&self) -> Vec<(String, Option<String>)> {
        self.env
            .lock()
            .expect("mod env poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn in_person_action(&self) -> bool {
        self.in_person_action
            .load(std::sync::atomic::Ordering::Acquire)
            > 0
    }

    /// Where the mod keeps `$.store` across sessions.
    pub fn store_path(&self, config_dir: &std::path::Path) -> PathBuf {
        config_dir
            .join(rebon_types::MODS_DIR)
            .join(".store")
            .join(format!("{}.json", self.name))
    }
}

/// What a surface asked a mod to draw.
#[derive(Clone, Debug, PartialEq)]
pub enum ModRenderAnswer {
    /// The validated tree.
    Tree(ModUiNode),
    /// The hook passed the ask on: the surface draws its own.
    Engine,
}

pub struct ModsRegistry {
    /// Which host answers each mod: the shared one, or its container.
    hosts: crate::plugin_plane::PlaneHosts,
    scope: String,
    /// The process kernel's root context: where the command and tool
    /// seats are.
    kernel: Context,
    /// The plane's fork: registrations fork off it and go with it.
    plane: Context,
    tools: Arc<ComposeToolRegistry>,
    runtime: tokio::runtime::Handle,
    bound: Duration,
    config_dir: PathBuf,
    pub ui: Arc<ModUiState>,
    mods: Mutex<Vec<Arc<ModRecord>>>,
    attached_surfaces: Mutex<Vec<String>>,
    /// This registry, for the command expanders it hands the seat: weak, so
    /// a registration does not keep the registry alive.
    me: std::sync::Weak<Self>,
    /// What mod commands answered since the last prompt, for the model to
    /// read with the next one ([`Self::take_command_context`]).
    command_context: Mutex<Vec<String>>,
    /// The plane's tool invoker: how a contained mod's writes and processes
    /// reach rebon's tools, where the person's permission is asked.
    invoker: std::sync::OnceLock<Arc<dyn rebon_plugin_supervisor::ToolInvoker>>,
}

/// One command's output as the model reads it: the command, what it
/// answered (the row the person saw), then the notes meant for the model
/// alone. Each part cut to [`MAX_COMMAND_CONTEXT_CHARS`].
fn command_context_entry(name: &str, text: &str, context: Option<&str>) -> String {
    let cut = |body: &str| -> String { body.chars().take(MAX_COMMAND_CONTEXT_CHARS).collect() };
    let mut entry = format!("<command-name>/{name}</command-name>");
    if !text.trim().is_empty() {
        entry.push_str(&format!(
            "\n<local-command-stdout>{}</local-command-stdout>",
            cut(text.trim())
        ));
    }
    if let Some(context) = context.map(str::trim).filter(|context| !context.is_empty()) {
        entry.push_str(&format!("\n{}", cut(context)));
    }
    entry
}

/// Appends `entry`, dropping the oldest past `max`.
fn push_bounded(list: &mut Vec<String>, entry: String, max: usize) {
    list.push(entry);
    let excess = list.len().saturating_sub(max);
    list.drain(..excess);
}

/// The most command outputs kept for the next prompt; older ones go first.
const MAX_COMMAND_CONTEXT: usize = 16;
/// The most characters of one command's output the model is handed.
const MAX_COMMAND_CONTEXT_CHARS: usize = 8_000;

static PROCESS_MODS: Mutex<Option<Arc<ModsRegistry>>> = Mutex::new(None);

/// The registry of the plane this process runs, if a plane runs.
/// Whether this process's plane loads Claude Code mods at all.
///
/// Yes by default: a terminal session or a worker runs the turns its mods
/// hook. A process that only ever *shows* sessions running elsewhere — the
/// desktop app — says no once at startup, and reaches each session's mods
/// through [`ModsLink::Remote`] instead.
pub fn process_loads_mods() -> bool {
    !MODS_OFF_HERE.load(std::sync::atomic::Ordering::Acquire)
}

/// See [`process_loads_mods`]. Set before the plane boots.
pub fn set_process_loads_mods(loads: bool) {
    MODS_OFF_HERE.store(!loads, std::sync::atomic::Ordering::Release);
}

static MODS_OFF_HERE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn process_mods() -> Option<Arc<ModsRegistry>> {
    PROCESS_MODS.lock().expect("mods slot poisoned").clone()
}

fn set_process_mods(registry: Option<Arc<ModsRegistry>>) {
    *PROCESS_MODS.lock().expect("mods slot poisoned") = registry;
}

impl ModsRegistry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hosts: crate::plugin_plane::PlaneHosts,
        scope: String,
        kernel: Context,
        plane: Context,
        tools: Arc<ComposeToolRegistry>,
        runtime: tokio::runtime::Handle,
        bound: Duration,
        config_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            hosts,
            scope,
            kernel,
            plane,
            tools,
            runtime,
            bound,
            config_dir,
            ui: Arc::new(ModUiState::new()),
            mods: Mutex::new(Vec::new()),
            attached_surfaces: Mutex::new(Vec::new()),
            me: me.clone(),
            command_context: Mutex::new(Vec::new()),
            invoker: std::sync::OnceLock::new(),
        })
    }

    /// Keeps what a mod command answered for the model's next prompt: in
    /// Claude Code the command's output row is one the model reads, and
    /// `context` notes are for the model alone, recorded after it.
    pub fn record_command_output(&self, name: &str, text: &str, context: Option<&str>) {
        let mut pending = self
            .command_context
            .lock()
            .expect("mods command context poisoned");
        push_bounded(
            &mut pending,
            command_context_entry(name, text, context),
            MAX_COMMAND_CONTEXT,
        );
    }

    /// Whether a command output waits for the next prompt.
    pub fn has_command_context(&self) -> bool {
        !self
            .command_context
            .lock()
            .expect("mods command context poisoned")
            .is_empty()
    }

    /// The command outputs waiting for the next prompt, taken.
    pub fn take_command_context(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .command_context
                .lock()
                .expect("mods command context poisoned"),
        )
    }

    /// Puts the registry in the process slot and on the policy seat, both
    /// undone when `plane` is disposed.
    pub fn install(self: &Arc<Self>, kernel: &Arc<rebon_kernel::Kernel>) {
        set_process_mods(Some(Arc::clone(self)));
        self.plane.effect_labeled("mods registry slot", || {
            rebon_kernel::Disposer::new(|| set_process_mods(None))
        });
        match rebon_kernel_seats::kernel_core_tools::process_policy_event_seat(kernel) {
            Some(seat) => {
                let subscriber = policy::ModsPolicySubscriber::new(self);
                if let Err(error) = seat.subscribe_scoped(
                    &self.plane,
                    policy::MODS_SUBSCRIBER_ID,
                    rebon_core::turn_hook::Order::NORMAL,
                    subscriber,
                ) {
                    tracing::warn!(%error, "mods: the policy seat refused the subscriber; mod hooks will not hear events");
                }
            }
            None => tracing::debug!(
                "mods: no policy seat on this kernel; mod hooks will not hear events"
            ),
        }
    }

    pub fn mods(&self) -> Vec<Arc<ModRecord>> {
        self.mods.lock().expect("mods table poisoned").clone()
    }

    pub fn get(&self, id: &str) -> Option<Arc<ModRecord>> {
        self.mods().into_iter().find(|record| record.id == id)
    }

    pub fn by_name(&self, name: &str) -> Option<Arc<ModRecord>> {
        self.mods().into_iter().find(|record| record.name == name)
    }

    pub fn is_mod(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    /// Whether this entry would be a mod, before it is attached.
    pub fn entry_is_mod(entry: &ComposeEntry) -> bool {
        rebon_plugin_package::is_mod_config(&entry.config)
    }

    /// Records a loaded mod and puts its commands and tools on the seats.
    pub fn attach(&self, entry: &ComposeEntry) -> Result<(), String> {
        let Some(record) = ModRecord::from_entry(entry) else {
            return Err(format!("{} is not a mod entry", entry.id));
        };
        let record = Arc::new(record);
        self.register_contributions(&record)?;
        let mut mods = self.mods.lock().expect("mods table poisoned");
        mods.retain(|held| held.id != record.id);
        mods.push(record);
        Ok(())
    }

    /// Forgets a mod and takes everything it registered and drew.
    pub fn detach(&self, id: &str) {
        let removed = {
            let mut mods = self.mods.lock().expect("mods table poisoned");
            let index = mods.iter().position(|record| record.id == id);
            index.map(|index| mods.remove(index))
        };
        if let Some(record) = removed {
            if let Some(scope) = record
                .registrations
                .lock()
                .expect("mod registrations poisoned")
                .take()
            {
                scope.dispose();
            }
            self.ui.forget_plugin(id);
        }
    }

    /// (Re)registers a mod's commands and tools on a fresh fork.
    fn register_contributions(&self, record: &Arc<ModRecord>) -> Result<(), String> {
        let previous = record
            .registrations
            .lock()
            .expect("mod registrations poisoned")
            .take();
        if let Some(scope) = previous {
            scope.dispose();
        }
        let scope = self.plane.fork(&format!("node/{}/mods", record.id));
        if let Some(seat) = self.kernel.get::<CommandSeatService>() {
            for command in record.commands() {
                let mut spec = CommandSpec::new(
                    command.name.clone(),
                    command.description.clone().unwrap_or_else(|| {
                        format!("/{}, a command of the mod {}", command.name, record.name)
                    }),
                )
                // A mod's command answers in the transcript: what its
                // `command.run` hook returns is the command's output row,
                // not a prompt, and running it starts no turn.
                .kind(CommandKind::Session)
                .surfaces(rebon_command_seat::Surfaces::ALL);
                if let Some(hint) = &command.argument_hint {
                    spec = spec.hint(hint.clone());
                }
                // Through `run_command`, as a surface in another process
                // runs it: the press counts as the person's, and what it
                // answers is kept for the model's next prompt.
                let proxy = remote::local_mod_command(
                    self.me.clone(),
                    self.runtime.clone(),
                    command.name.clone(),
                );
                seat.register(&scope, spec, CommandHandler::Prompt(proxy))
                    .map_err(|error| {
                        format!(
                            "[COMMAND_NAME_TAKEN] {} registers command /{} which is already taken: {error}",
                            record.name, command.name
                        )
                    })?;
            }
        }
        let tools: Vec<_> = record.tools.lock().expect("mod tools poisoned").clone();
        if !tools.is_empty() {
            let mut proxies = Vec::new();
            for (tool, schema) in tools {
                if let Some(proxy) = self.tools.declare(
                    &tool.name,
                    tool.description.clone().unwrap_or_else(|| {
                        format!("Tool {} of the mod {}", tool.name, record.name)
                    }),
                    Some(schema),
                    false,
                ) {
                    proxies.push(proxy);
                }
            }
            if let Some(seat) = self.kernel.get::<ToolSeatService>() {
                seat.register_tools(
                    &scope,
                    &format!("node/{}/mods", record.id),
                    Priority::Plugin,
                    proxies,
                )
                .map_err(|error| format!("registering the tools of {}: {error}", record.name))?;
            }
        }
        *record
            .registrations
            .lock()
            .expect("mod registrations poisoned") = Some(scope);
        Ok(())
    }

    // ---- what the seat needs ------------------------------------------

    fn register_command(
        &self,
        record: &Arc<ModRecord>,
        params: &Value,
    ) -> Result<Value, ToolRefusal> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolRefusal::new("[WRONG_SHAPE]", "command.register needs a name"))?;
        let changed = {
            let mut commands = record.commands.lock().expect("mod commands poisoned");
            let Some(command) = commands.iter_mut().find(|c| c.name == name) else {
                return Err(ToolRefusal::new(
                    "[UNDECLARED_COMMAND]",
                    format!("command {name} is not spelled as a literal in a $.command.register call of {}", record.name),
                ));
            };
            let description = params
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let hint = params
                .get("argumentHint")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let changed = (description.is_some() && description != command.description)
                || (hint.is_some() && hint != command.argument_hint);
            if description.is_some() {
                command.description = description;
            }
            if hint.is_some() {
                command.argument_hint = hint;
            }
            changed
        };
        if changed {
            self.register_contributions(record)
                .map_err(|error| ToolRefusal::new("[REGISTER_FAILED]", error))?;
        }
        Ok(json!({ "name": name, "isRegistered": true }))
    }

    fn register_tool(&self, record: &Arc<ModRecord>, params: &Value) -> Result<Value, ToolRefusal> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolRefusal::new("[WRONG_SHAPE]", "tool.register needs a name"))?;
        {
            let mut tools = record.tools.lock().expect("mod tools poisoned");
            let Some((tool, schema)) = tools.iter_mut().find(|(t, _)| t.name == name) else {
                return Err(ToolRefusal::new(
                    "[UNDECLARED_TOOL]",
                    format!(
                        "tool {name} is not spelled as a literal in a $.tool.register call of {}",
                        record.name
                    ),
                ));
            };
            if let Some(description) = params.get("description").and_then(Value::as_str) {
                tool.description = Some(description.to_owned());
            }
            if let Some(given) = params.get("inputSchema").filter(|v| v.is_object()) {
                *schema = given.clone();
            }
        }
        self.register_contributions(record)
            .map_err(|error| ToolRefusal::new("[REGISTER_FAILED]", error))?;
        Ok(json!({ "name": name, "isRegistered": true }))
    }

    fn settings_read(&self, record: &ModRecord) -> Result<Value, ToolRefusal> {
        self.kernel
            .call_json(
                rebon_kernel::SETTINGS_SERVICE,
                "read",
                json!({ rebon_kernel_seats::kernel_config_seats::CALLER_PLUGIN_ID: record.id }),
            )
            .map_err(|error| ToolRefusal::new("[SEAT_FAILED]", error.to_string()))
    }

    fn tool_list(&self) -> Value {
        let names: Vec<Value> = self
            .kernel
            .get::<ToolSeatService>()
            .and_then(|seat| seat.tools(None).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|tool| json!({ "name": tool.id().as_str(), "description": tool.description() }))
            .collect();
        Value::Array(names)
    }

    fn command_list(&self) -> Value {
        let rows: Vec<Value> = self
            .kernel
            .get::<CommandSeatService>()
            .map(|seat| seat.all())
            .unwrap_or_default()
            .into_iter()
            .map(|command| {
                json!({
                    "name": command.spec.name,
                    "description": command.spec.description,
                    "argumentHint": command.spec.hint,
                    "source": if command.owner.starts_with("node/") { "plugin" } else { "builtin" },
                })
            })
            .collect();
        Value::Array(rows)
    }

    fn store_read(&self, record: &ModRecord) -> serde_json::Map<String, Value> {
        std::fs::read(record.store_path(&self.config_dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
    }

    fn store_update(
        &self,
        record: &ModRecord,
        change: impl FnOnce(&mut serde_json::Map<String, Value>),
    ) -> Result<(), String> {
        let path = record.store_path(&self.config_dir);
        let mut store = self.store_read(record);
        change(&mut store);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        let bytes =
            serde_json::to_vec_pretty(&Value::Object(store)).map_err(|error| error.to_string())?;
        rebon_session::write_file_atomically(&path, &bytes)
            .map_err(|error| format!("{}: {error}", path.display()))
    }

    /// A surface says it is attached, so `$.session.surfaces()` lists it.
    pub fn attach_surface(&self, surface: &str) {
        let mut surfaces = self
            .attached_surfaces
            .lock()
            .expect("mods surfaces poisoned");
        if !surfaces.iter().any(|s| s == surface) {
            surfaces.push(surface.to_owned());
        }
    }

    pub fn attached_surfaces(&self) -> Vec<String> {
        self.attached_surfaces
            .lock()
            .expect("mods surfaces poisoned")
            .clone()
    }

    /// The seat's entry point, as the plane's dispatcher calls it.
    pub fn seat_call(
        self: &Arc<Self>,
        plugin_id: &str,
        method: &str,
        params: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Payload, ToolRefusal>> + Send>>
    {
        let registry = Arc::clone(self);
        let record = self.get(plugin_id);
        let method = method.to_owned();
        let plugin_id = plugin_id.to_owned();
        Box::pin(async move {
            let Some(record) = record else {
                return Err(ToolRefusal::new(
                    "[UNKNOWN_PLUGIN]",
                    format!("{plugin_id} is not a loaded mod"),
                ));
            };
            seat::call(registry, record, &method, params)
                .await
                .map(Payload::from)
        })
    }

    // ---- what the surfaces ask ------------------------------------------

    /// Hands the registry the plane's tool invoker. Once; a second call is
    /// ignored.
    pub fn set_tool_invoker(&self, invoker: Arc<dyn rebon_plugin_supervisor::ToolInvoker>) {
        let _ = self.invoker.set(invoker);
    }

    pub(crate) fn tool_invoker(&self) -> Option<Arc<dyn rebon_plugin_supervisor::ToolInvoker>> {
        self.invoker.get().cloned()
    }

    /// The scope the plane's own calls travel on.
    pub(crate) fn scope(&self) -> &str {
        &self.scope
    }

    /// One call of the mod's service, bounded.
    pub async fn call_mod_bounded(
        &self,
        id: &str,
        request: Value,
        bound: Option<Duration>,
    ) -> Result<Value, HostCallError> {
        let payload = self
            .hosts
            .for_plugin(id)
            .call_service_bounded(id, &self.scope, MOD_SERVICE, Payload::from(request), bound)
            .await?;
        payload
            .to_value()
            .map_err(|error| HostCallError::Malformed(format!("mod answer is not JSON: {error}")))
    }

    pub async fn call_mod(&self, id: &str, request: Value) -> Result<Value, String> {
        self.call_mod_bounded(id, request, Some(self.bound))
            .await
            .map_err(|error| error.to_string())
    }

    /// Tells a mod the session it is drawn for.
    pub async fn tell_facts(&self, id: &str, facts: &ModFacts) {
        if let Some(record) = self.get(id) {
            record.set_facts(|held| {
                if !facts.cwd.is_empty() {
                    held.cwd = facts.cwd.clone();
                }
                if facts.session_id.is_some() {
                    held.session_id = facts.session_id.clone();
                }
                if facts.model.is_some() {
                    held.model = facts.model.clone();
                }
                if facts.surface.is_some() {
                    held.surface = facts.surface.clone();
                }
            });
        }
        let _ = self
            .call_mod(
                id,
                json!({ "kind": "facts", "facts": {
                    "cwd": facts.cwd, "sessionId": facts.session_id, "model": facts.model, "surface": facts.surface,
                } }),
            )
            .await;
    }

    /// Asks a mod to draw one component instance for a surface.
    pub async fn render(
        &self,
        id: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        props: Value,
        viewport: Option<(u32, u32)>,
    ) -> Result<ModRenderAnswer, String> {
        let answer = self
            .call_mod(
                id,
                json!({
                    "kind": "render",
                    "component": component,
                    "surface": surface.as_str(),
                    "requestId": request_id,
                    "props": props,
                    "viewport": viewport.map(|(columns, rows)| json!({ "columns": columns, "rows": rows })),
                }),
            )
            .await?;
        if answer.get("engine").and_then(Value::as_bool) == Some(true) {
            return Ok(ModRenderAnswer::Engine);
        }
        if let Some(error) = answer.get("error").and_then(Value::as_str) {
            return Err(error.to_owned());
        }
        let Some(tree) = answer.get("tree") else {
            return Err("the mod answered no tree".to_owned());
        };
        let name = self
            .get(id)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| id.to_owned());
        validate_ui_tree(tree, surface)
            .map(ModRenderAnswer::Tree)
            .map_err(|refusal| {
                let line = format!(
                    "{name}: ui.render ({component}) refused: {refusal}; the engine drew its own"
                );
                tracing::warn!("{line}");
                self.ui.push_log(id, line.clone(), "debug");
                line
            })
    }

    /// Delivers a press on a Button (or a link) the mod drew.
    pub async fn press(
        &self,
        id: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        element: &str,
        href: Option<&str>,
    ) -> Result<Value, String> {
        self.person_action(
            id,
            json!({
                "kind": "press", "component": component, "surface": surface.as_str(),
                "requestId": request_id, "element": element, "href": href,
            }),
        )
        .await
    }

    /// Delivers a change (`change`) or Enter (`submit`) in an Input.
    #[allow(clippy::too_many_arguments)]
    pub async fn input(
        &self,
        id: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        element: &str,
        kind: &str,
        value: &str,
    ) -> Result<Value, String> {
        self.person_action(
            id,
            json!({
                "kind": "input", "component": component, "surface": surface.as_str(),
                "requestId": request_id, "element": element, "inputKind": kind, "value": value,
            }),
        )
        .await
    }

    /// Delivers a pick in a Select.
    pub async fn select(
        &self,
        id: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        element: &str,
        value: Value,
    ) -> Result<Value, String> {
        self.person_action(
            id,
            json!({
                "kind": "select", "component": component, "surface": surface.as_str(),
                "requestId": request_id, "element": element, "value": value,
            }),
        )
        .await
    }

    /// A key the person pressed while a mod's `Client` held the focus ring.
    #[allow(clippy::too_many_arguments)]
    pub async fn client_key(
        &self,
        id: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        element: &str,
        key: Value,
    ) -> Result<Value, String> {
        self.call_mod(
            id,
            json!({
                "kind": "clientKey", "component": component, "surface": surface.as_str(),
                "requestId": request_id, "element": element, "key": key,
            }),
        )
        .await
    }

    /// A pointer event over a mod's `Client`, region-relative.
    #[allow(clippy::too_many_arguments)]
    pub async fn client_pointer(
        &self,
        id: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        element: &str,
        pointer: Value,
    ) -> Result<Value, String> {
        self.call_mod(
            id,
            json!({
                "kind": "clientPointer", "component": component, "surface": surface.as_str(),
                "requestId": request_id, "element": element, "pointer": pointer,
            }),
        )
        .await
    }

    /// A site's focus ring about to move, raised as the mod's `ui.focus`:
    /// `{ deny }`, or `{ element }` where it lands.
    pub async fn focus(
        &self,
        id: &str,
        component: &str,
        request_id: &str,
        element: Option<&str>,
        origin: Value,
    ) -> Result<Value, String> {
        self.call_mod(
            id,
            json!({
                "kind": "focus", "component": component, "requestId": request_id,
                "element": element, "origin": origin,
            }),
        )
        .await
    }

    async fn person_action(&self, id: &str, request: Value) -> Result<Value, String> {
        let record = self.get(id);
        if let Some(record) = &record {
            record
                .in_person_action
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        let outcome = self.call_mod(id, request).await;
        if let Some(record) = &record {
            record
                .in_person_action
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            // A handler ran: whatever it changed, the pane shows it.
            self.ui.invalidate(&record.id, None);
        }
        outcome
    }

    /// What a mod says about itself: its patterns, commands and tools.
    pub async fn describe(&self, id: &str) -> Result<Value, String> {
        self.call_mod(id, json!({ "kind": "describe" })).await
    }

    /// The rows a listing shows, without asking Node.
    pub fn rows(&self) -> Vec<Value> {
        self.mods()
            .iter()
            .map(|record| {
                json!({
                    "id": record.id,
                    "name": record.name,
                    "version": record.version,
                    "root": record.root,
                    "events": record.events,
                    "calls": record.calls,
                    "commands": record.commands().iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
                    "tools": record.tools().iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                    "panes": self.ui.panes_of(&record.id).len(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_output_reads_as_the_command_its_answer_and_its_notes() {
        assert_eq!(
            command_context_entry("radar", " 0 running \n", Some("for the model")),
            "<command-name>/radar</command-name>\n<local-command-stdout>0 running</local-command-stdout>\nfor the model"
        );
        assert_eq!(
            command_context_entry("quiet", "  ", None),
            "<command-name>/quiet</command-name>",
            "a command that said nothing is still one the person ran"
        );
        let long = "x".repeat(MAX_COMMAND_CONTEXT_CHARS + 50);
        let entry = command_context_entry("big", &long, Some(""));
        assert_eq!(entry.matches('x').count(), MAX_COMMAND_CONTEXT_CHARS);
    }

    #[test]
    fn the_waiting_outputs_keep_the_newest_past_the_bound() {
        let mut list = Vec::new();
        for n in 0..5 {
            push_bounded(&mut list, n.to_string(), 3);
        }
        assert_eq!(list, vec!["2", "3", "4"]);
    }

    #[test]
    fn state_values_carry_versions_and_refuse_a_stale_write() {
        let record = ModRecord::for_test("a", &[]);
        assert_eq!(record.state_get("k"), (Value::Null, 0));
        assert_eq!(
            record.state_set("k", json!(1), None).unwrap(),
            (json!(1), 1)
        );
        assert_eq!(
            record.state_set("k", json!(2), Some(1)).unwrap(),
            (json!(2), 2)
        );
        assert_eq!(
            record.state_set("k", json!(3), Some(1)).unwrap_err(),
            (json!(2), 2)
        );
        assert_eq!(record.state_get("k"), (json!(2), 2));
    }

    #[test]
    fn the_environment_overlay_is_the_mods_own() {
        let record = ModRecord::for_test("a", &[]);
        assert_eq!(record.env_get("X"), None);
        record.env_set("X".into(), Some("1".into()));
        assert_eq!(record.env_get("X"), Some("1".into()));
        record.env_set("Y".into(), None);
        assert_eq!(
            record.env_overlay(),
            vec![("X".into(), Some("1".into())), ("Y".into(), None)]
        );
    }

    #[test]
    fn a_record_reads_the_marker_off_an_entry() {
        let entry = ComposeEntry {
            id: "tally".into(),
            root: "/mods/tally".into(),
            entry: "hooks/register.ts".into(),
            config: json!({ MOD_MARKER: {
                "name": "tally", "version": "1.0.0",
                "events": ["tool.call", "classic.*"], "calls": ["ui.status"],
                "commands": [{ "name": "hi", "description": "Says hi" }],
                "tools": [{ "name": "mcp__tally__count" }],
            } }),
            ..ComposeEntry::default()
        };
        assert!(ModsRegistry::entry_is_mod(&entry));
        let record = ModRecord::from_entry(&entry).unwrap();
        assert_eq!(record.name, "tally");
        assert_eq!(record.version.as_deref(), Some("1.0.0"));
        assert!(record.hooks_event("classic.Stop"));
        assert_eq!(record.commands()[0].description.as_deref(), Some("Says hi"));
        assert_eq!(record.tools()[0].name, "mcp__tally__count");
        assert_eq!(record.facts().cwd, "/mods/tally");
        assert!(ModRecord::from_entry(&ComposeEntry::default()).is_none());
    }
}
