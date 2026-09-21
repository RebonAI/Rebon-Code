//! `--acp` fast-path: run rebon as an ACP (Agent Client Protocol)
//! server over stdio or, with `--acp-port`, a raw TCP connection.
//!
//! When `--acp` is set, rebon delegates straight to the ACP server
//! instead of running through the interactive REPL bootstrap. The two
//! entries are deliberately symmetric — this module is the "I want to speak
//! JSON-RPC over stdio" entry, while the default run mode (see
//! [`crate::tui`]) is the local interactive path.
//!
//! All shared construction (model client, default model, projects
//! root, env-driven policy store, tool filter, MCP client) lives in
//! [`crate::session`] so the two entrypoints build the harness
//! identically.

use std::net::ToSocketAddrs;
use std::sync::Arc;

use anyhow::{bail, Context};
use rebon_acp::{serve_with_publishers, DefaultHandler};
use rebon_agent_core::{
    ChannelPermissionRequestPublisher, ChannelSessionUpdatePublisher, PromptExecutor,
    SessionUpdatePublisher,
};
use rebon_core::{query::EngineQueryExecutor, Engine};
use rebon_harness::{sub_agent_spawner_for_session, team_manager_for_session};
use rebon_permissions::PermissionMode;
use rebon_plugin_skill::{
    load_startup_skills, RegistrySkillCatalog, SkillContext, SkillLoaderConfig, SkillRegistry,
};
use rebon_plugin_tasks::runtime::TaskRegistryRuntimeController;
use rebon_tool::{
    AgentRegistry, ExternalSubAgentRunner, SharedToolFilter, SubAgentRuntimeHandle,
    SubAgentSpawnerRequest, TaskRuntimeController, TeamManagerRequest, WorkflowLauncherRequest,
    WorkflowTaskRuntimeHandle,
};

use crate::rebon_config::RuntimeOverride;
use crate::session_agent_router::{SessionAgentRouter, SessionAgentRouterConfig};

fn acp_config_option_applier(
    service_tier: rebon_api::ServiceTierHandle,
) -> Arc<dyn Fn(&str, &str) + Send + Sync> {
    // Only what this process has to *do* about a change. Persisting a setting
    // backed by the config file is the `config-options` seat's, registered by
    // whoever owns the setting — this used to be the third of three copies of
    // those writes, one per surface.
    Arc::new(move |config_id, value| {
        if config_id == "permissions" {
            if let Err(err) = crate::rebon_config::save_default_permission_mode_wire(value) {
                tracing::warn!(error = %err, mode = value, "failed to persist permission mode from ACP config option");
            }
        }
        if config_id == "model" {
            if let Err(err) = crate::rebon_config::persist_model_config_choice(value) {
                tracing::warn!(error = %err, model = value, "failed to persist model from ACP config option");
            }
        }
        // Live state the seat's write cannot reach: what the next request
        // sends, and which shell tools a session offers.
        if config_id == "fast_mode" {
            service_tier.set_fast(value == "on");
        }
        if config_id == "shell_tool" {
            crate::session::build::apply_shell_tool_choice(value);
        }
    })
}

use crate::session::build::system_prompt_config_for_engine;
use crate::session::mcp::build_default_mcp_client_for_cwd;
use rebon_harness::projects_root;

fn seed_acp_permission_mode(handler: &DefaultHandler, startup_override: Option<PermissionMode>) {
    if let Some(mode) = startup_override {
        handler.seed_startup_permission_mode(mode.as_wire());
    } else if let Some(mode) = crate::rebon_config::saved_default_permission_mode() {
        handler.seed_config_option_value("permissions", mode.as_wire());
    }
}

/// ACP transport selected by the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpTransport {
    /// Preserve the original ACP JSON-RPC-over-stdio behavior.
    Stdio,
    /// Listen for one raw TCP ACP connection on `host:port`.
    Tcp { host: String, port: u16 },
}

/// Run rebon as an ACP server reading JSON-RPC 2.0 from `transport`
/// (stdin/stdout for [`AcpTransport::Stdio`], one accepted connection
/// for [`AcpTransport::Tcp`]) and writing updates back on it, until the
/// client disconnects or an error occurs. `session/prompt` calls drive
/// the full agentic loop (tool use + permission reverse-RPC +
/// transcript persistence).
pub async fn run_acp_server(
    overrides: RuntimeOverride,
    transport: AcpTransport,
) -> anyhow::Result<()> {
    let AcpServerParts {
        handler,
        update_rx,
        permission_rx,
        engine,
        _cron_scheduler,
        ..
    } = build_acp_server(overrides).await?;

    match transport {
        AcpTransport::Stdio => {
            tracing::info!(
                tools = engine.tool_count(),
                "rebon: starting ACP server on stdio (real harness path)"
            );

            serve_with_publishers(
                tokio::io::stdin(),
                tokio::io::stdout(),
                handler,
                Some(update_rx),
                Some(permission_rx),
            )
            .await
        }
        AcpTransport::Tcp { host, port } => {
            let bind_addr = format!("{host}:{port}");
            let mut addrs = bind_addr
                .to_socket_addrs()
                .with_context(|| format!("failed to resolve ACP listen address {bind_addr}"))?;
            let addr = addrs
                .next()
                .with_context(|| format!("ACP listen address {bind_addr} resolved no addresses"))?;
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("failed to bind ACP listener on {bind_addr}"))?;
            let local_addr = listener.local_addr()?;
            eprintln!("rebon: ACP listening on tcp://{local_addr}");
            tracing::info!(
                tools = engine.tool_count(),
                address = %local_addr,
                "rebon: starting ACP server on raw TCP (single connection)"
            );

            let (stream, peer_addr) = listener.accept().await?;
            tracing::info!(peer = %peer_addr, "rebon: accepted ACP TCP connection");
            let (reader, writer) = stream.into_split();
            serve_with_publishers(
                reader,
                writer,
                handler,
                Some(update_rx),
                Some(permission_rx),
            )
            .await
        }
    }
}

/// An ACP server, built but not yet attached to a transport.
///
/// The handler owns method routing and the sessions; the two receivers carry
/// what a running turn publishes (`session/update` notifications and
/// `session/request_permission` reverse requests) and belong to whichever
/// transport serves the handler. The scheduler handle must stay alive as
/// long as the server does — dropping it aborts the cron task.
pub(crate) struct AcpServerParts {
    pub handler: DefaultHandler,
    pub update_rx: tokio::sync::mpsc::UnboundedReceiver<rebon_proto::types::SessionUpdateParams>,
    pub permission_rx: tokio::sync::mpsc::UnboundedReceiver<
        rebon_agent_core::publisher::OutboundPermissionRequest,
    >,
    pub engine: Arc<Engine>,
    /// The workspace this server was started in.
    pub cwd: std::path::PathBuf,
    /// The per-session agent choice, also reachable through the `agent`
    /// config option.
    pub agents: Arc<SessionAgentRouter>,
    /// The registries and runtime handles the executor was built on. `--acp`
    /// has no use for them beyond the executor; `rebon serve` reads them
    /// for the page's `/api/*` routes, which show what `/skills`, `/agents`,
    /// `/tasks`, `/mcp` and `/model` show in the terminal.
    pub skill_registry: Arc<SkillRegistry>,
    pub agent_registry: Arc<AgentRegistry>,
    pub task_registry_resolver: rebon_plugin_tasks::TaskRegistryResolver,
    pub mcp_client: Option<Arc<dyn rebon_tool::McpClient>>,
    pub mcp_warnings: Vec<String>,
    pub mcp_runtime_configs: Vec<String>,
    pub mcp_strict: bool,
    pub mcp_plugin_configs: Vec<crate::mcp_config::PluginMcpConfig>,
    pub runtime_model: rebon_core::query::SharedRuntimeModel,
    pub prune_level: rebon_api::PruneLevelHandle,
    pub _cron_scheduler: rebon_core::cron::SchedulerHandle,
}

/// Build the ACP server every transport shares: the engine, the model
/// client, the executor with its pollers and kernel wiring, the handler,
/// and the update/permission publishers. `--acp` serves it over stdio or
/// one TCP connection; `rebon serve` multiplexes it across browser tabs.
pub(crate) async fn build_acp_server(overrides: RuntimeOverride) -> anyhow::Result<AcpServerParts> {
    build_acp_server_inner(overrides, true).await
}

/// The same server for a transport that is **not** a host.
///
/// `rebon serve` translates for sessions that live in workers (RFC-0004
/// §15.4), so its handler must not take an active lock: the worker holds it,
/// and a second holder is exactly the double writer the lock exists to
/// prevent. What is left of the handler there is the metadata a page renders
/// — the config options, the slash-command menu, the session list.
pub(crate) async fn build_acp_server_without_session_ownership(
    overrides: RuntimeOverride,
) -> anyhow::Result<AcpServerParts> {
    build_acp_server_inner(overrides, false).await
}

async fn build_acp_server_inner(
    overrides: RuntimeOverride,
    take_session_ownership: bool,
) -> anyhow::Result<AcpServerParts> {
    let AcpStartupConfig {
        cwd,
        coordinator_mode,
        coordinator_mode_handle,
        coordinator_use_worktree,
    } = resolve_acp_startup_config(&overrides)?;

    let AcpAssembly {
        plugin_runtime,
        tool_filter,
        agent_registry,
        engine,
        runtime,
        declared_agents,
        acp_subagent_pool,
        acp_config_error,
    } = build_acp_assembly(&overrides, &cwd, coordinator_mode, coordinator_use_worktree).await?;

    if let Some(refusal) = rebon_plugin_host::plugin_boot::ensure_process_composition(
        &rebon_harness::kernel_bootstrap::process_plugin_registry(),
    )
    .await
    {
        tracing::warn!(detail = %refusal.message(), "kernel plugins unavailable for this ACP server");
    }
    let kernel_scopes = rebon_kernel_seats::kernel_services::SessionKernelScopes::new(
        rebon_harness::kernel_bootstrap::process_kernel(),
        engine.clone(),
        projects_root(),
    );
    let task_registry_resolver = kernel_scopes.task_registry_resolver();

    let runtime_mcp_configs = overrides.mcp_configs.clone();
    let runtime_strict_mcp_config = overrides.strict_mcp_config;
    let startup_permission_mode = overrides.permission_mode;

    let client = runtime.client;
    let model = runtime.model;
    let service_tier = runtime.service_tier.clone();
    let service_tier_enabled = service_tier.is_fast();
    let title_model = runtime.title_model;
    let runtime_model = runtime.runtime_model.clone();
    let prune_level = runtime.prune_level.clone();
    let model_profiles = runtime.model_profiles;
    let model_profiles_for_team = model_profiles.clone();
    let task_notification_poller =
        crate::task_notification_poller::TaskNotificationPoller::new_resolving(
            task_registry_resolver.clone(),
        );
    // `_session/steering` mailbox: the handler enqueues into it from the
    // serve loop's bypass worker; the executor's extra attachment poller
    // (composed below) injects the queued messages into the session's
    // running turn between tool rounds.
    let steering_poller = crate::steering_poller::AcpSteeringPoller::new();
    let workflow_launcher =
        crate::session::runtime::workflow_launcher_for_session(WorkflowLauncherRequest {
            task_runtime: WorkflowTaskRuntimeHandle::new(task_registry_resolver.clone()),
            cwd: cwd.clone(),
            config_home_dir: crate::rebon_config::config_home_dir(),
            session_root: projects_root(),
            model_profiles: model_profiles_for_team.clone(),
            active_provider: Some(runtime.provider_name.clone()),
        });
    let task_runtime_controller: Arc<dyn TaskRuntimeController> = Arc::new(
        TaskRegistryRuntimeController::new(task_registry_resolver.clone()),
    );
    let sub_agent_model_config = crate::rebon_config::saved_sub_agent_model_config();
    let sub_agent_model_router = crate::session::build::model_router_for_runtime(
        runtime_model.clone(),
        service_tier.clone(),
        sub_agent_model_config.clone(),
    );
    let subagent_filter = rebon_core::coordinator_mode::default_subagent_filter(coordinator_mode);
    let team_manager = team_manager_for_session(TeamManagerRequest {
        engine: SubAgentRuntimeHandle::new(engine.clone()),
        task_runtime: SubAgentRuntimeHandle::new(task_registry_resolver.clone()),
        client: client.clone(),
        default_model: model.clone(),
        model_config: sub_agent_model_config.clone(),
        model_profiles: model_profiles_for_team,
        model_router: sub_agent_model_router.clone(),
        base_filter: Some(SharedToolFilter::new(subagent_filter.clone())),
    });

    // 3. Build the extension injections. Every sub-agent receives the
    //    mode-aware base filter: normal agents cannot discover queue/CEO-only
    //    tools, while coordinator workers receive the async-agent allow list.
    //    The filter is intersected with any per-spec `allowed_tools` the model
    //    supplies.
    if coordinator_mode {
        tracing::info!(
            "rebon: coordinator mode active — applying async_agent_filter to sub-agents"
        );
    }
    let spawner = sub_agent_spawner_for_session(SubAgentSpawnerRequest {
        engine: SubAgentRuntimeHandle::new(Arc::downgrade(&engine)),
        task_runtime: Some(SubAgentRuntimeHandle::new(task_registry_resolver.clone())),
        client: client.clone(),
        default_model: model.clone(),
        model_config: sub_agent_model_config,
        model_profiles,
        model_router: sub_agent_model_router,
        base_filter: Some(SharedToolFilter::new(subagent_filter)),
        // The ACP surface wires no hooks of its own, so a worker spawned
        // from it inherits none — unchanged by the policy seat.
        policy: None,
        coordinator_mode: Some(coordinator_mode_handle.clone()),
        coordinator_use_worktree,
        file_history_tracker: None,
        external_runner: acp_subagent_pool
            .clone()
            .map(|pool| pool as Arc<dyn ExternalSubAgentRunner>),
    });

    // MCP runtime: spawn the servers `config.json#mcpServers`,
    // REBON_MCP_SERVERS_JSON and an approved project `.mcp.json` name.
    let mcp_build = build_default_mcp_client_for_cwd(
        &cwd,
        Vec::new(),
        &runtime_mcp_configs,
        runtime_strict_mcp_config,
        &plugin_runtime.mcp_configs,
    )
    .await?;
    for warning in &mcp_build.warnings {
        tracing::warn!("rebon: {warning}");
    }
    let mcp_warnings = mcp_build.warnings.clone();
    let mcp_plugin_configs = plugin_runtime.mcp_configs.clone();
    let mcp_client = mcp_build.client;
    let mcp_client_for_parts = mcp_client.clone();
    let skill_registry = Arc::new(SkillRegistry::new());
    match crate::rebon_config::load_disabled_skills() {
        Ok(disabled_skills) => skill_registry.set_disabled_skills(disabled_skills),
        Err(err) => {
            tracing::warn!(error = %err, "rebon ACP startup: failed to load disabled skills");
        }
    }

    // 3b. Apply the persisted sub-agent toggle before any tool
    //     snapshot is taken. See `tui/runner/dialog_keys.rs` for the TUI-side
    //     companion.
    let persisted_sub_agents = crate::rebon_config::saved_sub_agents_enabled();
    rebon_tool::set_sub_agents_enabled(persisted_sub_agents);
    let persisted_shell_tool = crate::session::build::apply_persisted_shell_tool();
    let persisted_claude_codex_fallback =
        crate::rebon_config::saved_claude_codex_fallback_enabled();

    let AcpHandlerParts {
        handler,
        server_state,
        system_prompt_config,
    } = build_acp_handler(
        &engine,
        &tool_filter,
        &model,
        &service_tier,
        startup_permission_mode,
        persisted_sub_agents,
        persisted_shell_tool,
        persisted_claude_codex_fallback,
        service_tier_enabled,
        take_session_ownership,
    );
    let (handler, policy_store_resolver) = rebon_harness::with_acp_session_policies(handler);
    // 5. Build the prompt executor.
    //
    // Cron poller wired through the same `with_cron_poller` seam used by
    // the TUI path — the scheduler spun up below pushes fired prompts
    // into this poller, and `execute()` merges them with the session-
    // state attachment poller via `CompositePoller`.
    let cron_poller = rebon_core::cron::CronPoller::new();
    let session_cron_store = rebon_tool::SessionCronStore::new();
    let mut executor =
        EngineQueryExecutor::new(engine.clone(), client, projects_root(), model.clone())
            .with_system_prompt_config(system_prompt_config)
            .with_shared_runtime_model(runtime_model.clone())
            .with_turn_skill_catalog(Arc::new(RegistrySkillCatalog::new(skill_registry.clone())))
            .with_extension(SkillContext::with_registry(skill_registry.clone()))
            .with_task_runtime_controller(task_runtime_controller)
            .with_policy_store_resolver(policy_store_resolver)
            .with_policy_resolver(acp_session_policy_resolver(plugin_runtime.hooks.clone()))
            .with_tool_filter(tool_filter)
            .with_shared_coordinator_mode(coordinator_mode_handle)
            .with_coordinator_use_worktree(coordinator_use_worktree)
            .with_server_state(server_state.clone())
            .with_title_model(title_model.clone())
            .with_cron_poller(cron_poller.clone())
            // The executor exposes a single extra-poller slot; compose the
            // task-notification poller with the steering mailbox rather
            // than widening the seam. Order puts task notifications ahead
            // of steered user messages within one injection round.
            .with_extra_attachment_poller(Arc::new(rebon_core::cron::CompositePoller::new(
                task_notification_poller,
                steering_poller.clone(),
            )))
            .with_session_cron_store(session_cron_store.clone());
    if let Some(spawner) = spawner {
        executor = executor.with_sub_agent_spawner(spawner);
    }
    if let Some(team_manager) = team_manager {
        executor = executor.with_team_manager(team_manager);
    }
    if let Some(workflow_launcher) = workflow_launcher {
        executor = executor.with_workflow_launcher(workflow_launcher);
    }
    if let Some(mcp) = mcp_client {
        executor = executor.with_mcp_client(mcp);
    }

    let executor = attach_acp_executor_extras(executor, &cwd, &kernel_scopes).await?;

    let (executor, cron_scheduler) = start_acp_cron_and_skills(
        executor,
        &cwd,
        cron_poller,
        session_cron_store,
        skill_registry.clone(),
        &plugin_runtime,
        persisted_claude_codex_fallback,
    )
    .await;

    // 6. Build the update + permission publishers.
    let (update_publisher, update_rx) = ChannelSessionUpdatePublisher::new();
    let (permission_publisher, permission_rx) = ChannelPermissionRequestPublisher::new();

    // 7. Wire the executor + publishers into the handler.
    //
    // Turns go through a per-session agent router rather than straight to
    // the engine executor, so a session can be pointed at a kernel loop on
    // the plugin plane or a third-party agent CLI — the `agent` config
    // option is `/agent` for surfaces that have no prompt line — without the
    // handler learning what an agent is.
    let update_publisher_arc: Arc<dyn SessionUpdatePublisher> = Arc::new(update_publisher);
    let engine_executor: Arc<dyn PromptExecutor> = Arc::new(executor);
    let local_backend: Arc<dyn rebon_agent_core::AgentBackend> =
        Arc::new(rebon_agent_core::LocalAgentBackend::new(engine_executor));
    // Mirror every journalled row into the in-memory transcript projection,
    // so switching a session back to the local engine replays the turns an
    // external agent or kernel loop wrote (matches the TUI wiring).
    let transcript_sink: rebon_acp_client::TranscriptSink = {
        let state = server_state.clone();
        Arc::new(
            move |session_id: &str, entry: rebon_session::TranscriptEntry| {
                if !state.push_transcript_entries(session_id, vec![entry]) {
                    tracing::debug!(
                        session_id,
                        "rebon: no live session record for a journalled ACP row"
                    );
                }
            },
        )
    };
    let agents = Arc::new(SessionAgentRouter::new(SessionAgentRouterConfig {
        local: local_backend,
        declared: declared_agents,
        config_error: acp_config_error,
        projects_root: projects_root(),
        config_dir: crate::rebon_config::config_home_dir(),
        transcript_sink: Some(transcript_sink),
        kernel_spawner: crate::session::build::kernel_loop_spawner(),
    }));
    let handler = handler
        .with_prompt_executor(agents.clone() as Arc<dyn PromptExecutor>)
        .with_session_config_options(session_config_options(agents.clone(), server_state.clone()))
        .with_update_publisher(update_publisher_arc)
        .with_permission_publisher(permission_publisher)
        // Enables `_session/steering` and its `_meta.steering.supported`
        // advertisement in the initialize response.
        .with_steering_sink(steering_poller);

    Ok(AcpServerParts {
        handler,
        update_rx,
        permission_rx,
        engine,
        cwd,
        agents,
        skill_registry,
        agent_registry,
        task_registry_resolver,
        mcp_client: mcp_client_for_parts,
        mcp_warnings,
        mcp_runtime_configs: runtime_mcp_configs,
        mcp_strict: runtime_strict_mcp_config,
        mcp_plugin_configs,
        runtime_model,
        prune_level,
        _cron_scheduler: cron_scheduler,
    })
}

/// The config option id a client sets to switch a session's agent.
pub(crate) const AGENT_CONFIG_OPTION: &str = "agent";

/// The config option id a client sets to choose a session's reasoning
/// effort — `/effort` for surfaces that have no prompt line.
pub(crate) const EFFORT_CONFIG_OPTION: &str = "effort";

/// The per-session config options: the `agent` a session's turns run on,
/// and the reasoning `effort` they run with.
///
/// Per session, because the router keeps one switch (and one effort) per
/// session id; the agent list is the router's, so a value the client is
/// shown is a value a switch will accept, and a switch the router refuses
/// (an agent that could not be reached, a loop that is not configured)
/// reaches the client as invalid params carrying the router's reason. The
/// effort values are the TUI's `/effort` levels plus `auto` for the
/// provider default, and a choice is persisted the way `/effort` persists
/// it, so the next TUI session sees the same level.
fn session_config_options(
    agents: Arc<SessionAgentRouter>,
    state: Arc<rebon_acp::ServerState>,
) -> rebon_acp::SessionConfigOptions {
    use rebon_proto::types::{ConfigOption, ConfigOptionType, ConfigOptionValue};

    let options = vec![
        ConfigOption {
            id: AGENT_CONFIG_OPTION.to_string(),
            name: "Agent".to_string(),
            description: Some(
                "Which agent runs this session's turns: Rebon's own engine, a kernel loop on the \
                 plugin plane, or a third-party agent CLI"
                    .to_string(),
            ),
            category: Some("agent".to_string()),
            option_type: ConfigOptionType::Select,
            current_value: rebon_config::LOCAL_AGENT_ID.to_string(),
            options: agents
                .option_values()
                .into_iter()
                .map(|(value, name)| ConfigOptionValue {
                    value,
                    name,
                    description: None,
                })
                .collect(),
        },
        ConfigOption {
            id: EFFORT_CONFIG_OPTION.to_string(),
            name: "Effort".to_string(),
            description: Some(
                "Reasoning effort for this session's turns; `auto` leaves it to the provider"
                    .to_string(),
            ),
            category: Some("model".to_string()),
            option_type: ConfigOptionType::Select,
            current_value: agents.default_effort(),
            options: crate::session_agent_router::EFFORT_LEVELS
                .iter()
                .map(|(value, name)| ConfigOptionValue {
                    value: (*value).to_string(),
                    name: (*name).to_string(),
                    description: None,
                })
                .collect(),
        },
    ];
    let apply_agents = agents.clone();
    rebon_acp::SessionConfigOptions {
        options,
        apply: Arc::new(move |session_id, config_id, value| {
            match config_id {
                AGENT_CONFIG_OPTION => {
                    let cwd = state
                        .get_session(session_id)
                        .map(|session| session.cwd)
                        .ok_or_else(|| format!("unknown session {session_id}"))?;
                    let receipt = apply_agents.switch(session_id, &cwd, value)?;
                    tracing::info!(session_id, agent = value, %receipt, "rebon: session agent switched");
                }
                EFFORT_CONFIG_OPTION => {
                    let cwd = state
                        .get_session(session_id)
                        .map(|session| session.cwd)
                        .ok_or_else(|| format!("unknown session {session_id}"))?;
                    apply_agents.set_effort(session_id, &cwd, value)?;
                    let persisted = (value != "auto").then_some(value);
                    if let Err(err) = crate::rebon_config::save_effort_level(persisted) {
                        tracing::warn!(error = %err, effort = value, "failed to persist effort from the session config option");
                    }
                    tracing::info!(session_id, effort = value, "rebon: session effort set");
                }
                other => return Err(format!("unknown session config option {other}")),
            }
            Ok(())
        }),
        current: Arc::new(move |session_id| {
            vec![
                (AGENT_CONFIG_OPTION.to_string(), agents.current(session_id)),
                (EFFORT_CONFIG_OPTION.to_string(), agents.effort(session_id)),
            ]
        }),
    }
}

/// Start the cron scheduler and load the startup skills.
///
/// The returned scheduler handle must stay alive: dropping it aborts the
/// tokio task and releases the cross-process owner lock, so the caller keeps
/// the binding for the lifetime of the ACP process.
async fn start_acp_cron_and_skills(
    mut executor: EngineQueryExecutor,
    cwd: &std::path::Path,
    cron_poller: Arc<rebon_core::cron::CronPoller>,
    session_cron_store: Arc<rebon_tool::SessionCronStore>,
    skill_registry: Arc<SkillRegistry>,
    plugin_runtime: &crate::plugin::runtime::PluginRuntimeContributions,
    persisted_claude_codex_fallback: bool,
) -> (EngineQueryExecutor, rebon_core::cron::SchedulerHandle) {
    // Start the cron scheduler rooted at the ACP session cwd. The
    // returned handle must stay alive — drop it and the tokio task is
    // aborted and the cross-process owner lock released. We let it
    // live for the lifetime of the ACP process by keeping the binding
    // in scope all the way through `serve_with_publishers`.
    let cron_scheduler = {
        let config = rebon_core::cron::SchedulerConfig::new(cwd.to_path_buf());
        rebon_core::cron::start_scheduler_with_session_store(
            config,
            cron_poller.clone(),
            Some(session_cron_store.clone()),
        )
    };

    // 5b. Progressive skill discovery: load startup skills.
    {
        let cwd = cwd.to_string_lossy().into_owned();
        let config_home = crate::rebon_config::config_home_dir()
            .to_string_lossy()
            .into_owned();
        let skill_state = load_startup_skills(
            &SkillLoaderConfig {
                config_home,
                cwd,
                session_id: String::new(),
                claude_codex_fallback_enabled: persisted_claude_codex_fallback,
                plugin_skill_dirs: plugin_runtime
                    .skill_dirs
                    .iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect(),
                plugin_command_dirs: plugin_runtime
                    .command_dirs
                    .iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect(),
                skill_bundles: rebon_harness::kernel_bootstrap::skill_bundles(),
            },
            &skill_registry,
        )
        .await;
        // Upgrade the registry-only value attached at build time: from here
        // the same bag carries the discovery state the turn subscriber reads.
        executor = executor.with_extension(SkillContext::new(skill_registry.clone(), skill_state));
    }
    (executor, cron_scheduler)
}

/// Attach the sandbox policy and the kernel wiring.
///
/// These are built here rather than in `wiring.rs` because an editor-driven
/// session runs the same tools against the same workspace: a `sandbox` block
/// that only bound in the TUI would be a setting the user could not tell was
/// off. Kernel scopes stay per session and generation -- a shared scope would
/// let one session's plugin middleware answer another's permission request.
async fn attach_acp_executor_extras(
    mut executor: EngineQueryExecutor,
    cwd: &std::path::Path,
    scopes: &Arc<rebon_kernel_seats::kernel_services::SessionKernelScopes>,
) -> anyhow::Result<EngineQueryExecutor> {
    // Sandbox, on the same terms as the TUI. This executor is built
    // here rather than in `wiring.rs`, so the policy has to be
    // attached here too — an editor-driven session runs the same
    // `Bash` and `PowerShell` tools against the same workspace, and a
    // `sandbox` block that only bound in the TUI would be a setting
    // the user could not tell was off.
    //
    // In `--acp` mode tracing goes to stderr, which the editor
    // surfaces, so a bad configuration is visible without a TUI to
    // show it in.
    if let Some(sandbox) = rebon_harness::resolve_command_sandbox(cwd) {
        executor = executor.with_command_sandbox(sandbox);
    }

    // Kernel wiring, matching the TUI and headless paths: the configured
    // composition boots (idempotent, and a no-op when nothing is
    // configured — it also installs the process-wide system-prompt
    // sections and web seat), and this server's turns get the plugin tool
    // seat plus the `permission/ask` waterfall. Without this the same
    // `kernelPlugins` config behaves differently depending on whether a
    // session was opened here or in the TUI.
    //
    // Scopes are per session and generation. The resolver acquires exactly
    // once for each ACP prompt and its broker retains that lease for the
    // whole turn. A shared scope would let one session's plugin middleware
    // answer another's permission request and would serve the wrong
    // transcript through the `session` seat.
    {
        executor = executor.with_kernel_context_resolver(scopes.resolver());
        // The plugin tool registry is the scopes' own — every session here
        // registers that same instance, so the executor dispatches from the
        // table the sessions actually write to.
        {
            executor = executor
                .with_plugin_tools(scopes.plugin_tools() as Arc<dyn rebon_tool::PluginToolProvider>)
                .with_web_provider_router(
                    rebon_kernel_seats::kernel_web_seat::KernelWebRouter::shared(),
                );
        }
    }
    Ok(executor)
}

/// What the handler brings with it once the persisted settings are seeded.
struct AcpHandlerParts {
    handler: DefaultHandler,
    server_state: Arc<rebon_acp::ServerState>,
    system_prompt_config: rebon_core::system_prompt::SystemPromptConfig,
}

/// Build the ACP handler and seed it from the persisted settings.
///
/// The handler comes first so the executor can be built against its shared
/// `ServerState`: that is what lets `session/prompt` replay a previously
/// loaded transcript into the model's history.
///
/// `--acp` opens a session in this process, so it takes the session's active
/// lock and refuses what someone else holds -- without that a browser could
/// run turns on the session a terminal was writing and the two transcripts
/// would diverge on disk. `rebon serve` is a client of a worker instead and
/// asks for this to be left off.
#[allow(clippy::too_many_arguments)]
fn build_acp_handler(
    engine: &Arc<Engine>,
    tool_filter: &rebon_tool::ToolFilter,
    model: &str,
    service_tier: &rebon_api::ServiceTierHandle,
    startup_permission_mode: Option<PermissionMode>,
    persisted_sub_agents: bool,
    persisted_shell_tool: rebon_tool::ShellToolPreference,
    persisted_claude_codex_fallback: bool,
    service_tier_enabled: bool,
    take_session_ownership: bool,
) -> AcpHandlerParts {
    // 4. Build the ACP handler first so we can borrow its shared
    //    ServerState, then build the executor with that state so
    //    `session/prompt` can replay previously-loaded transcripts
    //    into the model's history via [`EngineQueryExecutor::with_server_state`].
    let handler = DefaultHandler::default()
        .with_config_option_applier(acp_config_option_applier(service_tier.clone()))
        // `/memory` asks the `loaded-documents` seam what this session
        // loaded; without a scope it would report nothing.
        .with_kernel_scope(
            rebon_harness::kernel_bootstrap::process_kernel()
                .context()
                .clone(),
        );
    // `--acp` opens a session in this process, so it takes the session's
    // active lock and refuses what someone else holds — without that a
    // browser could run turns on the session a terminal was writing and the
    // two transcripts diverged on disk. `rebon serve` is a client of a
    // worker instead and asks for this to be left off.
    let handler = if take_session_ownership {
        handler.with_session_ownership()
    } else {
        handler
    };
    handler.seed_config_option_value(
        "sub_agents",
        if persisted_sub_agents { "on" } else { "off" },
    );
    handler.seed_config_option_value("shell_tool", persisted_shell_tool.as_wire());
    handler.seed_config_option_value(
        "claude_codex_fallback",
        if persisted_claude_codex_fallback {
            "on"
        } else {
            "off"
        },
    );
    handler.seed_config_option_value("fast_mode", if service_tier_enabled { "on" } else { "off" });
    seed_acp_permission_mode(&handler, startup_permission_mode);
    handler.seed_config_option_value("model", model);
    let update_auto_install = crate::rebon_config::load_update_preferences()
        .map(|prefs| prefs.auto_install)
        .unwrap_or(false);
    handler.seed_config_option_value(
        "update_auto_install",
        if update_auto_install { "on" } else { "off" },
    );
    let server_state = handler.state().clone();
    let system_prompt_config = system_prompt_config_for_engine(engine, tool_filter, model, false);
    AcpHandlerParts {
        handler,
        server_state,
        system_prompt_config,
    }
}

/// Everything the assembly and the agent list settle for this server.
struct AcpAssembly {
    plugin_runtime: crate::plugin::runtime::PluginRuntimeContributions,
    tool_filter: rebon_tool::ToolFilter,
    agent_registry: Arc<rebon_tool::AgentRegistry>,
    engine: Arc<Engine>,
    runtime: rebon_harness::RuntimeModel,
    declared_agents: Vec<rebon_agent_core::routing::DeclaredAgent>,
    acp_subagent_pool: Option<Arc<crate::acp_subagent_pool::AcpSubAgentPool>>,
    acp_config_error: Option<String>,
}

/// Run the shared session assembly for this server, then read the declared
/// agents off the registry it produced.
async fn build_acp_assembly(
    overrides: &RuntimeOverride,
    cwd: &std::path::Path,
    coordinator_mode: bool,
    coordinator_use_worktree: bool,
) -> anyhow::Result<AcpAssembly> {
    // Build the merged agent registry from built-ins + `~/.rebon/agents/` +
    // `<cwd>/.rebon/agents/` plus startup plugin agents. The plugin runtime is
    // this binary's own -- it reads `--plugin-dirs` and spawns the processes --
    // so it is resolved here and handed to the assembly.
    let plugin_runtime =
        crate::plugin::resolve_runtime_contributions(&crate::plugin::PluginRuntimeOptions {
            cwd: cwd.to_path_buf(),
            config_home: crate::rebon_config::config_home_dir(),
            plugin_dirs: overrides
                .plugin_dirs
                .iter()
                .map(std::path::PathBuf::from)
                .collect(),
            rebon_exe: std::env::current_exe().ok(),
        })?;
    for warning in &plugin_runtime.warnings {
        tracing::warn!("rebon: plugin runtime warning: {warning}");
    }

    // Tool filter, agent registry, engine and model client share harness assembly.
    // No policy is built at this server's startup cwd: with_acp_session_policies
    // loads each record's actual cwd and wraps the per-turn reverse-RPC broker
    // with that session's live rules store.
    // `fix_tools()` publishes the registry before the engine lists its tools;
    // otherwise the first turn describes the compiled-in built-ins only.
    //
    // External agents are spawnable because this wiring also builds the
    // sub-agent pool below whenever any are declared.
    // The fourth value is the proof that a session may be bound against this
    // runtime. This server binds none: it serves many sessions, and its kernel
    // scopes are opened once for the server rather than once per session.
    let (assembly, engine, runtime, _runtime_resolved) =
        rebon_harness::session_assembly::SessionAssembly::begin(
            rebon_harness::HarnessOverrides {
                provider: overrides.provider.clone(),
                model: overrides.model.clone(),
                cwd: Some(cwd.to_string_lossy().into_owned()),
                fast_mode: overrides.fast_mode,
                coordinator_mode,
                plugin_model_providers: plugin_runtime.model_providers.clone(),
                ..Default::default()
            },
            rebon_harness::session_assembly::AssemblyInputs {
                policy_loading:
                    rebon_harness::session_assembly::PolicyLoading::AcpSessionActivation,
                queue_session: false,
                agent_dirs: &plugin_runtime.agent_dirs,
                external_agents_spawnable: true,
                coordinator_use_worktree,
            },
        )?
        .fix_tools()
        .resolve_runtime()
        .await?
        .into_parts();
    let rebon_harness::session_assembly::SessionAssembly {
        tool_filter,
        agent_registry,
        ..
    } = assembly;

    let AcpAgentSetup {
        declared_agents,
        acp_subagent_pool,
        acp_config_error,
    } = build_acp_declared_agents(cwd, &plugin_runtime, &agent_registry);
    Ok(AcpAssembly {
        plugin_runtime,
        tool_filter,
        agent_registry,
        engine,
        runtime,
        declared_agents,
        acp_subagent_pool,
        acp_config_error,
    })
}

/// Per-session lookup of the policy-event handle this server's turns raise
/// events on.
///
/// A resolver rather than one handle because this server has many sessions
/// and one executor: the subscribers are the same for all of them — the
/// process seat, the user's `settings.json` hooks, the plugin hooks resolved
/// once at startup — but the handle also says *whose* session it is, and two
/// sessions sharing one would answer each other's events.
///
/// Rebuilt per turn on purpose. A plugin that subscribes after the server
/// started is on the process seat by then, and re-reading the settings is
/// what `SettingsHookSubscriber` does on every event anyway.
fn acp_session_policy_resolver(
    plugin_hooks: Vec<rebon_hooks::IndividualHookConfig>,
) -> rebon_core::policy_seat::PolicySourcesResolver {
    Arc::new(move |session_id: &str, cwd: &str| {
        rebon_harness::session_assembly::policy_sources_for_session(
            session_id,
            cwd,
            plugin_hooks.clone(),
        )
    })
}

/// The resident backends a declared-agent list implies.
struct AcpAgentSetup {
    declared_agents: Vec<rebon_agent_core::routing::DeclaredAgent>,
    acp_subagent_pool: Option<Arc<crate::acp_subagent_pool::AcpSubAgentPool>>,
    acp_config_error: Option<String>,
}

/// The third-party agents this server can route a session to.
///
/// The config error is carried rather than just logged: the config layer
/// refuses a malformed list wholesale, and an `agent` option list quietly
/// missing the agent the user just configured would be the wrong kind of
/// quiet. The router repeats the error where the list is shown.
fn build_acp_declared_agents(
    cwd: &std::path::Path,
    plugin_runtime: &crate::plugin::runtime::PluginRuntimeContributions,
    agent_registry: &Arc<AgentRegistry>,
) -> AcpAgentSetup {
    // Resident backends for `<agentId>:<model>` sub-agent specs — the
    // same pool the TUI wires, minus the file-history tracker (this
    // entrypoint has no session-scoped tracker, so routed writes go
    // direct and are honestly not rewind-covered).
    // The config error is carried, not just logged: the config layer refuses
    // a malformed list wholesale, and an `agent` option list that was quietly
    // missing the agent the user just configured would be the wrong kind of
    // quiet. The router repeats the error where the list is shown.
    let (configured_acp_agents, acp_config_error) = match crate::rebon_config::load_acp_agents() {
        Ok(configured) => (configured, None),
        Err(err) => {
            tracing::warn!(error = %err, "rebon: ignoring an unusable `acpAgents` config");
            (Vec::new(), Some(err.to_string()))
        }
    };
    let declared_agents = rebon_harness::agent_assembly::declared_agents(
        &configured_acp_agents,
        &plugin_runtime.acp_agents,
        &agent_registry,
    );
    let acp_subagent_pool = (!declared_agents.is_empty()).then(|| {
        crate::acp_subagent_pool::pool_from_declared(
            declared_agents.clone(),
            cwd.to_string_lossy().into_owned(),
            Vec::new(),
            None,
        )
    });

    AcpAgentSetup {
        declared_agents,
        acp_subagent_pool,
        acp_config_error,
    }
}

/// What the process-level settings say before any session exists.
struct AcpStartupConfig {
    cwd: std::path::PathBuf,
    coordinator_mode: bool,
    coordinator_mode_handle: rebon_tool::SharedCoordinatorMode,
    coordinator_use_worktree: bool,
}

/// Read the environment and the saved config into one startup decision.
/// Permission rules are loaded separately at each session's activation.
fn resolve_acp_startup_config(overrides: &RuntimeOverride) -> anyhow::Result<AcpStartupConfig> {
    if !overrides.development_channels.is_empty() {
        bail!(
        "--dangerously-load-development-channels requires the interactive TUI warning dialog and cannot be used with --acp"
    );
    }

    let cwd = std::env::current_dir()?;
    if !crate::rebon_config::is_directory_trusted(&cwd) {
        bail!(
        "workspace not trusted: {}. Run `rebon` in that directory first to accept the trust dialog.",
        cwd.display()
    );
    }

    // Tool filtering belongs to assembly; policy loading belongs to each session.
    let coordinator_mode = rebon_core::coordinator_mode::coordinator_mode_from_env_default();
    let coordinator_mode_handle = rebon_tool::SharedCoordinatorMode::new(coordinator_mode);
    let coordinator_use_worktree = crate::rebon_config::saved_coordinator_use_worktree();
    Ok(AcpStartupConfig {
        cwd,
        coordinator_mode,
        coordinator_mode_handle,
        coordinator_use_worktree,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    struct AcpGlobalStateGuard {
        previous_config_dir: Option<std::ffi::OsString>,
        previous_sub_agents_enabled: bool,
        _env_lock: std::sync::MutexGuard<'static, ()>,
    }

    impl AcpGlobalStateGuard {
        fn with_config_dir(config_dir: &std::path::Path) -> Self {
            let _env_lock = crate::test_env::lock_env();
            let previous_config_dir = std::env::var_os("REBON_CONFIG_DIR");
            let previous_sub_agents_enabled = rebon_tool::sub_agents_enabled();
            std::env::set_var("REBON_CONFIG_DIR", config_dir);
            Self {
                previous_config_dir,
                previous_sub_agents_enabled,
                _env_lock,
            }
        }
    }

    impl Drop for AcpGlobalStateGuard {
        fn drop(&mut self) {
            if let Some(value) = self.previous_config_dir.take() {
                std::env::set_var("REBON_CONFIG_DIR", value);
            } else {
                std::env::remove_var("REBON_CONFIG_DIR");
            }
            rebon_tool::set_sub_agents_enabled(self.previous_sub_agents_enabled);
        }
    }

    async fn drain(mut reader: DuplexStream) -> Vec<u8> {
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();
        out
    }

    fn ndjson_lines(bytes: &[u8]) -> Vec<&[u8]> {
        bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .collect()
    }

    fn permission_option_value(handler: &DefaultHandler) -> String {
        handler
            .config_options_for_session("missing-session")
            .into_iter()
            .find(|option| option.id == "permissions")
            .unwrap()
            .current_value
    }

    #[test]
    fn acp_startup_permission_override_beats_saved_default_without_persisting() {
        let tmp = TempDir::new().unwrap();
        let _guard = AcpGlobalStateGuard::with_config_dir(tmp.path());
        crate::rebon_config::save_default_permission_mode_in_dir(tmp.path(), PermissionMode::Auto)
            .unwrap();

        let overridden = DefaultHandler::default();
        seed_acp_permission_mode(&overridden, Some(PermissionMode::BypassPermissions));
        assert_eq!(permission_option_value(&overridden), "bypassPermissions");
        assert_eq!(
            crate::rebon_config::saved_default_permission_mode(),
            Some(PermissionMode::Auto)
        );

        let persisted = DefaultHandler::default();
        seed_acp_permission_mode(&persisted, None);
        assert_eq!(permission_option_value(&persisted), "auto");
    }

    async fn serve_config_option_value(value: &str) -> Vec<u8> {
        let handler = DefaultHandler::default()
            .with_config_option_applier(acp_config_option_applier(Default::default()));

        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, client_out) = tokio::io::duplex(4096);
        let input = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1,\"clientCapabilities\":{{}}}}}}\n{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/set_config_option\",\"params\":{{\"sessionId\":\"sess-missing\",\"configId\":\"sub_agents\",\"value\":\"{value}\"}}}}\n"
        );
        client_in.write_all(input.as_bytes()).await.unwrap();
        drop(client_in);

        let serve_task = tokio::spawn(async move {
            rebon_acp::serve(server_in, server_out, handler)
                .await
                .unwrap();
        });
        let out = drain(client_out).await;
        serve_task.await.unwrap();
        out
    }

    #[tokio::test]
    async fn acp_session_set_config_option_sub_agents_updates_runtime_switch() {
        let tmp = TempDir::new().unwrap();
        let _guard = AcpGlobalStateGuard::with_config_dir(tmp.path());

        rebon_tool::set_sub_agents_enabled(true);
        let out = serve_config_option_value("off").await;

        let lines = ndjson_lines(&out);
        assert_eq!(lines.len(), 2);
        let response: rebon_proto::JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        assert!(response.error.is_none());
        assert!(!rebon_tool::sub_agents_enabled());
        assert!(!crate::rebon_config::saved_sub_agents_enabled());

        let out = serve_config_option_value("on").await;
        let lines = ndjson_lines(&out);
        assert_eq!(lines.len(), 2);
        let response: rebon_proto::JsonRpcResponse = serde_json::from_slice(lines[1]).unwrap();
        assert!(response.error.is_none());
        assert!(rebon_tool::sub_agents_enabled());
        assert!(crate::rebon_config::saved_sub_agents_enabled());
    }

    /// A refusing `PreToolUse` hook, written for whichever shell the test
    /// host has. Exit code 2 is the blocking one and its stderr is the
    /// reason the person reads back.
    fn refusing_hook_settings() -> String {
        let hook = if cfg!(windows) {
            serde_json::json!({
                "type": "command",
                "shell": "powershell",
                "command": "[Console]::Error.WriteLine('refused by the configured hook'); exit 2"
            })
        } else {
            serde_json::json!({
                "type": "command",
                "shell": "bash",
                "command": "echo 'refused by the configured hook' >&2; exit 2"
            })
        };
        serde_json::json!({
            "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [hook] }] }
        })
        .to_string()
    }

    /// The handle this server hands the executor for one session runs that
    /// person's hooks, and carries that session rather than another's.
    ///
    /// `--acp` and the `serve` page behind it ran no hooks at all until the
    /// executor grew a per-session policy seam: one executor serves every
    /// session here, and one fixed handle would have answered for all of
    /// them.
    #[tokio::test(flavor = "multi_thread")]
    async fn acp_session_policy_handles_run_the_configured_pre_tool_use_hook() {
        let tmp = TempDir::new().unwrap();
        let _guard = AcpGlobalStateGuard::with_config_dir(tmp.path());
        std::fs::write(tmp.path().join("settings.json"), refusing_hook_settings()).unwrap();

        let project = TempDir::new().unwrap();
        let cwd = project.path().to_string_lossy().into_owned();
        let resolve = acp_session_policy_resolver(Vec::new());

        let first = resolve("session-one", &cwd);
        let second = resolve("session-two", &cwd);
        assert_eq!(first.context().session_id, "session-one");
        assert_eq!(second.context().session_id, "session-two");
        assert_eq!(first.context().cwd, cwd);
        assert_ne!(
            first.context().transcript_path,
            second.context().transcript_path,
            "two sessions must not share one transcript path"
        );
        assert!(first
            .subscriber_ids()
            .iter()
            .any(|id| id == rebon_core::policy_seat::SETTINGS_HOOKS_SUBSCRIBER_ID));

        let decision = rebon_core::hooks::run_pre_tool_use_hooks(
            &first,
            "Bash",
            serde_json::json!({ "command": "rm -rf /" }),
            "tool-use-1",
        )
        .await;
        match decision {
            rebon_core::hooks::PreToolUseDecision::Blocked { reason } => assert!(
                reason.contains("refused by the configured hook"),
                "the refusal lost the hook's own reason: {reason}"
            ),
            other => panic!("the configured PreToolUse hook did not refuse Bash: {other:?}"),
        }

        // A tool the matcher does not name is left alone, so what is wired
        // is a hook runtime and not a blanket refusal.
        assert!(matches!(
            rebon_core::hooks::run_pre_tool_use_hooks(
                &second,
                "Read",
                serde_json::json!({ "file_path": "x" }),
                "tool-use-2",
            )
            .await,
            rebon_core::hooks::PreToolUseDecision::Continue { .. }
        ));
    }
}
