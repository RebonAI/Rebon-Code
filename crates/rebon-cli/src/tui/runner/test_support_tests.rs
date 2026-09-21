use std::sync::{Arc, Mutex, RwLock};

use rebon_core::query::{RuntimeModelConfig, SharedRuntimeModel};

use crate::session::mcp::TuiMcpLoadStatus;
use crate::session::runtime::SessionRuntime;
use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::UiMode;

use rebon_agent_core::file_history::submit_tracker;

/// A spawner that keeps the two promises callers actually depend on.
///
/// The real spawners hand back the id the caller put in `metadata.agent_id`
/// and register a task under it. A stand-in that returns a fixed string does
/// neither: the caller's own id and the spawned one disagree — which
/// `task_runtime` asserts against in debug builds — and the task it goes
/// looking for afterwards was never registered.
struct TestSubAgentSpawner {
    tasks: Arc<rebon_plugin_tasks::runtime::TaskRegistry>,
}

#[async_trait::async_trait]
impl rebon_tool::SubAgentSpawner for TestSubAgentSpawner {
    async fn spawn(
        &self,
        _spec: rebon_tool::SubAgentSpec,
    ) -> Result<rebon_tool::SubAgentResult, String> {
        Ok(test_sub_agent_result())
    }

    async fn spawn_background(&self, spec: rebon_tool::SubAgentSpec) -> Result<String, String> {
        use rebon_plugin_tasks::runtime::{TaskData, TaskId, TaskStatus};

        let id = spec
            .metadata
            .get("agent_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("test-background-agent")
            .to_owned();

        // The registered task is built from the spec, not from defaults: a
        // caller that reads the task back is reading what it asked for, and
        // a stand-in that dropped the spec would let a continuation lose its
        // prompt, model and metadata without any test noticing.
        let mut snapshot = local_agent_snapshot(&id, TaskStatus::Running, spec.run_in_background);
        snapshot.metadata = spec.metadata.clone();
        if let Some(title) = spec
            .metadata
            .get("description")
            .and_then(serde_json::Value::as_str)
        {
            snapshot.title = title.to_owned();
        }
        if let TaskData::LocalAgent(data) = &mut snapshot.data {
            data.prompt = spec.prompt.clone();
            data.model = spec.model.clone();
            data.system = spec.system.clone();
            if let Some(agent_type) = spec
                .metadata
                .get("agent_type")
                .and_then(serde_json::Value::as_str)
            {
                data.agent_type = agent_type.to_owned();
            }
        }
        self.tasks
            .insert(TaskId::new(&id), snapshot, rebon_types::PromptCancel::new());
        Ok(id)
    }
}

fn test_sub_agent_result() -> rebon_tool::SubAgentResult {
    rebon_tool::SubAgentResult {
        final_text: String::new(),
        status: "completed".into(),
        tool_call_count: 0,
        stop_reason: None,
        error: None,
        output_file: None,
        duration_ms: None,
        agent_id: Some("test-agent".into()),
        agent_type: None,
        provider: None,
        model: None,
        sub_agent_tool_calls: None,
        read_file_count: None,
        total_tokens: None,
        output_tokens: None,
        usage: None,
        diagnostics: None,
        git: None,
    }
}

pub(super) fn test_runtime_model() -> SharedRuntimeModel {
    let client: Arc<dyn rebon_api::ModelClient> = Arc::new(rebon_api::MockModelClient::new());
    SharedRuntimeModel::new(RuntimeModelConfig {
        provider_name: "test".into(),
        client,
        model: "test-model".into(),
        model_profiles: rebon_types::ModelProfileMap::default(),
        title_model: "test-small-model".into(),
        model_marketing_name: None,
        knowledge_cutoff: None,
        prune_level: Some(rebon_api::PruneLevelHandle::new(rebon_api::PruneLevel::Off)),
        compact_provider: None,
        compact_fallback_provider: None,
        context_management: None,
        reasoning_mode: None,
    })
}

/// An engine with the built-in tools *and* the process kernel's tool seat
/// attached — what a real session has. Feature tools (StructuredOutput,
/// NotebookEdit, the web tools, …) are plugin-registered on that seat now,
/// so a bare `Engine::with_builtin_tools()` no longer sees them.
pub(crate) fn builtin_test_engine() -> Arc<rebon_core::Engine> {
    let engine = rebon_core::Engine::with_builtin_tools();
    engine.attach_upstream_tool_context(
        rebon_harness::kernel_bootstrap::process_kernel()
            .context()
            .clone(),
    );
    Arc::new(engine)
}

pub(crate) fn make_test_tui_session() -> TuiEngineSession {
    let handler = rebon_acp::DefaultHandler::default();
    let server_state = handler.state().clone();
    let session = server_state.create_session(".".into(), Vec::new());
    let session_id = session.id.clone();
    let (permission_broker, permission_rx) =
        rebon_core::permission::ChannelPermissionBroker::new(&session_id);
    let permission_broker = Arc::new(permission_broker);
    let (update_publisher, update_rx) = rebon_agent_core::ChannelSessionUpdatePublisher::new();
    let update_publisher: Arc<dyn rebon_agent_core::SessionUpdatePublisher> =
        Arc::new(update_publisher);
    let (_mcp_load_tx, mcp_load_rx) = tokio::sync::mpsc::unbounded_channel();
    let mcp =
        crate::session::mcp::SessionMcp::with_loader(mcp_load_rx, TuiMcpLoadStatus::NotConfigured);
    let engine = Arc::new(rebon_core::Engine::new());
    let kernel_scopes = rebon_kernel_seats::kernel_services::SessionKernelScopes::new(
        rebon_harness::kernel_bootstrap::process_kernel(),
        engine.clone(),
        std::env::temp_dir(),
    );
    let _binding = kernel_scopes.acquire(&session_id);
    let tasks = kernel_scopes.host_task_registry(&session_id);
    let client: Arc<dyn rebon_api::ModelClient> = Arc::new(rebon_api::MockModelClient::new());
    let core_executor = Arc::new(rebon_core::query::EngineQueryExecutor::new(
        engine.clone(),
        client.clone(),
        std::env::temp_dir(),
        "test-model",
    ));
    let resume_replay = core_executor.resume_replay_handle();
    let engine_core = Arc::new(crate::session::runtime::DeferredEngineCore::prebuilt(
        crate::session::runtime::EngineCore {
            executor: core_executor.clone(),
            resume_replay,
            blueprint: core_executor,
        },
    ));
    let task_notification_poller =
        crate::task_notification_poller::TaskNotificationPoller::new(tasks.as_ref().clone());
    let local_backend: Arc<dyn rebon_agent_core::AgentBackend> = Arc::new(
        rebon_agent_core::LocalAgentBackend::new(Arc::new(rebon_agent_core::StubPromptExecutor)),
    );
    let agent_switch = Arc::new(rebon_agent_core::AgentBackendSwitch::new(
        local_backend.clone(),
    ));
    // No agent CLIs in a test session: every turn stays on the local
    // engine, which is what the tests below assume.
    let session_agents = Arc::new(rebon_agent_core::routing::SessionAgents::local_only(
        agent_switch,
        local_backend.clone(),
        std::env::temp_dir(),
        ".",
        session_id.clone(),
    ));
    let projects_root = std::env::temp_dir();
    let cwd = ".".to_string();
    let executor: Arc<dyn rebon_agent_core::PromptExecutor> =
        Arc::new(rebon_agent_core::StubPromptExecutor);
    let sub_agent_spawner: Arc<dyn rebon_tool::SubAgentSpawner> = Arc::new(TestSubAgentSpawner {
        tasks: tasks.clone(),
    });
    let file_history_tracker = submit_tracker(&session_id);
    let policy = rebon_core::policy_seat::PolicySources::default().with_context(
        rebon_core::policy_seat::PolicyContext {
            cwd: cwd.clone(),
            transcript_path: "test-transcript.jsonl".into(),
            session_id: session_id.clone(),
            ..Default::default()
        },
    );
    let skill_registry = Arc::new(rebon_plugin_skill::SkillRegistry::new());
    let skill_state = Arc::new(Mutex::new(rebon_plugin_skill::SkillState::empty(
        &session_id,
        &cwd,
    )));
    let mid_turn_queue =
        crate::session::mid_turn_queue::MidTurnQueuedSubmitPoller::new(&session_id);
    let runtime = Arc::new(SessionRuntime {
        session_id: session_id.clone(),
        projects_root: projects_root.clone(),
        cwd: cwd.clone(),
        executor: executor.clone(),
        backend: local_backend.clone(),
        session_agents: session_agents.clone(),
        acp_subagent_pool: None,
        sub_agent_spawner: sub_agent_spawner.clone(),
        file_history_tracker: file_history_tracker.clone(),
        policy: policy.clone(),
        skill_registry: skill_registry.clone(),
        skill_state,
        update_publisher: update_publisher.clone(),
        permission_broker: permission_broker.clone(),
        mid_turn_queue: mid_turn_queue.clone(),
        tasks: tasks.clone(),
        task_notification_poller: task_notification_poller.clone(),
        runtime_handle: None,
    });
    TuiEngineSession {
        remote_background_attachment: None,
        pending_hosted_session: None,
        session: crate::session::EngineSession {
            engine_half: crate::session::runtime::SessionEngineHalf {
                runtime,
                runtime_factory: None,
                handler,
                update_publisher,
                engine_core,
                compact_runtime: crate::session::compact::CompactRuntime::new(),
                projection_invalid: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                update_rx,
                tasks,
                engine: engine.clone(),
                sub_agent_spawner,
                skill_registry,
                client,
                system_prompt_snapshot: Arc::new(RwLock::new(Some("test system prompt".into()))),
                permission_mode_cell: Arc::new(Mutex::new(
                    rebon_permissions::types::PermissionMode::Default,
                )),
                session_filter_handle: rebon_tool::SharedToolFilter::new(
                    rebon_core::coordinator_mode::normal_session_filter(),
                ),
                coordinator_mode_handle: rebon_tool::SharedCoordinatorMode::new(false),
                cron_scheduler: None,
                cron_poller: rebon_core::cron::CronPoller::new(),
                session_cron_store: rebon_tool::SessionCronStore::new(),
                task_notification_poller,
                kernel_scopes,
                executor,
                mcp: Some(mcp),
                permission_rx,
                live_policy_store: rebon_core::policy::PolicyStore::new(),
                team_manager: None,
                agent_registry: Arc::new(rebon_tool::AgentRegistry::builtins_only()),
                retry_notifier: rebon_api::RetryNotifier::default(),
                auto_mode_denials: Arc::new(Mutex::new(
                    rebon_permissions::auto_mode_denials::AutoModeDenialStore::default(),
                )),
                auto_mode_verdicts: Arc::new(rebon_permissions::AutoModeVerdictCache::default()),
                subagent_filter_handle: rebon_tool::SharedToolFilter::new(
                    rebon_core::coordinator_mode::normal_session_filter(),
                ),
                // A real ledger, not a stand-in: a test session counts its
                // turns the way a live one does, which is what the `/cost`
                // regression rests on.
                usage_ledger: Arc::new(Mutex::new(crate::session::usage::UsageLedger::new(
                    rebon_types::wall_clock_ms(),
                ))),
            },
            session_id: session_id.clone(),
            projects_root,
            server_state,
            cwd: cwd.clone(),
            model: rebon_harness::SessionModel {
                provider_name: "test".into(),
                provider_format: crate::rebon_config::ProviderFormat::Openai,
                name: "test-model".into(),
                default_name: "test-model".into(),
                title_name: "test-small-model".into(),
                prune_level: rebon_api::PruneLevelHandle::new(rebon_api::PruneLevel::Off),
                service_tier: rebon_api::ServiceTierHandle::default(),
                service_tier_available: false,
                runtime_model: test_runtime_model(),
            },
            loaded_transcript: Vec::new(),
            session_active_lock: None,
            attached_background_job_id: None,
            session_start_hook: None,
            startup: crate::session::startup::SessionStartupParams {
                effort_level: None,
                permission_mode: None,
                queue_session: false,
                channels: Vec::new(),
                development_channels: Vec::new(),
                settings: Vec::new(),
                add_dirs: Vec::new(),
                plugin_dirs: Vec::new(),
                mcp_configs: Vec::new(),
                strict_mcp_config: false,
            },
        },
        resume_warning: None,
        ui_mode: UiMode::Screen,
        configured_ui_mode: UiMode::Screen,
        math_rendering_mode: crate::rebon_config::MathRenderingMode::Off,
        terminal_startup: crate::tui::wiring::TuiStartupParams {
            agent_view: false,
            hosted: false,
            local: false,
            agent_view_cwd_scope: None,
            notices: Vec::new(),
        },
    }
}

pub(crate) fn make_ultraplan_test_tui_session() -> TuiEngineSession {
    let mut session = make_test_tui_session();
    session.engine_half.engine = builtin_test_engine();
    session
}

pub(super) fn local_agent_snapshot(
    id: &str,
    status: rebon_plugin_tasks::runtime::TaskStatus,
    backgrounded: bool,
) -> rebon_plugin_tasks::runtime::TaskSnapshot {
    use rebon_plugin_tasks::runtime::{LocalAgentData, TaskData, TaskId, TaskKind, TaskSnapshot};

    TaskSnapshot {
        id: TaskId::new(id),
        kind: TaskKind::LocalAgent,
        status,
        title: "worker".into(),
        last_progress: None,
        error: None,
        result: None,
        is_backgrounded: backgrounded,
        notified: false,
        start_time_ms: 0,
        end_time_ms: None,
        metadata: serde_json::json!({}),
        data: TaskData::LocalAgent(LocalAgentData {
            prompt: "do work".into(),
            agent_type: "general-purpose".into(),
            model: None,
            system: None,
            allowed_tools: None,
            token_count: 0,
            tool_use_count: 0,
            transcript: Vec::new(),
            streaming_text: None,
            pending_messages: Vec::new(),
            retrieved: false,
        }),
    }
}

pub(super) fn insert_local_agent_task(
    reg: &rebon_plugin_tasks::runtime::TaskRegistry,
    id: &str,
    status: rebon_plugin_tasks::runtime::TaskStatus,
    backgrounded: bool,
) -> rebon_types::PromptCancel {
    let cancel = rebon_types::PromptCancel::new();
    let snapshot = local_agent_snapshot(id, status, backgrounded);
    reg.insert(
        rebon_plugin_tasks::runtime::TaskId::new(id),
        snapshot,
        cancel.clone(),
    );
    cancel
}

pub(super) fn insert_terminal_agent_notification(
    app: &mut AppState,
    task_id: &str,
    output_file: &str,
) {
    use rebon_plugin_tasks::runtime::{
        LocalAgentData, TaskData, TaskId, TaskKind, TaskSnapshot, TaskStatus,
    };

    app.tasks.insert(
        TaskId::new(task_id),
        TaskSnapshot {
            id: TaskId::new(task_id),
            kind: TaskKind::LocalAgent,
            status: TaskStatus::Completed,
            title: "worker".into(),
            last_progress: None,
            error: None,
            result: Some(serde_json::json!({
                "final_text": "done",
                "output_file": output_file,
            })),
            is_backgrounded: true,
            notified: false,
            start_time_ms: 0,
            end_time_ms: Some(1),
            metadata: serde_json::json!({}),
            data: TaskData::LocalAgent(LocalAgentData {
                prompt: "do work".into(),
                agent_type: "worker".into(),
                model: None,
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        },
        rebon_types::PromptCancel::new(),
    );
}

pub(super) fn push_test_user_message(app: &mut AppState, uuid: &str, text: &str) {
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_tui::Message::User(rebon_tui::UserMessage {
            uuid: uuid.to_string(),
            timestamp: format!("2026-04-14T00:00:00.{}Z", uuid.trim_start_matches('u')),
            message: rebon_tui::UserMessageInner {
                role: rebon_tui::UserRole::User,
                content: vec![rebon_tui::UserContentBlock::Text(
                    rebon_tui::UserTextBlock {
                        text: text.to_string(),
                    },
                )],
            },
            is_compact_summary: None,
            is_meta: None,
            is_visible_in_transcript_only: None,
            image_paste_ids: None,
            plan_content: None,
        })),
    );
}

pub(super) struct RuntimeModeEnvGuard {
    previous: Vec<(&'static str, Option<String>)>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl RuntimeModeEnvGuard {
    pub(super) fn set_coordinator(value: Option<&str>) -> Self {
        Self::set_values(&[
            ("REBON_COORDINATOR_MODE", value),
            ("REBON_ALLOW_TOOLS", None),
            ("REBON_DENY_TOOLS", None),
        ])
    }

    pub(super) fn set_ultraplan_max_rounds(value: Option<&str>) -> Self {
        Self::set_values(&[("REBON_ULTRAPLAN_MAX_ROUNDS", value)])
    }

    pub(super) fn set_ultraplan_runtime_policy(
        runtime: Option<&str>,
        enforce: Option<&str>,
    ) -> Self {
        Self::set_values(&[
            ("REBON_ULTRAPLAN_RUNTIME", runtime),
            ("REBON_ULTRAPLAN_POLICY_ENFORCE", enforce),
        ])
    }

    fn set_values(values: &[(&'static str, Option<&str>)]) -> Self {
        let guard = crate::test_env::lock_env();
        let previous = values
            .iter()
            .map(|(key, _)| (*key, std::env::var(key).ok()))
            .collect::<Vec<_>>();
        for (key, value) in values {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        Self {
            previous,
            _guard: guard,
        }
    }
}

impl Drop for RuntimeModeEnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.previous {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
