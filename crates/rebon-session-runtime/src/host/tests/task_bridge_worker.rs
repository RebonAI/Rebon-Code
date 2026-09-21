//! Task-bridge behaviour a worker turn drives.
//!
//! Split out of `task_bridge.rs`: these tests stand up an IPC
//! server, attach a teammate runtime or a task-registry resolver, and drive a
//! rebuilt background turn. They move with the worker and the IPC server; the
//! store bridge's own tests stay beside the mirror half that keeps it.

use super::super::*;
use super::support::*;
use rebon_plugin_tasks::runtime::{
    register_in_process_teammate_task, InProcessTeammateData, InProcessTeammateTaskSpec,
    LocalAgentData, LocalWorkflowData, TaskData, TaskId, TaskSnapshot, TaskStatus,
    TeammateIdentity, WorkflowProgressEntry,
};

fn attach_task_scopes(
    ipc: &BackgroundIpcServer,
) -> Arc<rebon_kernel_seats::kernel_services::SessionKernelScopes> {
    let scopes = rebon_kernel_seats::kernel_services::SessionKernelScopes::new(
        rebon_harness::kernel_bootstrap::process_kernel(),
        Arc::new(rebon_core::Engine::with_builtin_tools()),
        rebon_harness::projects_root(),
    );
    ipc.attach_task_registry_resolver(scopes.task_registry_resolver());
    scopes
}

fn session_registry(
    scopes: &rebon_kernel_seats::kernel_services::SessionKernelScopes,
    session_id: &str,
) -> Arc<TaskRegistry> {
    let _binding = scopes.acquire(session_id);
    scopes.host_task_registry(session_id)
}

#[test]
fn background_workflow_descriptor_carries_a_bounded_live_graph_projection() {
    let long_phase = "phase".repeat(40);
    let long_label = "agent".repeat(40);
    let long_error = "error".repeat(80);
    let mut progress_entries: Vec<_> = (1..=101)
        .map(|index| WorkflowProgressEntry::Agent {
            index,
            state: "error".into(),
            phase_title: Some(long_phase.clone()),
            phase_id: Some(format!("phase-{index}")),
            label: long_label.clone(),
            tokens: 0,
            tool_calls: 0,
            tool_call_details: vec![serde_json::json!({"large": "payload".repeat(10_000)})],
            duration_ms: None,
            error: Some(long_error.clone()),
            agent_id: Some(format!("agent-{index}")),
        })
        .collect();
    progress_entries.extend((1..=100).map(|index| WorkflowProgressEntry::Phase {
        title: long_phase.clone(),
        state: if index % 2 == 0 {
            "completed".into()
        } else {
            "start".into()
        },
        phase_id: Some(format!("phase-{index}")),
    }));
    progress_entries.extend((1..=20).map(|_| WorkflowProgressEntry::Log {
        message: "log".repeat(120),
    }));
    let mut workflow = TaskSnapshot::new_pending(
        TaskId::new("workflow"),
        "Review".into(),
        TaskData::LocalWorkflow(LocalWorkflowData {
            run_id: "run-live".into(),
            workflow_name: "review".repeat(80),
            summary: Some("summary".repeat(100)),
            agent_count: 101,
            progress_entries,
            token_count: 0,
            tool_use_count: 0,
            output_path: None,
            script_path: None,
            args: None,
        }),
    );
    workflow.last_progress = Some("progress".repeat(1_000));
    workflow.error = Some("error".repeat(1_000));

    let descriptor = background_task_descriptor(&workflow);
    let result = descriptor.result.as_ref().expect("workflow result");
    assert!(result.is_object());
    assert!(result.to_string().chars().count() <= WORKFLOW_PREVIEW_RESULT_MAX_CHARS);
    let progress = result
        .get("workflowProgress")
        .expect("workflow progress projection");
    assert_eq!(progress["runId"], "run-live");
    let entries = progress["entries"].as_array().expect("progress entries");
    assert_eq!(entries.len(), 88);
    let agents: Vec<_> = entries
        .iter()
        .filter(|entry| entry["entry"]["type"] == "agent")
        .collect();
    assert_eq!(agents.len(), 48);
    assert_eq!(agents.last().unwrap()["entry"]["agentId"], "agent-48");
    assert!(agents
        .iter()
        .all(|entry| entry["entry"]["toolCallDetails"] == serde_json::json!([])));
    assert!(descriptor
        .last_progress
        .as_ref()
        .is_some_and(|value| value.chars().count() <= 2_048));
    assert!(descriptor
        .error
        .as_ref()
        .is_some_and(|value| value.chars().count() <= 2_048));
}

#[derive(Default)]
struct RecordingBackgroundTeamManager {
    task_messages: Mutex<Vec<(String, String)>>,
    registry: Option<Arc<TaskRegistry>>,
}

#[async_trait::async_trait]
impl TeamManager for RecordingBackgroundTeamManager {
    async fn spawn_teammate(
        &self,
        _spec: rebon_tool::TeammateSpawnSpec,
    ) -> Result<rebon_tool::TeammateSpawnResult, String> {
        unreachable!()
    }

    async fn send_message(
        &self,
        _team_name: &str,
        _recipient: &str,
        _message: String,
    ) -> Result<(), rebon_tools_core::ToolErrorPresentation> {
        unreachable!()
    }

    async fn send_message_to_task(
        &self,
        task_id: &str,
        message: String,
    ) -> Result<(), rebon_tools_core::ToolErrorPresentation> {
        self.task_messages
            .lock()
            .expect("task messages poisoned")
            .push((task_id.to_string(), message.clone()));
        if let Some(registry) = self.registry.as_ref() {
            registry.record_live_event(
                &TaskId::new(task_id),
                TaskLiveEventKind::UserMessage { text: message },
            );
        }
        Ok(())
    }

    async fn request_shutdown(
        &self,
        _team_name: &str,
        _recipient: &str,
        _reason: Option<String>,
    ) -> Result<String, String> {
        unreachable!()
    }

    async fn request_plan_approval(
        &self,
        _team_name: &str,
        _agent_name: &str,
        _plan_content: String,
    ) -> Result<String, String> {
        unreachable!()
    }

    async fn delete_team(&self, _team_name: &str) -> Result<(), String> {
        unreachable!()
    }
}

fn running_named_local_agent_snapshot(task_id: &str, display_name: &str) -> TaskSnapshot {
    let mut snapshot = TaskSnapshot::new_pending(
        TaskId::new(task_id),
        "Verify changes".into(),
        TaskData::LocalAgent(LocalAgentData {
            prompt: "Verify changes".into(),
            agent_type: "verification".into(),
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
    );
    snapshot.status = TaskStatus::Running;
    snapshot.is_backgrounded = true;
    snapshot.metadata = serde_json::json!({
        "display_name": display_name,
        "_runtime_is_idle": true,
    });
    snapshot
}

#[tokio::test]
async fn attaching_a_new_teammate_runtime_keeps_an_empty_runtime_while_its_bridge_runs() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    let scopes = attach_task_scopes(&ipc);
    let first_registry = session_registry(&scopes, "first-session");
    let first_bridge = ipc
        .attach_teammate_runtime(
            first_registry.clone(),
            Arc::new(RecordingBackgroundTeamManager::default()),
            "first-session".into(),
        )
        .unwrap();
    let first_generation = first_bridge.begin(TaskEventCursor::ZERO);

    let second_registry = Arc::new(TaskRegistry::new());
    ipc.attach_teammate_runtime(
        second_registry,
        Arc::new(RecordingBackgroundTeamManager::default()),
        "second-session".into(),
    );
    assert!(ipc
        .teammate_runtimes
        .lock()
        .expect("poisoned")
        .iter()
        .any(|runtime| Arc::ptr_eq(&runtime.registry, &first_registry)));

    assert!(first_bridge.finish_if_current(first_generation).is_ok());
    ipc.attach_teammate_runtime(
        Arc::new(TaskRegistry::new()),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "third-session".into(),
    );
    assert!(!ipc
        .teammate_runtimes
        .lock()
        .expect("poisoned")
        .iter()
        .any(|runtime| Arc::ptr_eq(&runtime.registry, &first_registry)));
}

#[tokio::test]
async fn rebuilt_background_turn_can_message_an_idle_agent_from_a_prior_turn() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    let scopes = attach_task_scopes(&ipc);
    let first_registry = session_registry(&scopes, "first-session");
    let task_id = TaskId::new("retained-verifier");
    let mut snapshot = TaskSnapshot::new_pending(
        task_id.clone(),
        "Verify changes".into(),
        TaskData::LocalAgent(LocalAgentData {
            prompt: "Verify changes".into(),
            agent_type: "verification".into(),
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
    );
    snapshot.status = TaskStatus::Running;
    snapshot.is_backgrounded = true;
    snapshot.metadata = serde_json::json!({ "_runtime_is_idle": true });
    first_registry.insert(task_id.clone(), snapshot, rebon_types::PromptCancel::new());
    ipc.attach_teammate_runtime(
        first_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "first-session".into(),
    );

    // The next turn rebuilds the session and resolves the same session seat.
    ipc.attach_teammate_runtime(
        first_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "first-session".into(),
    );

    assert!(ipc
        .teammate_runtimes
        .lock()
        .expect("poisoned")
        .iter()
        .any(|runtime| Arc::ptr_eq(&runtime.registry, &first_registry)));

    let controller = ipc.task_runtime_controller(store.clone(), state.identity.job_id.clone());
    rebon_tool::TaskRuntimeController::send_message_to_task(
        controller.as_ref(),
        "first-session",
        task_id.as_str(),
        "focused re-check".into(),
    )
    .await
    .unwrap();

    let snapshot = first_registry.snapshot(&task_id).unwrap();
    let TaskData::LocalAgent(data) = snapshot.data else {
        panic!("expected local agent");
    };
    assert_eq!(data.pending_messages, vec!["focused re-check"]);

    let bridge = ipc
        .teammate_runtimes
        .lock()
        .expect("poisoned")
        .iter()
        .find(|runtime| Arc::ptr_eq(&runtime.registry, &first_registry))
        .unwrap()
        .task_bridge
        .clone();
    assert!(bridge.is_running());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if task_live_batches_contain_user_message(
            &store.read_events(&state.identity.job_id).unwrap(),
            task_id.as_str(),
            "focused re-check",
        ) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "restarted bridge did not persist the routed follow-up"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn rebuilt_background_turn_routes_unique_display_name_after_a_rebuild() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    let scopes = attach_task_scopes(&ipc);
    let first_registry = session_registry(&scopes, "first-session");
    let first_id = TaskId::new("retained-verifier");
    first_registry.insert(
        first_id.clone(),
        running_named_local_agent_snapshot(first_id.as_str(), "verify-fix"),
        rebon_types::PromptCancel::new(),
    );
    ipc.attach_teammate_runtime(
        first_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "first-session".into(),
    );
    // The next turn rebuilds the session and resolves the same session seat,
    // so last turn's agent is still addressable.
    ipc.attach_teammate_runtime(
        first_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "first-session".into(),
    );

    let controller = ipc.task_runtime_controller(store, state.identity.job_id);
    rebon_tool::TaskRuntimeController::send_message_to_task(
        controller.as_ref(),
        "first-session",
        "VERIFY-FIX",
        "follow up by name".into(),
    )
    .await
    .unwrap();

    let snapshot = first_registry.snapshot(&first_id).unwrap();
    let TaskData::LocalAgent(data) = snapshot.data else {
        panic!("expected local agent");
    };
    assert_eq!(data.pending_messages, vec!["follow up by name"]);
}

/// The turn-to-turn property this cut is about, resolved by exact id rather
/// than by display name: a task created in one turn is still addressable in
/// the next, because both turns resolve the same session task-registry seat.

#[tokio::test]
async fn rebuilt_background_turn_routes_an_exact_id_from_a_prior_turn() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    let scopes = attach_task_scopes(&ipc);
    let first_registry = session_registry(&scopes, "first-session");
    let task_id = TaskId::new("retained-verifier");
    first_registry.insert(
        task_id.clone(),
        running_named_local_agent_snapshot(task_id.as_str(), "verify-fix"),
        rebon_types::PromptCancel::new(),
    );
    ipc.attach_teammate_runtime(
        first_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "first-session".into(),
    );
    // The next turn rebuilds the session and re-attaches.
    ipc.attach_teammate_runtime(
        first_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "first-session".into(),
    );

    let controller = ipc.task_runtime_controller(store, state.identity.job_id);
    rebon_tool::TaskRuntimeController::send_message_to_task(
        controller.as_ref(),
        "first-session",
        task_id.as_str(),
        "follow up by id".into(),
    )
    .await
    .unwrap();

    let snapshot = first_registry.snapshot(&task_id).unwrap();
    let TaskData::LocalAgent(data) = snapshot.data else {
        panic!("expected local agent");
    };
    assert_eq!(data.pending_messages, vec!["follow up by id"]);
}

#[tokio::test]
async fn rebuilt_background_turn_rejects_ambiguous_display_name_within_the_session() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    let scopes = attach_task_scopes(&ipc);
    let first_registry = session_registry(&scopes, "first-session");
    // Both agents live in one session seat; ambiguity is judged inside it.
    let second_registry = Arc::clone(&first_registry);
    for (registry, task_id) in [
        (&first_registry, "retained-verifier"),
        (&second_registry, "current-verifier"),
    ] {
        registry.insert(
            TaskId::new(task_id),
            running_named_local_agent_snapshot(task_id, "verify-fix"),
            rebon_types::PromptCancel::new(),
        );
        ipc.attach_teammate_runtime(
            Arc::clone(registry),
            Arc::new(RecordingBackgroundTeamManager::default()),
            format!("{task_id}-session"),
        );
    }

    let controller = ipc.task_runtime_controller(store, state.identity.job_id);
    let error = rebon_tool::TaskRuntimeController::send_message_to_task(
        controller.as_ref(),
        "first-session",
        "verify-fix",
        "ambiguous follow up".into(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.code, "agent_ambiguous");
    for (registry, task_id) in [
        (&first_registry, "retained-verifier"),
        (&second_registry, "current-verifier"),
    ] {
        let snapshot = registry.snapshot(&TaskId::new(task_id)).unwrap();
        let TaskData::LocalAgent(data) = snapshot.data else {
            panic!("expected local agent");
        };
        assert!(data.pending_messages.is_empty());
    }
}

#[tokio::test]
async fn rebuilt_background_turn_prefers_exact_id_over_prior_display_name() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    let scopes = attach_task_scopes(&ipc);
    let alias_registry = session_registry(&scopes, "first-session");
    alias_registry.insert(
        TaskId::new("prior-verifier"),
        running_named_local_agent_snapshot("prior-verifier", "current-verifier"),
        rebon_types::PromptCancel::new(),
    );
    ipc.attach_teammate_runtime(
        alias_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "first-session".into(),
    );
    let exact_registry = Arc::clone(&alias_registry);
    let exact_id = TaskId::new("current-verifier");
    exact_registry.insert(
        exact_id.clone(),
        running_named_local_agent_snapshot(exact_id.as_str(), "other-name"),
        rebon_types::PromptCancel::new(),
    );
    ipc.attach_teammate_runtime(
        exact_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "second-session".into(),
    );

    let controller = ipc.task_runtime_controller(store, state.identity.job_id);
    rebon_tool::TaskRuntimeController::send_message_to_task(
        controller.as_ref(),
        "first-session",
        exact_id.as_str(),
        "exact follow up".into(),
    )
    .await
    .unwrap();

    let exact = exact_registry.snapshot(&exact_id).unwrap();
    let TaskData::LocalAgent(exact_data) = exact.data else {
        panic!("expected local agent");
    };
    assert_eq!(exact_data.pending_messages, vec!["exact follow up"]);
    let alias = alias_registry
        .snapshot(&TaskId::new("prior-verifier"))
        .unwrap();
    let TaskData::LocalAgent(alias_data) = alias.data else {
        panic!("expected local agent");
    };
    assert!(alias_data.pending_messages.is_empty());
}

#[tokio::test]
async fn rebuilt_background_turn_can_stop_a_task_from_a_prior_turn() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    let scopes = attach_task_scopes(&ipc);
    let first_registry = session_registry(&scopes, "first-session");
    let task_id = TaskId::new("retained-verifier");
    let cancel = rebon_types::PromptCancel::new();
    first_registry.insert(
        task_id.clone(),
        running_named_local_agent_snapshot(task_id.as_str(), "verify-fix"),
        cancel.clone(),
    );
    ipc.attach_teammate_runtime(
        first_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "first-session".into(),
    );
    // The next turn rebuilds the session and resolves the same session seat.
    ipc.attach_teammate_runtime(
        first_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "first-session".into(),
    );

    let controller = ipc.task_runtime_controller(store, state.identity.job_id);
    let outcome = rebon_tool::TaskRuntimeController::stop_task(
        controller.as_ref(),
        "first-session",
        task_id.as_str(),
    )
    .await
    .unwrap();

    assert!(matches!(
        outcome,
        rebon_tool::StopTaskOutcome::Stopped { ref task_id, .. }
            if task_id == "retained-verifier"
    ));
    assert!(cancel.is_cancelled());
    assert_eq!(
        first_registry.snapshot(&task_id).unwrap().status,
        TaskStatus::Killed
    );
}

/// The bridge exits once every task in a registry has settled, and a parked
/// worker counts as settled. A stop arriving after that has nobody left to
/// publish it, so the client would keep rendering a killed worker as parked.

#[tokio::test]
async fn stopping_a_settled_registry_task_restarts_the_bridge_to_publish_the_kill() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    let scopes = attach_task_scopes(&ipc);
    let registry = session_registry(&scopes, "parked-session");
    let task_id = TaskId::new("parked-verifier");
    registry.insert(
        task_id.clone(),
        running_named_local_agent_snapshot(task_id.as_str(), "verify-parked"),
        rebon_types::PromptCancel::new(),
    );
    let bridge_state = ipc
        .attach_teammate_runtime(
            Arc::clone(&registry),
            Arc::new(RecordingBackgroundTeamManager::default()),
            "parked-session".into(),
        )
        .expect("teammate runtime carries a bridge state");
    assert!(
        !bridge_state.is_running(),
        "a settled registry has no bridge draining it"
    );

    let controller = ipc.task_runtime_controller(store.clone(), state.identity.job_id.clone());
    rebon_tool::TaskRuntimeController::stop_task(
        controller.as_ref(),
        "parked-session",
        task_id.as_str(),
    )
    .await
    .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let events = store.read_events(&state.identity.job_id).unwrap();
        if task_live_batches_contain_finished(&events, task_id.as_str(), "killed")
            || task_live_batches_contain_checkpoint(&events, task_id.as_str(), "killed")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the kill never reached the store"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn rebuilt_background_turn_tracks_shells_only_in_current_registry() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
    let scopes = attach_task_scopes(&ipc);
    let first_registry = session_registry(&scopes, "first-session");
    first_registry.insert(
        TaskId::new("retained-verifier"),
        running_named_local_agent_snapshot("retained-verifier", "verify-fix"),
        rebon_types::PromptCancel::new(),
    );
    ipc.attach_teammate_runtime(
        first_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "first-session".into(),
    );
    let current_registry = session_registry(&scopes, "second-session");
    ipc.attach_teammate_runtime(
        current_registry.clone(),
        Arc::new(RecordingBackgroundTeamManager::default()),
        "second-session".into(),
    );

    let controller = ipc.task_runtime_controller(store, state.identity.job_id);
    let shell_id = "sh_current_turn";
    rebon_tool::TaskRuntimeController::background_shell_started(
        controller.as_ref(),
        "second-session",
        rebon_tool::BackgroundShellTaskSpec {
            shell_id: shell_id.into(),
            tool_name: "Bash".into(),
            command: "cargo check".into(),
            session_id: Some("second-session".into()),
            agent_id: None,
            started_at_ms: 10,
        },
        rebon_types::PromptCancel::new(),
    );
    assert!(first_registry.snapshot(&TaskId::new(shell_id)).is_none());
    assert_eq!(
        current_registry
            .snapshot(&TaskId::new(shell_id))
            .unwrap()
            .status,
        TaskStatus::Running
    );

    rebon_tool::TaskRuntimeController::background_shell_finished(
        controller.as_ref(),
        "second-session",
        rebon_tool::BackgroundShellTaskCompletion {
            shell_id: shell_id.into(),
            status: rebon_tool::BackgroundShellCompletionStatus::Exited,
            completed_at_ms: 20,
            exit_code: Some(0),
            output: "ok".into(),
            stderr: String::new(),
            stream_order: None,
            error: None,
            next_cursor: 1,
            has_more: false,
            cursor_truncated: false,
            oldest_cursor: 0,
            observed: false,
        },
    );
    assert_eq!(
        current_registry
            .snapshot(&TaskId::new(shell_id))
            .unwrap()
            .status,
        TaskStatus::Completed
    );
    assert_eq!(
        current_registry
            .unnotified_terminal_agent_notifications()
            .len(),
        1
    );
    rebon_tool::TaskRuntimeController::background_shell_observed(
        controller.as_ref(),
        "second-session",
        shell_id,
    );
    assert!(current_registry
        .unnotified_terminal_agent_notifications()
        .is_empty());
}

#[test]
fn registered_background_task_reply_restarts_stopped_bridge_from_persisted_cursor() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let registry = Arc::new(TaskRegistry::new());
    let task_id = TaskId::new("failed-reviewer@team");
    let mut snapshot = TaskSnapshot::new_pending(
        task_id.clone(),
        "Review changes".into(),
        TaskData::InProcessTeammate(Box::new(InProcessTeammateData {
            identity: TeammateIdentity {
                agent_id: task_id.to_string(),
                agent_name: "failed-reviewer".into(),
                team_name: "team".into(),
                color: None,
                plan_mode_required: false,
                parent_session_id: "session".into(),
            },
            prompt: "initial".into(),
            model: None,
            model_profile: None,
            permission_mode: "auto".into(),
            awaiting_plan_approval: false,
            is_idle: false,
            shutdown_requested: false,
            pending_user_messages: Vec::new(),
            tool_use_count: 0,
            token_count: 0,
            transcript: Vec::new(),
            streaming_text: None,
        })),
    );
    snapshot.status = TaskStatus::Failed;
    snapshot.notified = true;
    registry.insert(task_id.clone(), snapshot, rebon_types::PromptCancel::new());
    let persisted_cursor = registry.session_live_events(None).latest_cursor;
    let manager = Arc::new(RecordingBackgroundTeamManager {
        task_messages: Mutex::new(Vec::new()),
        registry: Some(Arc::clone(&registry)),
    });
    let task_bridge = BackgroundTaskBridgeState::new();
    task_bridge.record_persisted_cursor(persisted_cursor);
    let runtimes = Arc::new(Mutex::new(vec![BackgroundTeammateRuntime {
        registry: registry.clone(),
        manager: manager.clone() as Arc<dyn TeamManager>,
        handle: runtime.handle().clone(),
        session_id: "session".into(),
        task_bridge: task_bridge.clone(),
    }]));

    reply_to_registered_background_task(
        &registry,
        &runtimes,
        &store,
        &job.identity.job_id,
        task_id.as_str(),
        "retry from agent view".into(),
    )
    .unwrap();

    assert_eq!(
        manager
            .task_messages
            .lock()
            .expect("task messages poisoned")
            .as_slice(),
        &[(task_id.to_string(), "retry from agent view".into())]
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let events = store.read_events(&job.identity.job_id).unwrap();
        let persisted_message = events.iter().any(|event| {
            event.kind == "task_live_batch"
                && serde_json::from_value::<BackgroundTaskEventBatch>(event.data.clone()).is_ok_and(
                    |batch| {
                        batch.from_cursor == persisted_cursor.get()
                            && batch.events.iter().any(|event| {
                                matches!(
                                    &event.event,
                                    BackgroundTaskEventKind::UserMessage { text }
                                        if text == "retry from agent view"
                                )
                            })
                    },
                )
        });
        let persisted_checkpoint =
            task_live_batches_contain_checkpoint(&events, task_id.as_str(), "failed");
        if persisted_message && persisted_checkpoint && !task_bridge.is_running() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "restarted bridge did not persist the teammate event and terminal checkpoint"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn registered_background_task_reply_queues_idle_teammate_follow_up() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let registry = Arc::new(TaskRegistry::new());
    let task_id = TaskId::new("reviewer@team");
    register_in_process_teammate_task(
        registry.as_ref(),
        InProcessTeammateTaskSpec {
            id: task_id.clone(),
            identity: TeammateIdentity {
                agent_id: "reviewer@team".into(),
                agent_name: "reviewer".into(),
                team_name: "team".into(),
                color: None,
                plan_mode_required: false,
                parent_session_id: "session".into(),
            },
            prompt: "initial".into(),
            model: None,
            model_profile: None,
            permission_mode: "auto".into(),
            agent_type: Some("Explore".into()),
            description: Some("inspect code".into()),
        },
    );
    rebon_plugin_tasks::runtime::mark_in_process_teammate_idle(
        registry.as_ref(),
        &task_id,
        Some("ready".into()),
    );

    reply_to_registered_background_task(
        &registry,
        &Arc::new(Mutex::new(Vec::new())),
        &store,
        &job.identity.job_id,
        task_id.as_str(),
        "follow up".into(),
    )
    .unwrap();

    let snapshot = registry.snapshot(&task_id).unwrap();
    let TaskData::InProcessTeammate(data) = snapshot.data else {
        panic!("expected teammate");
    };
    assert_eq!(data.pending_user_messages.len(), 1);
    assert_eq!(data.pending_user_messages[0].message, "follow up");
}
