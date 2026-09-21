//! The task store bridge, which stays with the binary.
//!
//! These drive `background::task_bridge` directly — descriptors, event
//! coalescing, notification persistence, and the bridge's exit conditions.
//! Nothing here stands up an IPC server; the tests that do were split into
//! `task_bridge_worker.rs` because they travel with the worker.

use super::super::*;
use super::support::*;
use rebon_plugin_tasks::runtime::{
    InProcessTeammateData, LocalAgentData, TaskData, TaskId, TaskSnapshot, TaskStatus,
    TeammateIdentity,
};

#[test]
fn background_task_descriptor_preserves_local_and_teammate_names() {
    let mut local = TaskSnapshot::new_pending(
        TaskId::new("local"),
        "Inspect code".into(),
        TaskData::LocalAgent(LocalAgentData {
            prompt: "Inspect code".into(),
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
    local.metadata = serde_json::json!({
        "display_name": " verify-final-app-stability "
    });
    let local_descriptor = background_task_descriptor(&local);
    assert_eq!(
        local_descriptor.agent_name.as_deref(),
        Some("verify-final-app-stability")
    );
    assert_eq!(local_descriptor.agent_type.as_deref(), Some("verification"));

    // A local agent parked at its turn boundary must ride the wire as
    // "idle", not "running": descriptor-only clients (the desktop Agent
    // View) have no other way to tell it from a worker mid-turn.
    local.status = TaskStatus::Running;
    local.metadata = serde_json::json!({
        "display_name": " verify-final-app-stability ",
        "_runtime_is_idle": true,
    });
    assert_eq!(background_task_descriptor(&local).status, "idle");
    local.metadata = serde_json::json!({
        "display_name": " verify-final-app-stability ",
    });
    assert_eq!(background_task_descriptor(&local).status, "running");

    let mut teammate = TaskSnapshot::new_pending(
        TaskId::new("reviewer@team"),
        "Review changes".into(),
        TaskData::InProcessTeammate(Box::new(InProcessTeammateData {
            identity: TeammateIdentity {
                agent_id: "reviewer@team".into(),
                agent_name: "reviewer".into(),
                team_name: "team".into(),
                color: None,
                plan_mode_required: false,
                parent_session_id: "session".into(),
            },
            prompt: "Review changes".into(),
            model: None,
            model_profile: None,
            permission_mode: "auto".into(),
            awaiting_plan_approval: false,
            is_idle: true,
            shutdown_requested: false,
            pending_user_messages: Vec::new(),
            tool_use_count: 0,
            token_count: 0,
            transcript: Vec::new(),
            streaming_text: None,
        })),
    );
    teammate.status = TaskStatus::Running;
    teammate.metadata = serde_json::json!({ "agent_type": "Explore" });
    let teammate_descriptor = background_task_descriptor(&teammate);
    assert_eq!(teammate_descriptor.agent_name.as_deref(), Some("reviewer"));
    assert_eq!(teammate_descriptor.agent_type.as_deref(), Some("Explore"));
    assert_eq!(teammate_descriptor.title, "Explore");
    assert_eq!(teammate_descriptor.status, "idle");
}

#[test]
fn task_bridge_activity_prevents_a_stale_exit() {
    let bridge = BackgroundTaskBridgeState::new();
    let generation = bridge.begin(TaskEventCursor::ZERO);

    assert!(bridge.claim_after_activity().is_none());
    let current_generation = bridge.finish_if_current(generation).unwrap_err();
    assert!(bridge.is_running());
    assert!(bridge.finish_if_current(current_generation).is_ok());
    assert!(!bridge.is_running());
}

#[test]
fn task_event_bridge_coalesces_adjacent_text_by_stream_kind() {
    let assistant = |cursor, task_id: &str, delta: &str, snapshot: &str| BackgroundTaskEvent {
        cursor,
        task_id: task_id.to_string(),
        timestamp_ms: cursor,
        task: None,
        event: BackgroundTaskEventKind::AssistantTextDelta {
            delta: delta.to_string(),
            snapshot: snapshot.to_string(),
        },
    };
    let thinking = |cursor, task_id: &str, delta: &str, snapshot: &str| BackgroundTaskEvent {
        cursor,
        task_id: task_id.to_string(),
        timestamp_ms: cursor,
        task: None,
        event: BackgroundTaskEventKind::ThinkingDelta {
            delta: delta.to_string(),
            snapshot: snapshot.to_string(),
        },
    };
    let events = coalesce_task_events(vec![
        assistant(1, "agent-1", "a", "a"),
        assistant(2, "agent-1", "b", "ab"),
        thinking(3, "agent-1", "c", "c"),
        thinking(4, "agent-1", "d", "cd"),
        assistant(5, "agent-1", "e", "abe"),
        assistant(6, "agent-2", "x", "x"),
    ]);

    assert_eq!(events.len(), 4);
    assert_eq!(events[0].cursor, 2);
    assert!(matches!(
        &events[0].event,
        BackgroundTaskEventKind::AssistantTextDelta { delta, snapshot }
            if delta == "ab" && snapshot == "ab"
    ));
    assert_eq!(events[1].cursor, 4);
    assert!(matches!(
        &events[1].event,
        BackgroundTaskEventKind::ThinkingDelta { delta, snapshot }
            if delta == "cd" && snapshot == "cd"
    ));
    assert!(matches!(
        &events[2].event,
        BackgroundTaskEventKind::AssistantTextDelta { delta, .. } if delta == "e"
    ));
}

fn idle_background_agent_snapshot(
    id: &str,
    title: &str,
    session_id: &str,
    start_time_ms: u64,
) -> TaskSnapshot {
    let mut snapshot = running_local_agent_snapshot(id, title);
    snapshot.is_backgrounded = true;
    snapshot.start_time_ms = start_time_ms;
    snapshot.metadata = serde_json::json!({
        "_runtime_is_idle": true,
        "parent_session_id": session_id,
    });
    snapshot.result = Some(serde_json::json!({
        "status": "completed",
        "final_text": format!("{title} completed"),
        "tool_call_count": 1,
        "total_tokens": 10,
    }));
    snapshot
}

fn bind_running_job_session(store: &BackgroundStore, job_id: &str, session_id: &str) {
    store
        .update_state(job_id, |state| {
            state.identity.session_id = Some(session_id.to_string());
            state.process.status = BackgroundJobStatus::Running;
            state.process.updated_at_ms = now_ms();
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn task_store_bridge_queues_idle_agent_notification_only_after_parent_stop() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let session_id = "session-idle-notification";
    bind_running_job_session(&store, &job.identity.job_id, session_id);
    let registry = Arc::new(TaskRegistry::new());
    let task_id = rebon_plugin_tasks::runtime::TaskId::new("agent-idle-notification");
    registry.insert(
        task_id.clone(),
        idle_background_agent_snapshot(task_id.as_str(), "idle notification", session_id, 1),
        rebon_types::PromptCancel::new(),
    );
    let expected_prompt_id = task_notification_pending_prompt_id(
        &registry
            .unnotified_terminal_notifications()
            .into_iter()
            .next()
            .expect("idle agent notification"),
    );

    let (stop_tx, bridge) = spawn_task_store_bridge(
        Arc::clone(&registry),
        store.clone(),
        job.identity.job_id.clone(),
        session_id.into(),
    );
    tokio::time::sleep(Duration::from_millis(TASK_BRIDGE_POLL_INTERVAL_MS * 3)).await;
    assert!(store
        .read_state(&job.identity.job_id)
        .unwrap()
        .identity
        .pending_prompts
        .is_empty());
    assert!(!registry.snapshot(&task_id).unwrap().notified);

    stop_tx.send(()).expect("bridge stop receiver alive");
    tokio::time::sleep(Duration::from_millis(TASK_BRIDGE_POLL_INTERVAL_MS * 3)).await;

    let state = store.read_state(&job.identity.job_id).unwrap();
    assert_eq!(state.identity.pending_prompts.len(), 1);
    assert_eq!(state.identity.pending_prompts[0].id, expected_prompt_id);
    assert!(state.identity.pending_prompts[0]
        .text
        .contains("<task-id>agent-idle-notification</task-id>"));
    assert!(registry.snapshot(&task_id).unwrap().notified);

    tokio::time::sleep(Duration::from_millis(TASK_BRIDGE_POLL_INTERVAL_MS * 2)).await;
    assert_eq!(
        store
            .read_state(&job.identity.job_id)
            .unwrap()
            .identity
            .pending_prompts
            .len(),
        1,
        "the same idle generation must not enqueue twice"
    );

    // A parked worker never turns terminal on its own — `keep_runtime_resumable`
    // holds it at Running so it stays resumable. Waiting for terminal here is
    // waiting for the post-parent timeout, which pins this worker process and
    // its session for the full hour; having reported, it is settled.
    tokio::time::timeout(Duration::from_secs(2), bridge)
        .await
        .expect("bridge should exit once the parked agent has reported")
        .expect("bridge task should not panic");
    assert_eq!(
        registry.snapshot(&task_id).unwrap().status,
        TaskStatus::Running,
        "the parked worker stays resumable after the bridge exits"
    );
}

#[test]
fn task_notifications_are_persisted_fifo_and_acked_after_success() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let session_id = "session-notification-fifo";
    bind_running_job_session(&store, &job.identity.job_id, session_id);
    let registry = TaskRegistry::new();
    let first_id = rebon_plugin_tasks::runtime::TaskId::new("agent-first-notification");
    let second_id = rebon_plugin_tasks::runtime::TaskId::new("agent-second-notification");
    let mut first_snapshot =
        idle_background_agent_snapshot(first_id.as_str(), "first", session_id, 1);
    first_snapshot
        .result
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
        .expect("agent result object")
        .insert(
            "output_file".into(),
            serde_json::Value::String("C:/reports/agent-first.md".into()),
        );
    registry.insert(
        first_id.clone(),
        first_snapshot,
        rebon_types::PromptCancel::new(),
    );
    registry.insert(
        second_id.clone(),
        idle_background_agent_snapshot(second_id.as_str(), "second", session_id, 2),
        rebon_types::PromptCancel::new(),
    );
    let poller = crate::task_notification_poller::TaskNotificationPoller::new(registry.clone());
    let mut failure_logged = false;

    assert!(persist_task_notifications_after_parent(
        &registry,
        &poller,
        &store,
        &job.identity.job_id,
        session_id,
        &mut failure_logged,
    ));
    let state = store.read_state(&job.identity.job_id).unwrap();
    assert_eq!(state.identity.pending_prompts.len(), 2);
    assert!(state.identity.pending_prompts[0]
        .id
        .starts_with("u-internal-"));
    assert_eq!(
        state.identity.pending_prompts[0].coordinator_report_paths,
        vec!["C:/reports/agent-first.md"]
    );
    assert!(state.identity.pending_prompts[0]
        .text
        .contains("<task-id>agent-first-notification</task-id>"));
    assert!(state.identity.pending_prompts[1]
        .coordinator_report_paths
        .is_empty());
    assert!(state.identity.pending_prompts[1]
        .text
        .contains("<task-id>agent-second-notification</task-id>"));
    assert!(registry.snapshot(&first_id).unwrap().notified);
    assert!(registry.snapshot(&second_id).unwrap().notified);

    assert!(persist_task_notifications_after_parent(
        &registry,
        &poller,
        &store,
        &job.identity.job_id,
        session_id,
        &mut failure_logged,
    ));
    assert_eq!(
        store
            .read_state(&job.identity.job_id)
            .unwrap()
            .identity
            .pending_prompts
            .len(),
        2
    );
}

#[test]
fn failed_task_notification_persistence_keeps_notification_retryable() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let session_id = "session-notification-retry";
    let registry = TaskRegistry::new();
    let task_id = rebon_plugin_tasks::runtime::TaskId::new("agent-notification-retry");
    registry.insert(
        task_id.clone(),
        idle_background_agent_snapshot(task_id.as_str(), "retry", session_id, 1),
        rebon_types::PromptCancel::new(),
    );
    let poller = crate::task_notification_poller::TaskNotificationPoller::new(registry.clone());
    let mut failure_logged = false;

    assert!(!persist_task_notifications_after_parent(
        &registry,
        &poller,
        &store,
        &job.identity.job_id,
        session_id,
        &mut failure_logged,
    ));
    assert!(store
        .read_state(&job.identity.job_id)
        .unwrap()
        .identity
        .pending_prompts
        .is_empty());
    assert!(!registry.snapshot(&task_id).unwrap().notified);
    assert_eq!(
        poller
            .unnotified_notifications_for_session(session_id)
            .len(),
        1
    );
}

#[tokio::test]
async fn task_store_bridge_persists_late_finished_after_parent_stop() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let registry = Arc::new(TaskRegistry::new());
    let task_id = rebon_plugin_tasks::runtime::TaskId::new("agent-late-finish");
    registry.insert(
        task_id.clone(),
        running_local_agent_snapshot(task_id.as_str(), "late finish"),
        rebon_types::PromptCancel::new(),
    );
    registry.record_live_event(&task_id, TaskLiveEventKind::Started);

    let (stop_tx, bridge) = spawn_task_store_bridge(
        Arc::clone(&registry),
        store.clone(),
        job.identity.job_id.clone(),
        "session-late".into(),
    );

    // Let the bridge observe Started while the parent is still "active".
    tokio::time::sleep(Duration::from_millis(TASK_BRIDGE_POLL_INTERVAL_MS * 3)).await;
    // Parent prompt completes — must not await the bridge join.
    stop_tx.send(()).expect("bridge stop receiver alive");

    // Detached worker finishes after parent stop.
    tokio::time::sleep(Duration::from_millis(TASK_BRIDGE_POLL_INTERVAL_MS * 2)).await;
    registry.update(&task_id, |snapshot| {
        snapshot.status = TaskStatus::Completed;
        snapshot.end_time_ms = Some(2);
    });
    registry.record_live_event(
        &task_id,
        TaskLiveEventKind::Finished {
            status: TaskStatus::Completed,
            error: None,
        },
    );

    tokio::time::timeout(Duration::from_secs(2), bridge)
        .await
        .expect("bridge should exit after terminal task")
        .expect("bridge task should not panic");

    let events = store.read_events_tail(&job.identity.job_id, 50).unwrap();
    assert!(
        task_live_batches_contain_finished(&events, task_id.as_str(), "completed"),
        "expected Finished(completed) persisted after parent stop; events={events:?}"
    );
}

#[tokio::test]
async fn task_store_bridge_persists_terminal_checkpoint_without_finished_event() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let registry = Arc::new(TaskRegistry::new());
    let task_id = rebon_plugin_tasks::runtime::TaskId::new("agent-terminal-checkpoint");
    registry.insert(
        task_id.clone(),
        running_local_agent_snapshot(task_id.as_str(), "terminal checkpoint"),
        rebon_types::PromptCancel::new(),
    );
    registry.record_live_event(&task_id, TaskLiveEventKind::Started);
    let blocker_id = rebon_plugin_tasks::runtime::TaskId::new("agent-still-running");
    registry.insert(
        blocker_id.clone(),
        running_local_agent_snapshot(blocker_id.as_str(), "still running"),
        rebon_types::PromptCancel::new(),
    );
    registry.record_live_event(&blocker_id, TaskLiveEventKind::Started);

    let (stop_tx, bridge) = spawn_task_store_bridge(
        Arc::clone(&registry),
        store.clone(),
        job.identity.job_id.clone(),
        "session-checkpoint".into(),
    );
    tokio::time::sleep(Duration::from_millis(TASK_BRIDGE_POLL_INTERVAL_MS * 2)).await;
    stop_tx.send(()).expect("bridge stop receiver alive");

    // Simulate a terminal path (or the update/event race) in which the
    // snapshot becomes terminal but no Finished event is available to the
    // bridge before it decides whether it can close.
    registry.update(&task_id, |snapshot| {
        snapshot.status = TaskStatus::Failed;
        snapshot.error = Some("worker failed before Finished emission".into());
        snapshot.end_time_ms = Some(2);
    });

    tokio::time::sleep(Duration::from_millis(TASK_BRIDGE_POLL_INTERVAL_MS * 3)).await;
    assert!(
        !bridge.is_finished(),
        "sibling running task should keep the post-parent bridge alive"
    );
    let events_while_running = store.read_events_tail(&job.identity.job_id, 50).unwrap();
    assert!(
        task_live_batches_contain_checkpoint(
            &events_while_running,
            task_id.as_str(),
            "failed"
        ),
        "terminal sibling must be checkpointed without waiting for every task; events={events_while_running:?}"
    );

    registry.update(&blocker_id, |snapshot| {
        snapshot.status = TaskStatus::Completed;
        snapshot.end_time_ms = Some(3);
    });
    registry.record_live_event(
        &blocker_id,
        TaskLiveEventKind::Finished {
            status: TaskStatus::Completed,
            error: None,
        },
    );

    tokio::time::timeout(Duration::from_secs(2), bridge)
        .await
        .expect("bridge should exit after every task becomes terminal")
        .expect("bridge task should not panic");

    let events = store.read_events_tail(&job.identity.job_id, 50).unwrap();
    assert!(
        task_live_batches_contain_checkpoint(&events, task_id.as_str(), "failed"),
        "expected terminal checkpoint after missing Finished; events={events:?}"
    );
    assert!(
        !task_live_batches_contain_finished(&events, task_id.as_str(), "failed"),
        "test precondition violated: no Finished event should exist"
    );
}

#[tokio::test]
async fn task_store_bridge_treats_dropped_stop_sender_as_parent_completion() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let registry = Arc::new(TaskRegistry::new());
    let task_id = rebon_plugin_tasks::runtime::TaskId::new("agent-parent-channel-closed");
    registry.insert(
        task_id.clone(),
        running_local_agent_snapshot(task_id.as_str(), "closed parent channel"),
        rebon_types::PromptCancel::new(),
    );
    registry.record_live_event(&task_id, TaskLiveEventKind::Started);

    let (stop_tx, bridge) = spawn_task_store_bridge(
        Arc::clone(&registry),
        store.clone(),
        job.identity.job_id.clone(),
        "session-channel-closed".into(),
    );
    drop(stop_tx);
    tokio::time::sleep(Duration::from_millis(TASK_BRIDGE_POLL_INTERVAL_MS * 3)).await;
    assert!(
        !bridge.is_finished(),
        "closed parent channel must not stop a bridge with a running worker"
    );

    registry.update(&task_id, |snapshot| {
        snapshot.status = TaskStatus::Completed;
        snapshot.end_time_ms = Some(2);
    });
    registry.record_live_event(
        &task_id,
        TaskLiveEventKind::Finished {
            status: TaskStatus::Completed,
            error: None,
        },
    );

    tokio::time::timeout(Duration::from_secs(2), bridge)
        .await
        .expect("bridge should exit after worker terminal state")
        .expect("bridge task should not panic");
    let events = store.read_events_tail(&job.identity.job_id, 50).unwrap();
    assert!(task_live_batches_contain_finished(
        &events,
        task_id.as_str(),
        "completed"
    ));
}

#[tokio::test]
async fn task_store_bridge_exits_promptly_when_all_tasks_terminal_at_parent_stop() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let registry = Arc::new(TaskRegistry::new());
    let task_id = rebon_plugin_tasks::runtime::TaskId::new("agent-already-done");
    let mut snapshot = running_local_agent_snapshot(task_id.as_str(), "already done");
    snapshot.status = TaskStatus::Completed;
    snapshot.end_time_ms = Some(2);
    registry.insert(task_id.clone(), snapshot, rebon_types::PromptCancel::new());
    registry.record_live_event(
        &task_id,
        TaskLiveEventKind::Finished {
            status: TaskStatus::Completed,
            error: None,
        },
    );

    let (stop_tx, bridge) = spawn_task_store_bridge(
        Arc::clone(&registry),
        store.clone(),
        job.identity.job_id.clone(),
        "session-terminal".into(),
    );
    stop_tx.send(()).expect("bridge stop receiver alive");

    tokio::time::timeout(Duration::from_secs(2), bridge)
        .await
        .expect("bridge should exit promptly when tasks are already terminal")
        .expect("bridge task should not panic");

    let events = store.read_events_tail(&job.identity.job_id, 50).unwrap();
    assert!(task_live_batches_contain_checkpoint(
        &events,
        task_id.as_str(),
        "completed"
    ));
}

#[tokio::test]
async fn task_store_bridge_exits_promptly_with_empty_registry_after_parent_stop() {
    let (_dir, store) = store();
    let job = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let registry = Arc::new(TaskRegistry::new());
    let (stop_tx, bridge) = spawn_task_store_bridge(
        registry,
        store.clone(),
        job.identity.job_id.clone(),
        "session-empty".into(),
    );
    stop_tx.send(()).expect("bridge stop receiver alive");

    tokio::time::timeout(Duration::from_secs(2), bridge)
        .await
        .expect("empty registry bridge should exit promptly")
        .expect("bridge task should not panic");
}
