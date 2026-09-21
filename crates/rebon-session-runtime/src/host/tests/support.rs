use super::super::*;
use rebon_session_host::BackgroundJobEvent;

pub(super) fn store() -> (tempfile::TempDir, BackgroundStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = BackgroundStore::new(dir.path());
    (dir, store)
}

pub(super) fn runtime() -> BackgroundRuntimeFields {
    BackgroundRuntimeFields {
        provider: None,
        model: None,
        fast_mode: None,
        channels: Vec::new(),
        development_channels: Vec::new(),
        provider_format: None,
        ui_mode: None,
        effort_level: None,
        permission_mode: None,
        capability_mode: rebon_types::AgentCapabilityMode::Normal,
        settings: Vec::new(),
        add_dirs: Vec::new(),
        plugin_dirs: Vec::new(),
        mcp_configs: Vec::new(),
        strict_mcp_config: false,
    }
}

pub(super) fn pending_prompt(
    id: &str,
    text: &str,
    images: Vec<BackgroundImageAttachment>,
) -> PendingPrompt {
    PendingPrompt::new(id.into(), text.into(), images, now_ms()).unwrap()
}

pub(super) fn pending_text(state: &BackgroundJobState) -> Option<&str> {
    state.pending_prompt().map(|prompt| prompt.text.as_str())
}

pub(super) fn install_ipc_owner(state: &mut BackgroundJobState, ipc: &BackgroundIpcServer) {
    let owner = ipc.owner();
    state.process.pid = Some(owner.endpoint.pid);
    state.process.pid_identity = owner.pid_identity;
    state.process.ipc_port = Some(owner.endpoint.port);
    state.process.ipc_token = Some(owner.endpoint.token);
}

pub(super) fn running_local_agent_snapshot(id: &str, title: &str) -> TaskSnapshot {
    TaskSnapshot {
        id: rebon_plugin_tasks::runtime::TaskId::new(id),
        kind: rebon_plugin_tasks::runtime::TaskKind::LocalAgent,
        status: TaskStatus::Running,
        title: title.into(),
        last_progress: None,
        error: None,
        result: None,
        is_backgrounded: false,
        notified: false,
        start_time_ms: 1,
        end_time_ms: None,
        metadata: serde_json::json!({}),
        data: rebon_plugin_tasks::runtime::TaskData::LocalAgent(
            rebon_plugin_tasks::runtime::LocalAgentData {
                prompt: "prompt".into(),
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
            },
        ),
    }
}

/// Assertions over the live task batches a bridge publishes.
///
/// Shared because both halves of the task-bridge split read them: the store
/// bridge's own tests and the ones a worker turn drives.
pub(super) fn task_live_batches_contain_user_message(
    events: &[BackgroundJobEvent],
    task_id: &str,
    message: &str,
) -> bool {
    events.iter().any(|event| {
        if event.kind != "task_live_batch" {
            return false;
        }
        let Ok(batch) = serde_json::from_value::<BackgroundTaskEventBatch>(event.data.clone())
        else {
            return false;
        };
        batch.events.iter().any(|item| {
            item.task_id == task_id
                && matches!(
                    &item.event,
                    BackgroundTaskEventKind::UserMessage { text } if text == message
                )
        })
    })
}

pub(super) fn task_live_batches_contain_finished(
    events: &[BackgroundJobEvent],
    task_id: &str,
    status: &str,
) -> bool {
    events.iter().any(|event| {
        if event.kind != "task_live_batch" {
            return false;
        }
        let Ok(batch) = serde_json::from_value::<BackgroundTaskEventBatch>(event.data.clone())
        else {
            return false;
        };
        batch.events.iter().any(|item| {
            item.task_id == task_id
                && matches!(
                    &item.event,
                    BackgroundTaskEventKind::Finished {
                        status: finished_status,
                        ..
                    } if finished_status == status
                )
        })
    })
}

pub(super) fn task_live_batches_contain_checkpoint(
    events: &[BackgroundJobEvent],
    task_id: &str,
    status: &str,
) -> bool {
    events.iter().any(|event| {
        if event.kind != "task_live_batch" {
            return false;
        }
        let Ok(batch) = serde_json::from_value::<BackgroundTaskEventBatch>(event.data.clone())
        else {
            return false;
        };
        batch.cursor_was_stale
            && batch
                .reset_tasks
                .iter()
                .any(|task| task.task_id == task_id && task.status == status)
    })
}
