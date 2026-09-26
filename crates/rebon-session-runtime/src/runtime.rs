//! The machinery a session runtime is made of.
//!
//! One session runtime is an immutable bundle installed atomically: a `/new`,
//! a resume, or an attach swaps the whole `Arc` rather than retargeting
//! journals, hooks, skills or agent backends underneath work still running.
//! [`SessionRuntimeFactory`] mints them, [`DeferredEngineCore`] defers the
//! part a mirror never pays for, and [`SessionEngineHalf`] is what only the
//! host of a session holds.
//!
//! None of it draws anything. It sat in the binary's `tui::wiring` because that is
//! where sessions were assembled, not because a terminal was involved.
use crate::build::{kernel_loop_spawner, system_prompt_config_for_engine};
use crate::mcp::SessionMcp;
use crate::rebon_config;
use rebon_acp::{DefaultHandler, ServerState};
use rebon_agent_core::{
    AgentBackend, ChannelSessionUpdatePublisher, LocalAgentBackend, PromptExecutor,
    SessionUpdatePublisher,
};
use rebon_api::{ModelClient, PruneLevelHandle};
use rebon_core::permission::{
    ChannelPermissionBroker, OutboundPermissionQuery, SharedChannelPermissionBroker,
};
use rebon_core::{
    auto_mode_classifier::ModelAutoModeClassifier,
    policy::PolicyStore,
    query::{EngineQueryExecutor, SharedRuntimeModel},
    Engine,
};
use rebon_harness::{
    kernel_bootstrap::process_kernel, resolve_command_sandbox, sub_agent_spawner_for_session,
    RuntimeModel,
};
use rebon_permissions::{
    auto_mode_denials::AutoModeDenialStore,
    denial_sink::{AutoModeHooks, PermissionModeProvider, SharedDenialSink},
    types::PermissionMode,
};
use rebon_plugin_skill::{
    load_startup_skills, RegistrySkillCatalog, SkillContext, SkillLoaderConfig, SkillRegistry,
    SkillState,
};
use rebon_plugin_tasks::{runtime::TaskRegistry, TaskRegistryResolver};
use rebon_provider::provider_runtime_cache::ProviderRuntimeCache;
use rebon_tool::{
    AgentRegistry, Extensions, ExternalSubAgentRunner, McpClient, SubAgentRuntimeHandle,
    SubAgentSpawner, SubAgentSpawnerRequest, TaskRuntimeController, TeamManager, ToolFilter,
    UnavailableSubAgentSpawner, WorkflowLauncher, WorkflowLauncherRequest, WorkflowLauncherService,
    WorkflowTaskRuntimeHandle,
};
use rebon_types::SessionUpdateParams;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

/// Immutable resources that define one TUI session runtime.
///
/// `TuiEngineSession` keeps only the currently installed `Arc`; every spawned
/// prompt captures its own clone. A `/new`, resume, or attach installs a fresh
/// value instead of retargeting journals, host-fs services, hooks, skills or
/// agent backends underneath work that is still running.
pub struct SessionRuntime {
    pub session_id: String,
    pub projects_root: PathBuf,
    pub cwd: String,
    pub executor: Arc<dyn PromptExecutor>,
    /// This session's local (engine) backend. `session_agents` holds the same
    /// `Arc` and is what a turn actually routes through; this one is here for
    /// a runtime swap, which mints the next session's switch from it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub backend: Arc<dyn AgentBackend>,
    pub session_agents:
        Arc<rebon_agent_core::routing::SessionAgents<rebon_acp_client::AcpAgentBackend>>,
    pub acp_subagent_pool: Option<Arc<crate::acp_subagent_pool::AcpSubAgentPool>>,
    pub sub_agent_spawner: Arc<dyn SubAgentSpawner>,
    pub file_history_tracker: crate::SharedFileHistoryTracker,
    pub policy: rebon_core::policy_seat::PolicySources,
    pub skill_registry: Arc<SkillRegistry>,
    /// The startup skill scan's state. Production reads it only through the
    /// executor's `SkillContext`, which holds its own clone of the same `Arc`;
    /// the session's copy is what `/skill` and `/resume` tests reach for.
    #[allow(dead_code)]
    pub skill_state: Arc<std::sync::Mutex<SkillState>>,
    pub update_publisher: Arc<dyn SessionUpdatePublisher>,
    pub permission_broker: SharedChannelPermissionBroker,
    pub mid_turn_queue: Arc<crate::mid_turn_queue::MidTurnQueuedSubmitPoller>,
    pub tasks: Arc<TaskRegistry>,
    pub task_notification_poller: Arc<crate::task_notification_poller::TaskNotificationPoller>,
    pub runtime_handle: Option<tokio::runtime::Handle>,
}

impl std::fmt::Debug for SessionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRuntime")
            .field("session_id", &self.session_id)
            .field("cwd", &self.cwd)
            .field("agent", &self.session_agents.current_id())
            .finish_non_exhaustive()
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        // The runtime Arc is held by every active/detached prompt. Reaching
        // Drop therefore means no turn can still be using these backends.
        if let Some(runtime_handle) = &self.runtime_handle {
            self.session_agents.retire(runtime_handle);
            if let Some(pool) = self.acp_subagent_pool.clone() {
                runtime_handle.spawn(async move {
                    pool.shutdown().await;
                });
            }
        }
    }
}

pub struct SessionRuntimeInstall {
    pub runtime: Arc<SessionRuntime>,
    pub update_rx: UnboundedReceiver<SessionUpdateParams>,
    pub permission_rx: UnboundedReceiver<OutboundPermissionQuery>,
}

/// Root/process-scoped inputs used to mint immutable session runtimes.
pub struct SessionRuntimeFactory {
    /// Where the fork template lives. Deferred so that a mirror, which
    /// never builds a runtime unless it becomes this process's session,
    /// does not pay for the engine core just to carry a factory.
    /// `build` is the factory-side reader that mints
    /// it late.
    pub engine_core: Arc<DeferredEngineCore>,
    pub engine: Arc<Engine>,
    pub client: Arc<dyn ModelClient>,
    pub default_model: String,
    pub model_profiles: rebon_types::ModelProfileMap,
    pub sub_agent_model_config: rebon_types::SubAgentModelConfig,
    pub sub_agent_model_router: Arc<dyn rebon_agent_core::model_router::AgentModelRouter>,
    pub subagent_filter_handle: rebon_tool::SharedToolFilter,
    pub coordinator_mode_handle: rebon_tool::SharedCoordinatorMode,
    pub coordinator_use_worktree: bool,
    /// The owner boundary every session's services hang off, task state
    /// included. Held as the *table* rather than as one session's registry:
    /// this factory is process-scoped and is asked to build a runtime for
    /// whichever session the terminal moved to, so the registry has to be
    /// looked up per build.
    ///
    /// It used to be one `Arc<TaskRegistry>` plus the session id it belonged
    /// to, and `build` refused any other session rather than hand over the
    /// wrong session's task state. The refusal was right about the danger and
    /// wrong about the remedy: it made every legitimate move -- `/new`,
    /// `/resume`, `rebon attach` -- fail, because each of those is exactly a
    /// build for a session id the factory was not minted with.
    pub kernel_scopes: Arc<rebon_kernel_seats::kernel_services::SessionKernelScopes>,
    pub task_registry_resolver: TaskRegistryResolver,
    pub local_declared_agents: Vec<rebon_agent_core::routing::DeclaredAgent>,
    pub declared_agents: Vec<rebon_agent_core::routing::DeclaredAgent>,
    pub acp_config_error: Option<String>,
    pub runtime_add_dirs: Vec<String>,
    pub projects_root: PathBuf,
    pub transcript_sink: rebon_acp_client::TranscriptSink,
    pub skill_loader: SkillLoaderConfig,
    pub plugin_hooks: Vec<rebon_hooks::IndividualHookConfig>,
    pub auto_mode_denials: Arc<std::sync::Mutex<AutoModeDenialStore>>,
    pub auto_mode_verdicts: Arc<rebon_permissions::AutoModeVerdictCache>,
    pub permission_mode_cell: Arc<std::sync::Mutex<PermissionMode>>,
    /// The session table, for what only a record knows about a session's
    /// mode — where it entered plan mode from.
    pub server_state: Arc<rebon_acp::ServerState>,
    pub runtime_model: SharedRuntimeModel,
    pub kernel_context_resolver: rebon_core::query::KernelSessionContextResolver,
    pub session_cron_store: Arc<rebon_tool::SessionCronStore>,
    pub additional_attachment_poller: Option<Arc<dyn rebon_core::query::AttachmentPoller>>,
    pub runtime_handle: tokio::runtime::Handle,
    /// What `/provider reconnect` drops.
    pub provider_runtimes: Arc<ProviderRuntimeCache<RuntimeModel>>,
}

impl SessionRuntimeFactory {
    pub async fn build(
        &self,
        session_id: &str,
        cwd: &str,
        preferred_agent: &str,
    ) -> anyhow::Result<SessionRuntimeInstall> {
        // This session's task state, from the scope that owns it. The lease is
        // held across the lookup rather than taken and dropped: `acquire` is
        // what binds a scope that does not exist yet (a `/new` session, or one
        // `rebon attach` moved to), and `host_task_registry` deliberately does
        // not bind -- it reads an already-bound scope and says so in its own
        // documentation. Holding the lease is what makes the second call's
        // precondition true, and what keeps a scope at the capacity limit from
        // being evicted between the two.
        let tasks = {
            let _bound = self.kernel_scopes.acquire(session_id);
            self.kernel_scopes.host_task_registry(session_id)
        };
        let skill_registry = Arc::new(SkillRegistry::new());
        match crate::rebon_config::load_disabled_skills() {
            Ok(disabled) => skill_registry.set_disabled_skills(disabled),
            Err(err) => {
                tracing::warn!(error = %err, "session runtime: failed to load disabled skills")
            }
        }
        let mut skill_loader = self.skill_loader.clone();
        skill_loader.cwd = cwd.to_string();
        skill_loader.session_id = session_id.to_string();
        let skill_state = load_startup_skills(&skill_loader, &skill_registry).await;

        let file_history_tracker =
            crate::SharedFileHistoryTracker::new(rebon_session::FileHistoryStore::new(
                self.projects_root.clone(),
                PathBuf::from(cwd),
                session_id,
            ));
        let tracker_dyn = Arc::new(file_history_tracker.clone())
            as Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>;

        let local_declared = self.local_declared_agents.clone();
        let acp_subagent_pool = (!local_declared.is_empty()).then(|| {
            crate::acp_subagent_pool::pool_from_declared(
                local_declared,
                cwd.to_string(),
                self.runtime_add_dirs.iter().map(PathBuf::from).collect(),
                Some(tracker_dyn.clone()),
            )
        });
        // Before the spawner: a sub-agent inherits this session's subscribers.
        //
        // The same constructor the session builder used at startup, rather
        // than a second copy of the subscriber list: this one had drifted,
        // and rebinding a session to another directory dropped the process
        // seat — every plugin subscribed to `policy-events` stopped being
        // asked for the rest of the run.
        let policy = rebon_harness::session_assembly::policy_sources_for_session(
            session_id,
            cwd,
            self.plugin_hooks.clone(),
            &self.projects_root,
        );
        let sub_agent_spawner = sub_agent_spawner_for_session(SubAgentSpawnerRequest {
            engine: SubAgentRuntimeHandle::new(Arc::downgrade(&self.engine)),
            policy: Some(SubAgentRuntimeHandle::new(policy.clone())),
            task_runtime: Some(SubAgentRuntimeHandle::new(
                self.task_registry_resolver.clone(),
            )),
            client: self.client.clone(),
            default_model: self.default_model.clone(),
            model_config: self.sub_agent_model_config.clone(),
            model_profiles: self.model_profiles.clone(),
            model_router: self.sub_agent_model_router.clone(),
            base_filter: Some(self.subagent_filter_handle.clone()),
            coordinator_mode: Some(self.coordinator_mode_handle.clone()),
            coordinator_use_worktree: self.coordinator_use_worktree,
            file_history_tracker: Some(tracker_dyn.clone()),
            external_runner: acp_subagent_pool
                .clone()
                .map(|pool| pool as Arc<dyn ExternalSubAgentRunner>),
        })
        .unwrap_or_else(|| Arc::new(UnavailableSubAgentSpawner));

        let workflow_launcher = workflow_launcher_for_session(WorkflowLauncherRequest {
            task_runtime: WorkflowTaskRuntimeHandle::new(self.task_registry_resolver.clone()),
            cwd: PathBuf::from(cwd),
            config_home_dir: crate::rebon_config::config_home_dir(),
            session_root: self.projects_root.clone(),
            model_profiles: self.model_profiles.clone(),
            active_provider: Some(self.runtime_model.get().provider_name),
        });

        let (permission_broker, permission_rx) =
            ChannelPermissionBroker::new(session_id.to_string());
        {
            let sink = SharedDenialSink::new(self.auto_mode_denials.clone());
            let provider: Arc<dyn PermissionModeProvider> =
                Arc::new(rebon_acp::session::SessionPermissionModeSource::cell(
                    Arc::clone(&self.server_state),
                    session_id,
                    self.permission_mode_cell.clone(),
                ));
            permission_broker.set_auto_mode_hooks(Some(
                AutoModeHooks::new(Arc::new(sink), provider)
                    .with_verdicts(self.auto_mode_verdicts.clone()),
            ));
        }
        let runtime_model = self.runtime_model.fork_session();
        permission_broker.set_auto_mode_classifier(Some(Arc::new(ModelAutoModeClassifier::new(
            runtime_model.clone(),
        ))));
        permission_broker.set_kernel_context_resolver(self.kernel_context_resolver.clone());
        let permission_broker: SharedChannelPermissionBroker = Arc::new(permission_broker);

        let mid_turn_queue = crate::mid_turn_queue::MidTurnQueuedSubmitPoller::new(session_id);
        let mut user_poller: Arc<dyn rebon_core::query::AttachmentPoller> = mid_turn_queue.clone();
        if let Some(additional) = &self.additional_attachment_poller {
            user_poller = Arc::new(rebon_core::cron::CompositePoller::new(
                user_poller,
                additional.clone(),
            ));
        }
        let task_notification_poller =
            crate::task_notification_poller::TaskNotificationPoller::new_session_resolving(
                self.task_registry_resolver.clone(),
                tasks.subscribe_notification_revision(),
            );
        let extra_attachment_poller: Arc<dyn rebon_core::query::AttachmentPoller> = Arc::new(
            rebon_core::cron::CompositePoller::new(user_poller, task_notification_poller.clone()),
        );

        let engine_executor = Arc::new(
            self.engine_core
                .core()
                .blueprint
                .fork_with_session_runtime(rebon_core::query::SessionExecutionRuntime {
                    sub_agent_spawner: Some(sub_agent_spawner.clone()),
                    workflow_launcher,
                    turn_skill_catalog: Some(Arc::new(RegistrySkillCatalog::new(
                        skill_registry.clone(),
                    ))),
                    extensions: session_plugin_extensions(&skill_registry, &skill_state),
                    permission_broker: permission_broker.clone()
                        as Arc<dyn rebon_tool::PermissionBroker>,
                    extra_attachment_poller: Some(extra_attachment_poller),
                    session_cron_store: self.session_cron_store.clone(),
                    file_history_tracker: tracker_dyn.clone(),
                    policy: policy.clone(),
                })
                .with_shared_runtime_model(runtime_model),
        );
        let backend: Arc<dyn AgentBackend> = Arc::new(LocalAgentBackend::new(
            engine_executor as Arc<dyn PromptExecutor>,
        ));
        let agent_switch = Arc::new(rebon_agent_core::AgentBackendSwitch::new(backend.clone()));
        let executor: Arc<dyn PromptExecutor> = agent_switch.clone();

        let mut write_roots = vec![PathBuf::from(cwd)];
        write_roots.extend(self.runtime_add_dirs.iter().map(PathBuf::from));
        let mut agents = rebon_harness::agent_assembly::build_session_agents(
            self.declared_agents.clone(),
            agent_switch,
            backend.clone(),
            self.projects_root.clone(),
            cwd.to_string(),
            session_id.to_string(),
            tracker_dyn,
            write_roots,
            Some(self.transcript_sink.clone()),
        )
        .with_config_error(self.acp_config_error.clone());
        let config_dir = rebon_config::config_home_dir();
        use rebon_plugin_host::kernel_loop_backend::{KernelLoopBackend, KernelLoopConfig};
        use rebon_plugin_host::loop_host::{loop_backend_id, KNOWN_LOOP_VENDORS};
        for vendor in KNOWN_LOOP_VENDORS {
            let Some(loop_config) = KernelLoopConfig::for_vendor(&config_dir, vendor.id) else {
                continue;
            };
            let backend_id = loop_backend_id(vendor.id);
            let journal = Arc::new(
                rebon_acp_client::TranscriptJournal::new(
                    self.projects_root.clone(),
                    cwd.to_string(),
                    session_id.to_string(),
                    backend_id.clone(),
                )
                .with_transcript_sink(self.transcript_sink.clone()),
            );
            agents = agents.with_kernel_backend(
                backend_id.clone(),
                vendor.label,
                Arc::new(KernelLoopBackend::new(
                    backend_id,
                    kernel_loop_spawner(),
                    loop_config,
                    journal,
                )),
            );
        }
        let session_agents = Arc::new(agents);
        if !preferred_agent.eq_ignore_ascii_case(rebon_config::LOCAL_AGENT_ID) {
            session_agents.switch_to(preferred_agent).map_err(|err| {
                anyhow::anyhow!(
                    "could not preserve agent `{preferred_agent}` for session {session_id}: {err}"
                )
            })?;
        }

        let (update_publisher, update_rx) = ChannelSessionUpdatePublisher::new();
        let update_publisher: Arc<dyn SessionUpdatePublisher> = Arc::new(update_publisher);
        let runtime = Arc::new(SessionRuntime {
            session_id: session_id.to_string(),
            projects_root: self.projects_root.clone(),
            cwd: cwd.to_string(),
            executor,
            backend,
            session_agents,
            acp_subagent_pool,
            sub_agent_spawner,
            file_history_tracker,
            policy,
            skill_registry,
            skill_state,
            update_publisher,
            permission_broker,
            mid_turn_queue,
            tasks,
            task_notification_poller,
            runtime_handle: Some(self.runtime_handle.clone()),
        });
        Ok(SessionRuntimeInstall {
            runtime,
            update_rx,
            permission_rx,
        })
    }
}

/// Everything [`build_engine_core`] needs, captured while the session was
/// wired. Cheap clones of handles the wiring builds anyway; the point of
/// the bundle is that a mirror can carry it without paying for what it
/// would build.
pub struct EngineCoreInputs {
    pub startup_started: std::time::Instant,
    pub engine: Arc<Engine>,
    pub client: Arc<dyn ModelClient>,
    pub projects_root: PathBuf,
    pub model: String,
    pub capability_mode: rebon_types::AgentCapabilityMode,
    pub startup_session_filter: ToolFilter,
    pub system_prompt_snapshot: rebon_core::query::SystemPromptSnapshot,
    pub title_model: String,
    pub sub_agent_spawner: Arc<dyn SubAgentSpawner>,
    pub skill_registry: Arc<SkillRegistry>,
    /// `None` when nothing is on the `team-manager` seat, which is what
    /// `plugins.agents.enabled = false` looks like: the executor attaches no
    /// manager and the team tools answer "no team runtime here".
    pub team_manager: Option<Arc<dyn TeamManager>>,
    pub task_runtime_controller: Arc<dyn TaskRuntimeController>,
    pub tasks: Arc<TaskRegistry>,
    pub task_registry_resolver: TaskRegistryResolver,
    pub live_policy_store: PolicyStore,
    pub session_filter_handle: rebon_tool::SharedToolFilter,
    pub coordinator_mode_handle: rebon_tool::SharedCoordinatorMode,
    pub coordinator_use_worktree: bool,
    pub server_state: Arc<ServerState>,
    pub prune_level: PruneLevelHandle,
    pub cron_poller: Arc<rebon_core::cron::CronPoller>,
    pub extra_attachment_poller: Arc<dyn rebon_core::query::AttachmentPoller>,
    pub session_cron_store: Arc<rebon_tool::SessionCronStore>,
    pub runtime_model: SharedRuntimeModel,
    pub sandbox_cwd: PathBuf,
    pub mcp_client: Arc<dyn McpClient>,
    pub provider_name: String,
    pub skill_state: Arc<std::sync::Mutex<SkillState>>,
    pub cwd: String,
    pub model_profiles: rebon_types::ModelProfileMap,
    pub kernel_context_resolver: rebon_core::query::KernelSessionContextResolver,
    pub kernel_plugin_tools: Arc<dyn rebon_tool::PluginToolProvider>,
    pub file_history_tracker: crate::SharedFileHistoryTracker,
    pub permission_broker: SharedChannelPermissionBroker,
    pub policy: rebon_core::policy_seat::PolicySources,
}

/// The heavy inside of a session: the engine executor wired end to end,
/// and the two handles only it can mint. ≈250 ms of a debug build plus an
/// engine's worth of resident memory — the part of a session a mirror
/// never uses.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub struct EngineCore {
    /// The fully-wired engine executor for this session.
    pub executor: Arc<EngineQueryExecutor>,
    /// Replay/compact handle minted by the executor.
    pub resume_replay: rebon_core::query::ResumeReplayHandle,
    /// The template [`SessionRuntimeFactory::build`] forks runtimes from.
    pub blueprint: Arc<EngineQueryExecutor>,
}

/// [`EngineCore`], built the first time something needs it.
///
/// A session built to run turns fills this during wiring — same products,
/// same checkpoints, built from the same inputs the inline code used. A
/// mirror leaves it empty: it shows a session the worker runs, and nothing
/// on the mirror path reads the core. The one who pays late is a mirror
/// becoming this process's session after all (`/new` falling back to
/// local, a failed `/hosted` handover taken back): its first `core()`
/// builds inline on the calling thread, ≈250 ms, and logs that it did.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub struct DeferredEngineCore {
    cell: std::sync::OnceLock<EngineCore>,
    inputs: std::sync::Mutex<Option<EngineCoreInputs>>,
}

impl DeferredEngineCore {
    pub fn new(inputs: EngineCoreInputs) -> Self {
        Self {
            cell: std::sync::OnceLock::new(),
            inputs: std::sync::Mutex::new(Some(inputs)),
        }
    }

    /// A core already in hand — tests, which stub the executor anyway.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn prebuilt(core: EngineCore) -> Self {
        let cell = std::sync::OnceLock::new();
        let _ = cell.set(core);
        Self {
            cell,
            inputs: std::sync::Mutex::new(None),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn is_built(&self) -> bool {
        self.cell.get().is_some()
    }

    /// The core, built now if this is the first thing to need it. Every
    /// caller but the wiring's own eager fill reaches a built cell or is
    /// the late payer — that build gets its own log line.
    ///
    /// The build is synchronous and infallible — nothing in the deferred
    /// segment returns an error; a sandbox block that cannot be read is
    /// logged and the policy left inert, exactly as at startup.
    pub fn core(&self) -> &EngineCore {
        self.core_impl(true)
    }

    /// The wiring's eager fill: same build, but it is not a deferred
    /// payment and must not log as one — the moved checkpoints inside
    /// `build_engine_core` are its trace.
    pub(crate) fn build_at_wiring(&self) -> &EngineCore {
        self.core_impl(false)
    }

    fn core_impl(&self, late: bool) -> &EngineCore {
        if let Some(core) = self.cell.get() {
            return core;
        }
        // Serialize the build; a loser of the race finds the cell filled.
        let mut inputs = self.inputs.lock().expect("engine core inputs poisoned");
        if let Some(inputs) = inputs.take() {
            let started = std::time::Instant::now();
            let core = build_engine_core(inputs);
            if late {
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "rebon: built the engine core this session had deferred"
                );
            }
            let _ = self.cell.set(core);
        }
        self.cell
            .get()
            .expect("engine core neither built nor buildable")
    }
}

/// The prompt executor a mirror's local backend wraps: nothing behind it
/// until a turn actually runs here. A mirror's submits go to the owner
/// before local dispatch is reached, so this executes only after the
/// session has become this process's — and by then `core()` has usually
/// been paid by the runtime swap that made it so.
pub(crate) struct DeferredEnginePromptExecutor {
    pub core: Arc<DeferredEngineCore>,
}

#[async_trait::async_trait]
impl PromptExecutor for DeferredEnginePromptExecutor {
    async fn execute(
        &self,
        request: rebon_agent_core::PromptRequest,
    ) -> Result<rebon_agent_core::PromptOutcome, rebon_agent_core::PromptExecutorError> {
        self.core.core().executor.execute(request).await
    }
}

/// This session's plugin-owned state, in the one bag a turn carries.
///
/// The engine forwards it whole to the turn's `ToolContext` and to its hook
/// events without reading it. Skills are the only feature in it today; a
/// second one inserts its own value here rather than growing
/// `SessionExecutionRuntime` another field.
fn session_plugin_extensions(
    skill_registry: &Arc<SkillRegistry>,
    skill_state: &Arc<std::sync::Mutex<SkillState>>,
) -> Extensions {
    let mut extensions = Extensions::default();
    extensions.insert(SkillContext::new(
        skill_registry.clone(),
        skill_state.clone(),
    ));
    extensions
}

/// Build the engine core: everything between the "system prompt config ready"
/// and "query executor built" startup checkpoints, plus the blueprint fork.
///
/// The checkpoints are logged from here, after "startup skills loaded",
/// because the inputs include everything session-scoped — a session that runs
/// turns logs them in that order.
fn build_engine_core(inputs: EngineCoreInputs) -> EngineCore {
    let EngineCoreInputs {
        startup_started,
        engine,
        client,
        projects_root,
        model,
        capability_mode,
        startup_session_filter,
        system_prompt_snapshot,
        title_model,
        sub_agent_spawner,
        skill_registry,
        team_manager,
        task_runtime_controller,
        tasks,
        task_registry_resolver,
        live_policy_store,
        session_filter_handle,
        coordinator_mode_handle,
        coordinator_use_worktree,
        server_state,
        prune_level,
        cron_poller,
        extra_attachment_poller,
        session_cron_store,
        runtime_model,
        sandbox_cwd,
        mcp_client,
        provider_name,
        skill_state,
        cwd,
        model_profiles,
        kernel_context_resolver,
        kernel_plugin_tools,
        file_history_tracker,
        permission_broker,
        policy,
    } = inputs;

    // Build the system prompt config for lazy resolution. Static info is
    // captured now; dynamic info (cwd, is_git, CLAUDE.md) is resolved
    // per-turn inside execute(). Use the filtered tool names so the system
    // prompt only references tools the session can actually use.
    let system_prompt_config =
        system_prompt_config_for_engine(&engine, &startup_session_filter, &model, true);
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: system prompt config ready"
    );

    // Sandbox: settings → policy → every `Bash` / `PowerShell` call. A
    // `sandbox` block that cannot be read is logged and the session
    // continues *unsandboxed* rather than dying — there is nowhere here to
    // surface a fatal error to the user. The note is what `/doctor` and
    // the log show; the policy that goes on is inert, so nothing claims
    // the commands are confined.
    let command_sandbox = resolve_command_sandbox(&sandbox_cwd);
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: sandbox policy resolved"
    );

    // The prompt executor with every extension. Thinking is enabled
    // per-turn by the runner based on the current effort level; the
    // executor keeps its default max_tokens.
    let mut executor = EngineQueryExecutor::new(engine, client, projects_root.clone(), model)
        .with_capability_mode(capability_mode)
        .with_system_prompt_config(system_prompt_config)
        .with_system_prompt_snapshot(system_prompt_snapshot)
        .with_title_model(title_model)
        .with_sub_agent_spawner(sub_agent_spawner)
        .with_turn_skill_catalog(Arc::new(RegistrySkillCatalog::new(skill_registry.clone())))
        .with_task_runtime_controller(task_runtime_controller)
        .with_queue_controller(std::sync::Arc::new(
            crate::queue_controller::PublishedQueueController,
        ))
        .with_escalation_resolver(tasks.escalation_registry().resolver())
        .with_policy_store(live_policy_store)
        .with_shared_tool_filter(session_filter_handle)
        .with_shared_coordinator_mode(coordinator_mode_handle)
        .with_coordinator_use_worktree(coordinator_use_worktree)
        .with_server_state(server_state)
        .with_prune_level(prune_level)
        .with_cron_poller(cron_poller)
        .with_extra_attachment_poller(extra_attachment_poller)
        .with_session_cron_store(session_cron_store)
        .with_shared_runtime_model(runtime_model);
    if let Some(team_manager) = team_manager {
        executor = executor.with_team_manager(team_manager);
    }
    if let Some(sandbox) = command_sandbox {
        executor = executor.with_command_sandbox(sandbox);
    }
    executor = executor.with_mcp_client(mcp_client);
    executor = executor.with_extension(SkillContext::new(skill_registry, skill_state));
    let workflow_launcher = workflow_launcher_for_session(WorkflowLauncherRequest {
        task_runtime: WorkflowTaskRuntimeHandle::new(task_registry_resolver),
        cwd: std::path::PathBuf::from(&cwd),
        config_home_dir: crate::rebon_config::config_home_dir(),
        session_root: projects_root,
        model_profiles,
        active_provider: Some(provider_name.clone()),
    });
    if let Some(workflow_launcher) = workflow_launcher {
        executor = executor.with_workflow_launcher(workflow_launcher);
    }
    executor = executor.with_kernel_context_resolver(kernel_context_resolver);
    executor = executor
        .with_plugin_tools(kernel_plugin_tools)
        .with_web_provider_router(rebon_kernel_seats::kernel_web_seat::KernelWebRouter::shared());
    executor = executor.with_file_history_tracker(Arc::new(file_history_tracker)
        as Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>);
    executor =
        executor.with_permission_broker(permission_broker as Arc<dyn rebon_tool::PermissionBroker>);
    executor = executor.with_policy(policy);
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: query executor built"
    );

    let resume_replay = executor.resume_replay_handle();
    let blueprint = Arc::new(executor.session_runtime_blueprint());
    EngineCore {
        executor: Arc::new(executor),
        resume_replay,
        blueprint,
    }
}

/// Fully-wired harness session for the local TUI path.
///
/// Produced by [`build_tui_session`]. Holds every handle the TUI
/// event loop needs to keep the harness alive plus the receiver ends
/// of the update + permission channels so the loop can drain them
/// synchronously from inside `tokio::task::spawn_blocking`.
///
/// The [`DefaultHandler`] is kept whole rather than projected down to
/// the specific methods the runner needs — the submit path will
/// call into the handler directly and it is not yet clear which
/// methods that will require. Keeping the handler field flat also
/// means this struct evolves by addition rather than re-shaping,
/// which matches the rest of the plan's "no refactors until we
/// know what we need" posture.
/// The half of a session that only the process running its turns can have.
///
/// A terminal that is a client of somebody else's session has none of this:
/// no executor, no MCP stack, no permission broker, no file-history tracker.
/// It has a session id, a cwd, and a socket.
///
/// Today every session still builds one — the field is not optional yet, and
/// this is deliberately a regrouping and nothing more. Naming the boundary is
/// what makes "do not build it" expressible at all: while these lived beside
/// `session_id` and `cwd`, there was no way to say which fields a mirror is
/// entitled to and no compiler to check it. See RFC-0004 §15.1 for the
/// division and §16.6 for how the rest of it lands.
///
/// Kept as public fields rather than behind an accessor on purpose: the runner
/// borrows several parts of a session at once on nearly every frame
/// (`&mut session.engine_half.update_rx` next to `&session.session_id`), and a
/// method returning `&SessionEngineHalf` would borrow the whole session and
/// break every one of those call sites.
///
/// A field here is either something only this half has, or a handle a surface
/// re-points on its own between runtime installs (the skill registry, the task
/// registry). What the runtime already owns is read through [`Self::runtime`]
/// rather than mirrored: seven mirrors used to be copied field by field in
/// `install_runtime`, and a mirror that anyone forgot to copy would have gone
/// on answering with the previous session's handle.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub struct SessionEngineHalf {
    /// Direct handle on the prompt executor. Cloned from the same
    /// `Arc<dyn PromptExecutor>` the handler holds, so calls via this field
    /// drive the same engine state.
    pub executor: Arc<dyn PromptExecutor>,
    /// Receiver end of the typed permission-query channel.
    pub permission_rx: UnboundedReceiver<OutboundPermissionQuery>,
    /// Shared live policy store behind both permission brokers.
    pub live_policy_store: PolicyStore,
    /// The MCP servers, while this process hosts them. See [`SessionMcp`].
    pub mcp: Option<SessionMcp>,
    /// Already-loaded agent registry shared with the engine.
    pub agent_registry: Arc<AgentRegistry>,
    /// Session-scoped teammate runtime.
    pub team_manager: Option<Arc<dyn TeamManager>>,
    /// Session-scoped denial store the broker pushes into under auto mode.
    pub auto_mode_denials: Arc<std::sync::Mutex<AutoModeDenialStore>>,
    /// What this session's turns have cost, counted once for the session
    /// rather than once per screen.
    ///
    /// Here rather than on `AppState` because a hosted session's turns run in
    /// a worker, which builds its own `AppState` to answer a command with: a
    /// count kept there answered `/cost` with the defaults of a state no turn
    /// had ever passed through. Both processes write this one, and every
    /// surface reads a snapshot of it.
    pub usage_ledger: Arc<std::sync::Mutex<crate::usage::UsageLedger>>,
    /// Deny-fingerprint / one-shot-exemption cache shared with the auto gate.
    pub auto_mode_verdicts: Arc<rebon_permissions::AutoModeVerdictCache>,
    /// Shared handle on the sub-agent spawner's base tool filter.
    pub subagent_filter_handle: rebon_tool::SharedToolFilter,
    /// Shared retry-state handle written by the retry middleware.
    pub retry_notifier: rebon_api::RetryNotifier,
    /// Atomically installed immutable resources for the current session.
    /// Prompt dispatch clones this Arc before spawning.
    pub runtime: Arc<SessionRuntime>,
    pub runtime_factory: Option<Arc<SessionRuntimeFactory>>,
    /// Fully-wired ACP handler. Owns its own clones of the prompt
    /// executor, the update publisher, and the permission publisher.
    /// Kept alive so the `serve_with_publishers`-shaped path stays
    /// available for any future backend that wants to drive the
    /// handler directly; the TUI submit path **bypasses** the
    /// handler and calls the executor directly via [`Self::executor`]
    /// below.
    ///
    /// The runner reads it for one thing: `apply_config_option_local`,
    /// the session-option path (`/model`, permission mode, sub-agent
    /// toggles). Session records are read through
    /// [`TuiEngineSession::server_state`] on the client half instead —
    /// a mirror needs those, and it must not need this.
    pub handler: DefaultHandler,
    /// Cloned update publisher. Re-used per [`PromptRequest`] so the
    /// streaming assistant reply flows back to `update_rx` below.
    /// The submit path stuffs this into `PromptRequest::update_publisher` on
    /// every submit.
    pub update_publisher: Arc<dyn SessionUpdatePublisher>,
    /// The deferred engine core: the wired executor, its
    /// replay handle and the factory's fork template. Filled during wiring
    /// for a session that runs turns; empty in a mirror until the moment —
    /// if ever — the session becomes this process's. Read it through
    /// [`Self::resume_replay`] or [`DeferredEngineCore::core`].
    pub engine_core: Arc<DeferredEngineCore>,
    /// In-flight immediate `/compact`, if any. Session-scoped because the
    /// compacted history is installed against this session's replay
    /// baseline, and a `/resume` mid-run must invalidate the result.
    pub compact_runtime: crate::compact::CompactRuntime,
    /// Set after canonical disk history commits but either in-memory projection
    /// fails. New turns are rejected until the session is rebuilt/reloaded.
    pub projection_invalid: Arc<std::sync::atomic::AtomicBool>,
    /// Receiver end of the session-update channel. Drained each
    /// frame by the runner and fed into
    /// the binary's `tui::update::translate_session_update`.
    pub update_rx: UnboundedReceiver<SessionUpdateParams>,
    /// Shared handle on the coordinator's background-task registry.
    ///
    /// Cloned into the binary's `tui::app::AppState::tasks` at the start
    /// of the binary's `tui::runner::run_blocking` so the TUI render +
    /// event-handling layer reads the same snapshots any future
    /// spawn point (async BashTool, `/run` command, sub-agent
    /// queries) writes to. Minted here — rather than in the runner —
    /// because the same registry needs to be reachable from the
    /// executor / handler / spawn side once those grow task-aware
    /// code paths.
    pub tasks: Arc<TaskRegistry>,
    /// Shared engine handle. Cloned into the background-task spawn
    /// points (the `/run` slash command and any future async tool
    /// promotion path) so they can call
    /// [`rebon_plugin_tasks::runtime::spawn_local_shell_task`] without
    /// having to reach into the executor.
    pub engine: Arc<Engine>,
    /// Shared sub-agent spawner used by runtime features that need to
    /// launch local background reviewers without going through the model-facing Agent tool.
    pub sub_agent_spawner: Arc<dyn SubAgentSpawner>,
    /// Already-loaded skill registry shared with the executor. `/context`
    /// reads this snapshot instead of re-running skill discovery.
    pub skill_registry: Arc<SkillRegistry>,
    /// Shared model client retained for session runtime features that need the
    /// already-resolved provider connection.
    pub client: Arc<dyn ModelClient>,
    /// Last fully-resolved system prompt captured by the executor.
    pub system_prompt_snapshot: rebon_core::query::SystemPromptSnapshot,
    /// Shared handle on the session-level tool filter. The `/ceo`
    /// slash command swaps this cell to restrict (or un-restrict)
    /// which tools the running executor sees on its next iteration.
    pub session_filter_handle: rebon_tool::SharedToolFilter,
    pub coordinator_mode_handle: rebon_tool::SharedCoordinatorMode,
    /// Writable cell that matches `AppState::permission_mode`. The
    /// broker reads it via a `PermissionModeProvider` closure so the
    /// engine can branch on the current mode without reaching into
    /// TUI-only state. Shift+Tab cycling writes through this cell.
    pub permission_mode_cell: Arc<std::sync::Mutex<PermissionMode>>,
    /// Scheduler handle for the cron worker. Kept on the session so
    /// the background task stays alive for the lifetime of the TUI
    /// and shuts down (releases the cross-process owner lock) when
    /// the session drops. Stored as `Option` only so
    /// [`SchedulerHandle::stop`] can take the inner value; the field
    /// is `Some(...)` throughout normal operation.
    pub cron_scheduler: Option<rebon_core::cron::SchedulerHandle>,
    /// What a scheduler fires into and reads from, kept apart from the
    /// scheduler itself so a session built as a mirror (which runs none) can
    /// start one at the moment it becomes this process's.
    pub cron_poller: Arc<rebon_core::cron::CronPoller>,
    pub session_cron_store: Arc<rebon_tool::SessionCronStore>,
    /// Session-scoped bridge that injects terminal background-agent
    /// notifications into the active query and coordinates delivery
    /// with the idle notification fallback.
    pub task_notification_poller: Arc<crate::task_notification_poller::TaskNotificationPoller>,
    /// The owner boundary for all session services, including task-registry.
    pub kernel_scopes: Arc<rebon_kernel_seats::kernel_services::SessionKernelScopes>,
}

impl SessionEngineHalf {
    /// The replay/compact handle, minting the deferred engine core if this
    /// is the first thing to need it. Every caller is on an owner path
    /// (resume, `/compact`, the worker's rewind) — a mirror never reads
    /// this, so a mirror never pays here.
    pub fn resume_replay(&self) -> rebon_core::query::ResumeReplayHandle {
        self.engine_core.core().resume_replay.clone()
    }
}

/// Build this session's workflow launcher off the `workflow-launcher` seat.
///
/// `None` is the ordinary answer when `plugins.workflow.enabled` is false:
/// nothing provides the seat, the executor gets no launcher, and the
/// `Workflow` tool is off the tool seat in the same breath. A provider that
/// refuses is a wiring fault and is logged rather than silently swallowed.
pub fn workflow_launcher_for_session(
    request: WorkflowLauncherRequest,
) -> Option<Arc<dyn WorkflowLauncher>> {
    let source = process_kernel()
        .context()
        .get::<WorkflowLauncherService>()?;
    match source.for_session(request) {
        Ok(launcher) => Some(launcher),
        Err(error) => {
            tracing::warn!(%error, "workflow launcher seat refused this session");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel_seats::kernel_services::SessionKernelScopes;

    fn scopes() -> (tempfile::TempDir, Arc<SessionKernelScopes>) {
        let projects = tempfile::tempdir().expect("projects root");
        let scopes = SessionKernelScopes::new(
            rebon_harness::kernel_bootstrap::process_kernel(),
            Arc::new(Engine::with_builtin_tools()),
            projects.path().to_path_buf(),
        );
        (projects, scopes)
    }

    /// The factory can supply task state to a session it was not minted with,
    /// and gives each session its own.
    ///
    /// This is the pair of calls `SessionRuntimeFactory::build` makes, on a
    /// real scope table. Before it, `build` refused outright when the session
    /// id was not the one the factory happened to hold, which made every move
    /// to another session fail: `/new`, `/resume`, and `rebon attach <job>`
    /// are each a build for an id the factory was not minted with. `attach`
    /// was the one that showed it -- the terminal builds its own session at
    /// startup, so by the time it asks for the background session's runtime
    /// the factory is already pinned to a different id.
    ///
    /// The danger the refusal was guarding against is real and is still
    /// guarded: two sessions must not share task state. That is asserted here
    /// as the property it actually is, rather than as "only one session may
    /// ever be built".
    #[test]
    fn a_runtime_can_be_built_for_a_session_the_factory_did_not_start_with() {
        let (_projects, scopes) = scopes();

        // The session the terminal started on.
        let first = {
            let _bound = scopes.acquire("sess-started-with");
            scopes.host_task_registry("sess-started-with")
        };

        // The one `rebon attach` moves to. Never bound before this call: the
        // acquire is what binds it, which is why the lease is held across the
        // lookup rather than taken and dropped.
        let attached = {
            let _bound = scopes.acquire("sess-attached-to");
            scopes.host_task_registry("sess-attached-to")
        };

        assert!(
            !Arc::ptr_eq(&first, &attached),
            "each session owns its task state; a shared registry is the defect \
             the old refusal was guarding against"
        );

        // And the same session asked twice is the same state, not a new one --
        // otherwise a re-entry would silently lose whatever was queued.
        let again = {
            let _bound = scopes.acquire("sess-attached-to");
            scopes.host_task_registry("sess-attached-to")
        };
        assert!(Arc::ptr_eq(&attached, &again));
    }
}
