//! `agents`: the feature plugin that puts the `Agent` tool on the process
//! tool seat.
//!
//! One tool, `Agent` (aliases `AgentTool`, `Task`): the model hands it a
//! prompt and an agent type, and a sub-agent runs that work on its own —
//! one-shot or as a named session teammate, in the foreground, in the
//! background, or delegated to an external ACP agent. [`runtime`] is what
//! actually runs it: the spawner the tool calls, the worker loop behind that,
//! and the team manager that keeps a named teammate alive between turns.
//!
//! **What moved and what did not.** The tool and its runtime are both here;
//! the sub-agent domain they speak stayed behind, because they are far from
//! its only readers:
//!
//! | stayed in `rebon-tool` | why |
//! |---|---|
//! | [`SubAgentSpawner`](rebon_tool::SubAgentSpawner) | [`runtime::spawner::EngineSubAgentSpawner`] implements it and `ToolContext` hands it out; the TUI and the ultraplan review path each have their own |
//! | `SubAgentSpec` / `SubAgentResult` and the option enums | the engine's parent-context builder and the TUI build and read them without this tool |
//! | [`AgentRegistry`](rebon_tool::AgentRegistry) and the built-in agents | `/agents`, the background dispatcher, the TUI's `@`-completion and `rebon serve`'s API all resolve agent definitions directly |
//! | `SubAgentProgressSender` | the spawner reports child activity through it |
//! | `agent_input_may_write` | `rebon-core`'s run loop reads it to spot a parallel write batch |
//! | [`SubAgentContext`](rebon_tool::SubAgentContext) | `ToolContext` carries the spawner, the write-batch flag and the frozen parent capsule |
//! | [`ExternalSubAgentRunner`](rebon_tool::ExternalSubAgentRunner) | the front end implements it and hands it over the seat below |
//!
//! Turning the plugin off takes `Agent` off the seat *and* leaves the
//! `sub-agent-spawner` and `team-manager` seats empty: the model can no
//! longer delegate, and nothing is standing by to run a delegation if it
//! did. `/agents` still edits definitions.
//!
//! **Where the registry comes from.** `Tool::description(&self) -> &str` has
//! nowhere to take a `ToolContext` from: the engine projects the tool list
//! without one, and the `Agent` description *is* the merged agent list
//! rendered into prose. So the registry reaches the tool the same way the
//! `sub_agents_enabled` switch does — a process-wide cell in `rebon-tool`
//! that the host fills at startup ([`rebon_tool::set_agent_registry_selection`]).
//! [`AgentToolProvider`] builds the tool from that cell on demand and
//! rebuilds only when the host publishes a newer one, so a plugin loaded
//! before the host has read `~/.rebon/agents/` still describes the merged
//! list on the first turn.
//!
//! **Session scopes.** The tool reads the spawner, the permission mode and
//! the ultraplan run off `ToolContext` at call time, so it is a process-wide
//! registration and goes on the process seat. Per-session state, when it
//! moves, lands on the host's session scope
//! ([`rebon_core::session_scope`](rebon_core::session_scope)) — this
//! plugin used to fork under each session to provide a marker of its own,
//! which is now one seat the host provides for every session.

use std::path::Path;
use std::sync::{Arc, RwLock};

use rebon_command_seat::{
    CommandHandler, CommandSeatService, CommandSpec, Surfaces, COMMAND_SEAT_SERVICE,
};
use rebon_core::tool_seat::{Priority, SeatToolProvider, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{
    Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta, Service,
};
use rebon_tool::{
    SubAgentSpawnerRequest, SubAgentSpawnerService, SubAgentSpawnerSource, TeamManagerRequest,
    TeamManagerService, TeamManagerSource, Tool,
};
use rebon_tools_core::tool_matches_name;
use rebon_ui_seat::{DialogDef, UiSeatService};

pub mod agent_files;
pub mod agent_tool;
pub mod dialog;
pub mod runtime;
pub mod surface;
pub mod tool_buckets;

pub use agent_tool::AgentTool;
pub use runtime::spawner::EngineSubAgentSpawner;
pub use runtime::team_manager::{InProcessTeamManager, SessionTaskTeamManager};

pub use rebon_tool::agent::AGENT_TOOL_NAME;

/// Stable id: the config key `plugins.agents.enabled`.
pub const PLUGIN_ID: &str = "agents";

const PROVIDER_ID: &str = "agents";

/// Every tool name this plugin puts on the seat, canonical spelling.
pub const TOOL_NAMES: &[&str] = &[AGENT_TOOL_NAME];

/// The one tool, built against the process agent registry as it stands now.
///
/// The seat gets a [`AgentToolProvider`] rather than this list, because the
/// tool has to be rebuilt when the registry selection changes. This is the
/// snapshot form, public so a test that needs the whole builtin catalogue on
/// a bare [`rebon_core::Engine`] can register it without standing up a
/// kernel.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    AgentToolProvider::default().tools()
}

/// The seat provider behind the `Agent` tool.
///
/// Not a fixed `Vec<Arc<dyn Tool>>` like every other feature plugin's,
/// because the tool's description is rendered from the host's merged agent
/// registry and the host publishes that *after* the kernel boots. The
/// provider builds the tool the first time the seat asks for it and keeps it
/// until [`rebon_tool::set_agent_registry_selection`] bumps the generation,
/// at which point the next ask rebuilds it.
#[derive(Default)]
pub struct AgentToolProvider {
    built: RwLock<Option<Arc<AgentTool>>>,
    /// The kernel scope every tool this provider builds resolves its
    /// optional seams through. `None` for [`tools`], which has no kernel.
    kernel_scope: Option<Context>,
}

impl AgentToolProvider {
    /// A provider whose tools resolve their optional seams through `scope`.
    pub fn with_kernel_scope(scope: Context) -> Self {
        Self {
            built: RwLock::new(None),
            kernel_scope: Some(scope),
        }
    }

    fn current(&self) -> Arc<AgentTool> {
        let wanted = rebon_tool::agent_registry_selection().generation;
        if let Some(tool) = self
            .built
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .filter(|tool| tool.selection_generation() == Some(wanted))
        {
            return tool.clone();
        }
        let mut slot = self
            .built
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Another writer may have rebuilt while this one waited.
        if let Some(tool) = slot
            .as_ref()
            .filter(|tool| tool.selection_generation() == Some(wanted))
        {
            return tool.clone();
        }
        let tool = Arc::new(match self.kernel_scope.clone() {
            Some(scope) => AgentTool::from_process_registry().with_kernel_scope(scope),
            None => AgentTool::from_process_registry(),
        });
        *slot = Some(tool.clone());
        tool
    }
}

impl SeatToolProvider for AgentToolProvider {
    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        let tool = self.current();
        if tool.is_enabled() && tool_matches_name(tool.id().as_str(), tool.aliases(), name) {
            Some(tool as Arc<dyn Tool>)
        } else {
            None
        }
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.current() as Arc<dyn Tool>]
    }
}

/// The provider on the `sub-agent-spawner` seat.
///
/// Stateless: everything a session's spawner needs is in the request, because
/// the front end is the side that knows which engine, which client and which
/// models this session has. The two things it cannot name are the engine and
/// the task registry resolver — `rebon-tool` sits below both — so they cross
/// as [`rebon_tool::SubAgentRuntimeHandle`]s and are unwrapped here, where the
/// concrete types are in scope again.
struct RuntimeSpawnerSource;

impl SubAgentSpawnerSource for RuntimeSpawnerSource {
    fn for_session(
        &self,
        request: SubAgentSpawnerRequest,
    ) -> Result<Arc<dyn rebon_tool::SubAgentSpawner>, String> {
        let engine = request
            .engine
            .get::<std::sync::Weak<rebon_core::Engine>>()
            .ok_or_else(|| {
                "the sub-agent spawner needs a `Weak<Engine>` on its engine handle".to_string()
            })?
            .clone();
        let mut spawner = EngineSubAgentSpawner::new(engine, request.client)
            .with_default_model(request.default_model)
            .with_model_config(request.model_config)
            .with_model_profiles(request.model_profiles)
            .with_model_router(request.model_router)
            .with_coordinator_use_worktree(request.coordinator_use_worktree);
        if let Some(filter) = request.base_filter {
            spawner = spawner.with_shared_base_filter(filter);
        }
        if let Some(mode) = request.coordinator_mode {
            spawner = spawner.with_shared_coordinator_mode(mode);
        }
        if let Some(handle) = request.task_runtime {
            let resolver = handle
                .get::<rebon_plugin_tasks::TaskRegistryResolver>()
                .ok_or_else(|| {
                    "the sub-agent spawner needs a `TaskRegistryResolver` on its task-runtime handle"
                        .to_string()
                })?
                .clone();
            spawner = spawner.with_task_registry_resolver(resolver);
        }
        if let Some(tracker) = request.file_history_tracker {
            spawner = spawner.with_file_history_tracker(tracker);
        }
        if let Some(handle) = request.policy {
            let policy = handle
                .get::<rebon_core::policy_seat::PolicySources>()
                .ok_or_else(|| {
                    "the sub-agent spawner needs a `PolicySources` on its policy handle".to_string()
                })?
                .clone();
            spawner = spawner.with_policy(policy);
        }
        if let Some(runner) = request.external_runner {
            spawner = spawner.with_external_runner(runner);
        }
        Ok(Arc::new(spawner) as Arc<dyn rebon_tool::SubAgentSpawner>)
    }
}

/// The provider on the `team-manager` seat.
///
/// Same shape and same reasoning as [`RuntimeSpawnerSource`], with one
/// difference the request spells out: a teammate outlives the turn that
/// spawned it, so its half of the engine crosses as an `Arc`, not a `Weak`.
struct RuntimeTeamManagerSource;

impl TeamManagerSource for RuntimeTeamManagerSource {
    fn for_session(
        &self,
        request: TeamManagerRequest,
    ) -> Result<Arc<dyn rebon_tool::TeamManager>, String> {
        let engine = request
            .engine
            .get::<Arc<rebon_core::Engine>>()
            .ok_or_else(|| {
                "the team manager needs an `Arc<Engine>` on its engine handle".to_string()
            })?
            .clone();
        let resolver = request
            .task_runtime
            .get::<rebon_plugin_tasks::TaskRegistryResolver>()
            .ok_or_else(|| {
                "the team manager needs a `TaskRegistryResolver` on its task-runtime handle"
                    .to_string()
            })?
            .clone();
        let mut manager =
            SessionTaskTeamManager::new(engine, request.client, resolver, request.default_model)
                .with_model_config(request.model_config)
                .with_model_profiles(request.model_profiles)
                .with_model_router(request.model_router);
        if let Some(filter) = request.base_filter {
            manager = manager.with_shared_base_filter(filter);
        }
        Ok(Arc::new(manager) as Arc<dyn rebon_tool::TeamManager>)
    }
}

#[derive(Default)]
pub struct AgentsPlugin;

/// `/agents` as the command seat sees it.
///
/// Not in the built-in table: an agent definition is only worth editing
/// if something will run it, and turning this plugin off is exactly the
/// statement that nothing will. The handler is `Native` — the front end
/// maps the id to opening [`dialog::AgentsDialogState`], which needs the
/// session handles only it holds.
pub fn command_spec() -> CommandSpec {
    CommandSpec::new("agents", "Manage agent definitions")
        .zh_aliases(["智能体"])
        .category(rebon_command_seat::Category::Agent)
        .surfaces(Surfaces::LOCAL.with(Surfaces::WEB))
        .kind(rebon_command_seat::CommandKind::Panel)
}

/// The `/agents` panel, built from the [`dialog::AgentsDialogInput`] the
/// front end hands over as the seat's opaque payload. A caller that
/// passes something else gets no panel rather than an empty one.
fn dialog_def() -> DialogDef {
    DialogDef::new(dialog::DIALOG_ID, |args| {
        let input = args.payload_as::<dialog::AgentsDialogInput>()?;
        Some(Box::new(
            dialog::AgentsDialogState::open_with_external_model_options(
                Path::new(&input.cwd),
                input.tool_names.clone(),
                input.initial_agent_type.as_deref(),
                input.external_model_options.clone(),
            ),
        ))
    })
}

impl Plugin for AgentsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID)
            .inject(&[TOOL_SEAT_SERVICE, COMMAND_SEAT_SERVICE])
            // Per-agent memory is the `memory` plugin's, and it may be
            // off. A sub-agent then spawns without it, which is what an
            // agent that declares no scope already gets.
            .optional_inject(&[
                rebon_instructions::agent_documents::AGENT_MEMORY_SERVICE,
                <UiSeatService as Service>::NAME,
            ])
            .provides(&[
                rebon_tool::SUB_AGENT_SPAWNER_SERVICE,
                rebon_tool::TEAM_MANAGER_SERVICE,
            ])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register(
            ctx,
            PROVIDER_ID,
            Priority::Feature,
            Arc::new(AgentToolProvider::with_kernel_scope(ctx.clone())),
        )?;

        ctx.provide::<SubAgentSpawnerService>(Arc::new(RuntimeSpawnerSource))?;
        ctx.provide::<TeamManagerService>(Arc::new(RuntimeTeamManagerSource))?;

        let commands = ctx.require::<CommandSeatService>()?;
        let spec = command_spec();
        let handler = CommandHandler::Native(spec.name.clone());
        commands.register(ctx, spec, handler)?;

        // The panel is optional: a kernel booted without the UI seat
        // (a headless harness) still gets the tool and the command.
        if let Ok(ui) = ctx.require::<UiSeatService>() {
            ui.register_dialog(ctx, dialog_def())?;
        }

        Ok(())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(AgentsPlugin::default()))
}

/// This crate's one export to the binary's plugin table.
///
/// Default-enabled. Note the second switch: `rebon_tool::sub_agents_enabled`
/// is the user-facing "Sub-agents" toggle, persisted in
/// `~/.rebon/config.json`, and the tool's `is_enabled()` still honours it.
/// Either switch off takes `Agent` off the seat.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Sub-agent delegation (Agent)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_command_seat::{CommandSeat, Surface};
    use rebon_core::tool_seat::ToolSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::{AgentRegistry, ToolResolver};
    use rebon_ui_seat::{DialogArgs, UiSeat};

    /// Stands in for the Core plugins that provide the three seats this one
    /// registers on — `core-tools`, `core-commands` and `core-ui`. Those
    /// plugins depend on this crate and so cannot be depended on from here;
    /// all this plugin needs of them is the seats on the kernel root.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[
                TOOL_SEAT_SERVICE,
                COMMAND_SEAT_SERVICE,
                <UiSeatService as Service>::NAME,
            ])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<ToolSeatService>(ToolSeat::new())?;
            ctx.provide::<CommandSeatService>(CommandSeat::new())?;
            ctx.provide::<UiSeatService>(UiSeat::new())
        }
    }

    fn make_seat(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(SeatPlugin))
    }

    static DEFS: &[PluginDef] = &[
        PluginDef {
            id: "test-seat",
            title: "Test seat",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_seat,
        },
        PLUGIN,
    ];

    fn boot() -> (Arc<Kernel>, Arc<PluginRegistry>) {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        (kernel, registry)
    }

    fn seat(kernel: &Kernel) -> Arc<ToolSeat> {
        kernel
            .context()
            .get::<ToolSeatService>()
            .expect("the seat is on the root")
    }

    #[test]
    fn the_switch_takes_the_tool_off_the_seat_and_puts_it_back() {
        let (kernel, registry) = boot();
        let seat = seat(&kernel);

        assert_eq!(TOOL_NAMES.len(), 1);
        for name in TOOL_NAMES {
            assert!(
                seat.resolve(name, None).unwrap().is_some(),
                "{name} resolves while agents is loaded"
            );
        }

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("agents is a feature plugin");
        for name in TOOL_NAMES {
            assert!(
                seat.resolve(name, None).unwrap().is_none(),
                "disabling the plugin takes {name} off the seat"
            );
        }

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        for name in TOOL_NAMES {
            assert!(
                seat.resolve(name, None).unwrap().is_some(),
                "{name} is back on the seat"
            );
        }
    }

    /// The switch reaches the runtime, not only the tool. With the plugin
    /// off both seats are empty, a session is built with no spawner and no
    /// team manager, and there is nothing standing by to run a delegation —
    /// which is the same answer the model gets from the missing tool. Before
    /// this seat existed the front end went on building a spawner nobody
    /// could reach.
    #[test]
    fn the_switch_takes_the_runtime_off_its_seats_and_puts_it_back() {
        let (kernel, registry) = boot();

        assert!(kernel.context().get::<SubAgentSpawnerService>().is_some());
        assert!(kernel.context().get::<TeamManagerService>().is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("agents is a feature plugin");
        assert!(
            kernel.context().get::<SubAgentSpawnerService>().is_none(),
            "no provider on the seat means the front end attaches no spawner"
        );
        assert!(
            kernel.context().get::<TeamManagerService>().is_none(),
            "and no team manager either"
        );

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(kernel.context().get::<SubAgentSpawnerService>().is_some());
        assert!(kernel.context().get::<TeamManagerService>().is_some());
    }

    /// The policy handle crosses the same way and has to be unwrapped the
    /// same way. Without this, a handle that is read by nobody looks exactly
    /// like a handle that is read correctly: the spawner still builds, and
    /// every sub-agent silently asks no one.
    #[test]
    fn a_policy_handle_of_the_wrong_shape_is_refused() {
        let refused = RuntimeSpawnerSource.for_session(SubAgentSpawnerRequest {
            engine: rebon_tool::SubAgentRuntimeHandle::new(std::sync::Arc::downgrade(
                &std::sync::Arc::new(rebon_core::Engine::with_builtin_tools()),
            )),
            task_runtime: None,
            client: Arc::new(rebon_api::MockModelClient::new()),
            default_model: "test-model".into(),
            model_config: rebon_types::SubAgentModelConfig::default(),
            model_profiles: rebon_types::ModelProfileMap::default(),
            model_router: Arc::new(
                rebon_agent_core::model_router::SingleProviderModelRouter::new(
                    Arc::new(rebon_api::MockModelClient::new()),
                    "test-model",
                ),
            ),
            base_filter: None,
            coordinator_mode: None,
            coordinator_use_worktree: false,
            file_history_tracker: None,
            external_runner: None,
            policy: Some(rebon_tool::SubAgentRuntimeHandle::new(7u32)),
        });
        let Err(error) = refused else {
            panic!("a u32 is not a set of policy subscribers");
        };
        assert!(error.contains("PolicySources"), "{error}");
    }

    /// The engine and the task registry cross the seats untyped, so a handle
    /// of the wrong shape has to come back as an error at the one call site
    /// that builds a session — not as a panic partway through someone's run.
    #[test]
    fn a_runtime_handle_of_the_wrong_shape_is_refused() {
        let refused = RuntimeSpawnerSource.for_session(SubAgentSpawnerRequest {
            engine: rebon_tool::SubAgentRuntimeHandle::new(7u32),
            task_runtime: None,
            client: Arc::new(rebon_api::MockModelClient::new()),
            default_model: "test-model".into(),
            model_config: rebon_types::SubAgentModelConfig::default(),
            model_profiles: rebon_types::ModelProfileMap::default(),
            model_router: Arc::new(
                rebon_agent_core::model_router::SingleProviderModelRouter::new(
                    Arc::new(rebon_api::MockModelClient::new()),
                    "test-model",
                ),
            ),
            base_filter: None,
            coordinator_mode: None,
            coordinator_use_worktree: false,
            file_history_tracker: None,
            external_runner: None,
            policy: None,
        });
        let Err(error) = refused else {
            panic!("a u32 is not an engine");
        };
        assert!(error.contains("Weak<Engine>"), "{error}");

        let refused = RuntimeTeamManagerSource.for_session(TeamManagerRequest {
            engine: rebon_tool::SubAgentRuntimeHandle::new(Arc::new(
                rebon_core::Engine::with_builtin_tools(),
            )),
            task_runtime: rebon_tool::SubAgentRuntimeHandle::new(7u32),
            client: Arc::new(rebon_api::MockModelClient::new()),
            default_model: "test-model".into(),
            model_config: rebon_types::SubAgentModelConfig::default(),
            model_profiles: rebon_types::ModelProfileMap::default(),
            model_router: Arc::new(
                rebon_agent_core::model_router::SingleProviderModelRouter::new(
                    Arc::new(rebon_api::MockModelClient::new()),
                    "test-model",
                ),
            ),
            base_filter: None,
        });
        let Err(error) = refused else {
            panic!("a u32 is not a task registry resolver");
        };
        assert!(error.contains("TaskRegistryResolver"), "{error}");
    }

    /// What the front end installs when the seat is empty: every spawn path
    /// it still owns refuses in one sentence naming the switch.
    #[tokio::test]
    async fn the_stand_in_spawner_refuses_and_says_why() {
        let spawner: Arc<dyn rebon_tool::SubAgentSpawner> =
            Arc::new(rebon_tool::UnavailableSubAgentSpawner);
        let mut spec = rebon_tool::SubAgentSpec::new("do the thing");

        let preflight = spawner.preflight(&mut spec).expect_err("no sub-agents");
        assert_eq!(preflight, rebon_tool::SUB_AGENTS_UNAVAILABLE);
        let spawned = spawner
            .spawn(spec.clone())
            .await
            .expect_err("no sub-agents");
        assert_eq!(spawned, rebon_tool::SUB_AGENTS_UNAVAILABLE);
        // The detached path routes through `spawn` by default, so the TUI's
        // "resume this background agent" gets the same sentence.
        let detached = spawner
            .spawn_detached(spec)
            .await
            .expect_err("no sub-agents");
        assert_eq!(detached, rebon_tool::SUB_AGENTS_UNAVAILABLE);
    }

    /// Both back-compat spellings resolve to the same tool; the TUI's
    /// permission flow still matches on `"Agent" | "AgentTool"`.
    #[test]
    fn the_switch_reaches_the_back_compat_aliases() {
        let (kernel, registry) = boot();
        let seat = seat(&kernel);

        for alias in ["AgentTool", "Task"] {
            assert!(
                seat.resolve(alias, None)
                    .unwrap()
                    .is_some_and(|tool| tool.id().as_str() == AGENT_TOOL_NAME),
                "{alias} resolves to the Agent tool"
            );
        }

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("agents is a feature plugin");
        for alias in ["AgentTool", "Task"] {
            assert!(seat.resolve(alias, None).unwrap().is_none());
        }
    }

    /// The name the rest of the tree spells this tool by stayed in
    /// `rebon-tool`; the tool itself must keep answering to it.
    #[test]
    fn the_tool_answers_to_the_shared_name() {
        let tool = AgentTool::new();
        assert_eq!(tool.id().as_str(), AGENT_TOOL_NAME);
        assert_eq!(tool.aliases(), &["AgentTool", "Task"]);
        assert_eq!(
            AGENT_TOOL_NAME, "Agent",
            "the wire name is a compatibility surface"
        );
        assert_eq!(tool.kind(), rebon_tools_core::ToolKind::Agent);

        let shared = rebon_tools_core::BUILTIN_TOOL_FACTS
            .iter()
            .find(|entry| entry.name == AGENT_TOOL_NAME)
            .expect("Agent is in the shared facts table");
        assert_eq!(tool.aliases(), shared.aliases);
        assert_eq!(tool.kind(), shared.kind);
        assert_eq!(tool.file_target_field(), shared.file_target_field);
    }

    /// The seat provider is lazy on purpose: a host that publishes its
    /// merged registry after the kernel booted must still reach the
    /// description the model sees.
    #[test]
    fn the_seat_picks_up_a_registry_the_host_publishes_after_boot() {
        let (kernel, _registry) = boot();
        let seat = seat(&kernel);

        let before = seat
            .resolve(AGENT_TOOL_NAME, None)
            .unwrap()
            .expect("Agent is on the seat");
        let before_description = before.description().to_string();

        // A registry with the worktree-isolated agents in it renders a
        // different agent list, so the description has to change.
        rebon_tool::set_agent_registry_selection(Arc::new(AgentRegistry::builtins_only()), true);
        let after = seat
            .resolve(AGENT_TOOL_NAME, None)
            .unwrap()
            .expect("Agent is still on the seat");
        assert_ne!(
            after.description(),
            before_description,
            "publishing a registry rebuilds the tool the seat hands out"
        );

        // And a second ask with nothing republished reuses it.
        let again = seat.resolve(AGENT_TOOL_NAME, None).unwrap().unwrap();
        assert_eq!(again.description(), after.description());
    }

    /// `/agents` is this plugin's command, not a built-in: switching the
    /// plugin off takes it out of the catalogue the same way it takes
    /// `Agent` off the tool seat.
    #[test]
    fn the_switch_takes_the_command_out_of_the_catalogue_and_puts_it_back() {
        let (kernel, registry) = boot();
        let commands: Arc<CommandSeat> = kernel
            .context()
            .get::<CommandSeatService>()
            .expect("the command seat is on the root");

        let found = commands.find("agents").expect("registered while loaded");
        assert_eq!(found.owner, PLUGIN_ID);
        assert_eq!(found.handler.native_id(), Some("agents"));
        assert!(found.spec.available_on(Surface::Tui));

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("agents is a feature plugin");
        assert!(
            commands.find("agents").is_none(),
            "disabling the plugin takes /agents with it"
        );

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(commands.find("agents").is_some());
    }

    /// The panel goes the same way, and comes back opening on the
    /// working directory the front end names.
    #[test]
    fn the_switch_takes_the_panel_off_the_ui_seat_and_puts_it_back() {
        let (kernel, registry) = boot();
        let ui: Arc<UiSeat> = kernel
            .context()
            .get::<UiSeatService>()
            .expect("the ui seat is on the root");

        assert!(ui.has(dialog::DIALOG_ID));
        let project = tempfile::TempDir::new().expect("temp project");
        let opened = ui.open(
            dialog::DIALOG_ID,
            DialogArgs::payload(dialog::AgentsDialogInput {
                cwd: project.path().to_string_lossy().into_owned(),
                tool_names: vec!["Read".into()],
                initial_agent_type: None,
                external_model_options: Vec::new(),
            }),
        );
        assert_eq!(
            opened.map(|dialog| dialog.id()),
            Some(dialog::DIALOG_ID),
            "the factory builds the panel from the payload"
        );

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("agents is a feature plugin");
        assert!(!ui.has(dialog::DIALOG_ID));
        assert!(ui.open(dialog::DIALOG_ID, DialogArgs::none()).is_none());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(ui.has(dialog::DIALOG_ID));
    }

    /// Anything but the panel's own input opens nothing, rather than an
    /// empty panel rooted at the current directory.
    #[test]
    fn the_panel_declines_an_input_it_does_not_recognise() {
        let (kernel, _registry) = boot();
        let ui: Arc<UiSeat> = kernel
            .context()
            .get::<UiSeatService>()
            .expect("the ui seat is on the root");

        assert!(ui.open(dialog::DIALOG_ID, DialogArgs::none()).is_none());
        assert!(ui
            .open(dialog::DIALOG_ID, DialogArgs::values(["/some/path"]))
            .is_none());
    }
}
