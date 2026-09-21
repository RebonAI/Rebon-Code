use super::*;

/// A descriptor rides along with every live event, so an unbounded agent
/// prompt would be re-encoded into the job event log once per streamed
/// chunk. Cap it at the same budget as tool payloads — but truncate
/// instead of collapsing whitespace the way `shorten_excerpt` does,
/// because this text is rendered as a chat row and seeded as the
/// transcript's first entry, where line structure matters.
pub(crate) fn bounded_task_text(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value;
    }
    let truncated: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

pub(crate) fn bounded_task_prompt(prompt: String) -> String {
    bounded_task_text(prompt, 8_192)
}

pub(crate) const WORKFLOW_PREVIEW_RESULT_MAX_CHARS: usize = 65_536;

pub(crate) fn workflow_descriptor_result(
    data: &rebon_plugin_tasks::runtime::LocalWorkflowData,
) -> serde_json::Value {
    let mut result = serde_json::json!({
        "workflowProgress": rebon_plugin_tasks::workflow_progress::workflow_progress_preview_value(data),
    });
    let original_entries = result["workflowProgress"]["entries"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    while result.to_string().chars().count() > WORKFLOW_PREVIEW_RESULT_MAX_CHARS {
        let Some(entries) = result["workflowProgress"]["entries"].as_array_mut() else {
            break;
        };
        if entries.pop().is_none() {
            break;
        }
    }
    let retained_entries = result["workflowProgress"]["entries"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    if retained_entries < original_entries {
        result["workflowProgress"]["truncated"] = serde_json::Value::Bool(true);
        result["workflowProgress"]["omittedEntries"] =
            serde_json::json!(original_entries - retained_entries);
    }
    result
}

pub(crate) fn background_task_descriptor(snapshot: &TaskSnapshot) -> BackgroundTaskDescriptor {
    let (agent_name, agent_type, model, token_count, tool_use_count, prompt) = match &snapshot.data
    {
        TaskData::LocalAgent(data) => (
            Some(
                snapshot
                    .metadata_str("display_name")
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&data.agent_type)
                    .to_string(),
            ),
            Some(data.agent_type.clone()),
            data.model.clone(),
            Some(data.token_count),
            Some(data.tool_use_count),
            Some(bounded_task_prompt(data.prompt.clone())),
        ),
        TaskData::InProcessTeammate(data) => (
            Some(data.identity.agent_name.clone()),
            snapshot.metadata_str("agent_type").map(str::to_string),
            data.model.clone(),
            Some(data.token_count),
            Some(data.tool_use_count),
            Some(bounded_task_prompt(data.prompt.clone())),
        ),
        _ => (None, None, None, None, None, None),
    };
    let title = bounded_task_text(
        match &snapshot.data {
            TaskData::InProcessTeammate(_) => agent_type
                .clone()
                .filter(|agent_type| !agent_type.trim().is_empty())
                .unwrap_or_else(|| snapshot.title.clone()),
            _ => snapshot.title.clone(),
        },
        512,
    );
    let metadata_str = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| {
                snapshot
                    .metadata
                    .get(*key)
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_string)
    };
    let result = match &snapshot.data {
        TaskData::LocalWorkflow(data) => Some(workflow_descriptor_result(data)),
        _ => snapshot.result.clone().map(bounded_task_json),
    };
    BackgroundTaskDescriptor {
        task_id: snapshot.id.to_string(),
        title,
        kind: snapshot.kind.to_string(),
        status: if snapshot.status == TaskStatus::Running
            && rebon_plugin_tasks::runtime::is_agent_snapshot_idle(snapshot)
        {
            // A worker parked at its turn boundary is not terminal: its task
            // stays Running with the idle flag set (teammates carry it in the
            // data, local agents in `_runtime_is_idle` metadata). The wire
            // must say "idle" so clients that only see descriptors can tell
            // the two apart — "running" here pins the desktop Agent View in
            // Working forever.
            "idle".into()
        } else {
            snapshot.status.to_string()
        },
        is_backgrounded: snapshot.is_backgrounded,
        start_time_ms: snapshot.start_time_ms,
        end_time_ms: snapshot.end_time_ms,
        last_progress: snapshot
            .last_progress
            .clone()
            .map(|progress| bounded_task_text(progress, 2_048)),
        error: snapshot
            .error
            .clone()
            .map(|error| bounded_task_text(error, 2_048)),
        prompt,
        parent_tool_call_id: metadata_str(&["parent_tool_call_id", "parentToolCallId"])
            .map(|id| bounded_task_text(id, 256)),
        agent_id: metadata_str(&["agent_id", "agentId"])
            .map(|id| bounded_task_text(id, 256))
            .or_else(|| {
                matches!(&snapshot.data, TaskData::LocalAgent(_)).then(|| snapshot.id.to_string())
            }),
        agent_name: agent_name.map(|name| bounded_task_text(name, 512)),
        agent_type: agent_type.map(|agent_type| bounded_task_text(agent_type, 128)),
        model: model.map(|model| bounded_task_text(model, 512)),
        token_count,
        tool_use_count,
        result,
    }
}

pub(crate) fn bounded_task_json(value: serde_json::Value) -> serde_json::Value {
    bounded_task_json_with_limit(value, 8_192)
}

pub(crate) fn bounded_task_json_with_limit(
    value: serde_json::Value,
    max_chars: usize,
) -> serde_json::Value {
    let encoded = value.to_string();
    if encoded.chars().count() <= max_chars {
        value
    } else {
        serde_json::Value::String(shorten_excerpt(&encoded, max_chars))
    }
}

pub(crate) fn background_task_event(
    registry: &TaskRegistry,
    event: TaskLiveEvent,
) -> BackgroundTaskEvent {
    let task = registry
        .snapshot(&event.task_id)
        .as_ref()
        .map(background_task_descriptor);
    let kind = match event.kind {
        TaskLiveEventKind::Started => BackgroundTaskEventKind::Started,
        TaskLiveEventKind::UserMessage { text } => BackgroundTaskEventKind::UserMessage { text },
        TaskLiveEventKind::AssistantTextDelta { delta, snapshot } => {
            BackgroundTaskEventKind::AssistantTextDelta { delta, snapshot }
        }
        TaskLiveEventKind::ThinkingDelta { delta, snapshot } => {
            BackgroundTaskEventKind::ThinkingDelta { delta, snapshot }
        }
        TaskLiveEventKind::ThinkingEnd => BackgroundTaskEventKind::ThinkingEnd,
        TaskLiveEventKind::AssistantTurnComplete { text } => {
            BackgroundTaskEventKind::AssistantTurnComplete { text }
        }
        TaskLiveEventKind::ToolStart {
            tool_use_id,
            name,
            input,
        } => BackgroundTaskEventKind::ToolStart {
            tool_use_id,
            name,
            input: bounded_task_json(input),
        },
        TaskLiveEventKind::ToolProgress {
            tool_use_id,
            name,
            message,
        } => BackgroundTaskEventKind::ToolProgress {
            tool_use_id,
            name,
            message,
        },
        TaskLiveEventKind::ToolFinish {
            tool_use_id,
            name,
            outcome,
        } => match outcome {
            Ok(output) => BackgroundTaskEventKind::ToolFinish {
                tool_use_id,
                name,
                output: Some(bounded_task_json(output)),
                error: None,
            },
            Err(error) => BackgroundTaskEventKind::ToolFinish {
                tool_use_id,
                name,
                output: None,
                error: Some(error),
            },
        },
        TaskLiveEventKind::TerminalOutput { stream, chunk } => {
            BackgroundTaskEventKind::TerminalOutput {
                stream: match stream {
                    TaskTerminalStream::Stdout => String::from("stdout"),
                    TaskTerminalStream::Stderr => String::from("stderr"),
                },
                chunk,
            }
        }
        TaskLiveEventKind::Finished { status, error } => BackgroundTaskEventKind::Finished {
            status: status.to_string(),
            error,
        },
    };
    BackgroundTaskEvent {
        cursor: event.cursor.get(),
        task_id: event.task_id.to_string(),
        timestamp_ms: event.timestamp_ms,
        task,
        event: kind,
    }
}

pub(crate) fn coalesce_task_events(events: Vec<BackgroundTaskEvent>) -> Vec<BackgroundTaskEvent> {
    let mut coalesced: Vec<BackgroundTaskEvent> = Vec::with_capacity(events.len());
    for event in events {
        let merged = coalesced.last_mut().is_some_and(|previous| {
            if previous.task_id != event.task_id {
                return false;
            }
            match (&mut previous.event, &event.event) {
                (
                    BackgroundTaskEventKind::AssistantTextDelta { delta, snapshot },
                    BackgroundTaskEventKind::AssistantTextDelta {
                        delta: next_delta,
                        snapshot: next_snapshot,
                    },
                ) => {
                    delta.push_str(next_delta);
                    *snapshot = next_snapshot.clone();
                    previous.cursor = event.cursor;
                    previous.timestamp_ms = event.timestamp_ms;
                    previous.task = event.task.clone();
                    true
                }
                (
                    BackgroundTaskEventKind::ThinkingDelta { delta, snapshot },
                    BackgroundTaskEventKind::ThinkingDelta {
                        delta: next_delta,
                        snapshot: next_snapshot,
                    },
                ) => {
                    delta.push_str(next_delta);
                    *snapshot = next_snapshot.clone();
                    previous.cursor = event.cursor;
                    previous.timestamp_ms = event.timestamp_ms;
                    previous.task = event.task.clone();
                    true
                }
                _ => false,
            }
        });
        if !merged {
            coalesced.push(event);
        }
    }
    coalesced
}

/// Drain live registry events into the job event log, advancing `cursor`
/// only after a successful durable append. Returns `false` when the append
/// failed so callers do not treat terminal in-memory state as persisted.
pub(crate) fn flush_task_store_bridge_batch(
    registry: &TaskRegistry,
    store: &BackgroundStore,
    job_id: &str,
    stream_id: &str,
    cursor: &mut TaskEventCursor,
    append_failure_logged: &mut bool,
) -> bool {
    let from_cursor = *cursor;
    let batch = registry.session_live_events(Some(*cursor));
    let next_cursor = batch.next_cursor;
    if batch.cursor_was_stale || !batch.events.is_empty() {
        let reset_tasks = if batch.cursor_was_stale {
            registry
                .snapshots()
                .iter()
                .map(background_task_descriptor)
                .collect()
        } else {
            Vec::new()
        };
        let events = coalesce_task_events(
            batch
                .events
                .into_iter()
                .map(|event| background_task_event(registry, event))
                .collect(),
        );
        let persisted = BackgroundTaskEventBatch {
            schema: 1,
            stream_id: stream_id.to_string(),
            from_cursor: from_cursor.get(),
            through_cursor: next_cursor.get(),
            cursor_was_stale: batch.cursor_was_stale,
            reset_tasks,
            events,
        };
        // Advance the cursor ONLY after a durable append. On failure keep
        // the old cursor so the next tick re-reads the same events instead
        // of dropping them forever. If the bounded registry journal evicts
        // them before we succeed, the retry read returns `cursor_was_stale`
        // and the snapshot resync above rebuilds state.
        match store.append_task_event_batch(job_id, &persisted) {
            Ok(()) => {
                *cursor = next_cursor;
                *append_failure_logged = false;
                true
            }
            Err(err) => {
                if !*append_failure_logged {
                    tracing::warn!(
                        target: "background_tasks",
                        job_id = %job_id,
                        error = %err,
                        "failed to persist task event batch; keeping cursor and retrying"
                    );
                    *append_failure_logged = true;
                }
                false
            }
        }
    } else {
        *cursor = next_cursor;
        true
    }
}

/// Persist a full registry checkpoint before the post-parent bridge exits.
///
/// A worker updates its snapshot and then records `Finished` in two adjacent
/// operations. A bridge running on another runtime thread can observe the
/// terminal snapshot in that tiny gap. The checkpoint makes the terminal
/// snapshot itself durable even if the journal event lands just after the
/// bridge's final drain (and also covers terminal paths that emit no event).
pub(crate) fn persist_task_store_bridge_checkpoint(
    store: &BackgroundStore,
    job_id: &str,
    stream_id: &str,
    cursor: TaskEventCursor,
    snapshots: &[TaskSnapshot],
    append_failure_logged: &mut bool,
) -> bool {
    if snapshots.is_empty() {
        return true;
    }
    let checkpoint = BackgroundTaskEventBatch {
        schema: 1,
        stream_id: stream_id.to_string(),
        from_cursor: cursor.get(),
        through_cursor: cursor.get(),
        // `reset_tasks` is a complete state checkpoint. Mark it as a resync
        // batch so durable readers prefer these snapshots over earlier event
        // projections (for example a previously persisted Started event).
        cursor_was_stale: true,
        reset_tasks: snapshots.iter().map(background_task_descriptor).collect(),
        events: Vec::new(),
    };
    match store.append_task_event_batch(job_id, &checkpoint) {
        Ok(()) => {
            *append_failure_logged = false;
            true
        }
        Err(err) => {
            if !*append_failure_logged {
                tracing::warn!(
                    target: "background_tasks",
                    job_id = %job_id,
                    error = %err,
                    "failed to persist terminal task checkpoint; retrying"
                );
                *append_failure_logged = true;
            }
            false
        }
    }
}

/// Whether a task has stopped producing work of its own accord.
///
/// Terminal is the obvious case. A worker parked at a turn boundary is the
/// other one: `keep_runtime_resumable` deliberately holds its snapshot at
/// `Running` so the persistent actor can take a follow-up, so it never turns
/// terminal on its own. Waiting for that is waiting forever — the bridge
/// would only ever exit through its post-parent timeout, pinning the worker
/// process and its session for the whole hour. A follow-up `SendMessage`
/// restarts the bridge via `ensure_task_store_bridge_after_activity`, so
/// leaving a parked agent behind loses nothing.
pub(crate) fn task_is_settled(snapshot: &TaskSnapshot) -> bool {
    snapshot.status.is_terminal() || rebon_plugin_tasks::runtime::is_agent_snapshot_idle(snapshot)
}

pub(crate) fn terminal_task_signature(snapshots: &[TaskSnapshot]) -> Vec<(String, String)> {
    let mut signature = snapshots
        .iter()
        .filter(|snapshot| snapshot.status.is_terminal())
        .map(|snapshot| (snapshot.id.to_string(), snapshot.status.to_string()))
        .collect::<Vec<_>>();
    signature.sort_unstable();
    signature
}

pub(crate) fn task_notification_pending_prompt_id(notification: &TaskNotification) -> String {
    let hash = notification
        .task_id
        .as_str()
        .bytes()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    format!(
        "u-internal-task-notification-{:016x}-{hash:016x}",
        notification.generation
    )
}

pub(crate) fn persist_task_notifications_after_parent(
    registry: &TaskRegistry,
    notification_poller: &crate::task_notification_poller::TaskNotificationPoller,
    store: &BackgroundStore,
    job_id: &str,
    session_id: &str,
    append_failure_logged: &mut bool,
) -> bool {
    let notifications = notification_poller.unnotified_notifications_for_session(session_id);
    if notifications.is_empty() {
        *append_failure_logged = false;
        return true;
    }

    let rebon_exe = rebon_exe();
    let mut delivered = Vec::new();
    let mut all_persisted = true;
    for notification in notifications {
        let prompt_id = task_notification_pending_prompt_id(&notification);
        let coordinator_report_paths = notification.output_file.into_iter().collect();
        match rebon_session_host::append_background_internal_prompt(
            store,
            job_id,
            prompt_id,
            notification.message,
            coordinator_report_paths,
            true,
            &rebon_exe,
        ) {
            Ok(_) => delivered.push((notification.task_id, notification.generation)),
            Err(error) => {
                if !*append_failure_logged {
                    tracing::warn!(
                        target: "background_tasks",
                        job_id = %job_id,
                        task_id = %notification.task_id,
                        generation = notification.generation,
                        %error,
                        "failed to persist task notification follow-up; keeping it retryable"
                    );
                    *append_failure_logged = true;
                }
                all_persisted = false;
            }
        }
    }
    registry.mark_notification_generations_delivered(&delivered);
    if all_persisted {
        *append_failure_logged = false;
    }
    all_persisted
}

// Keeps the last durable cursor and fences bridge exit against concurrent IPC activity.
#[derive(Clone)]
pub(crate) struct BackgroundTaskBridgeState {
    inner: Arc<Mutex<BackgroundTaskBridgeStateInner>>,
}

pub(crate) struct BackgroundTaskBridgeStateInner {
    running: bool,
    cursor: TaskEventCursor,
    generation: u64,
}

impl BackgroundTaskBridgeState {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BackgroundTaskBridgeStateInner {
                running: false,
                cursor: TaskEventCursor::ZERO,
                generation: 0,
            })),
        }
    }

    pub(crate) fn begin(&self, initial_cursor: TaskEventCursor) -> u64 {
        let mut inner = self.inner.lock().expect("task bridge state poisoned");
        inner.running = true;
        inner.cursor = initial_cursor;
        inner.generation
    }

    pub(crate) fn is_running(&self) -> bool {
        self.inner
            .lock()
            .expect("task bridge state poisoned")
            .running
    }

    pub(crate) fn claim_after_activity(&self) -> Option<TaskEventCursor> {
        let mut inner = self.inner.lock().expect("task bridge state poisoned");
        inner.generation = inner.generation.wrapping_add(1);
        if inner.running {
            None
        } else {
            inner.running = true;
            Some(inner.cursor)
        }
    }

    pub(crate) fn record_persisted_cursor(&self, cursor: TaskEventCursor) {
        self.inner
            .lock()
            .expect("task bridge state poisoned")
            .cursor = cursor;
    }

    pub(crate) fn finish_if_current(&self, generation: u64) -> Result<(), u64> {
        let mut inner = self.inner.lock().expect("task bridge state poisoned");
        if inner.generation != generation {
            return Err(inner.generation);
        }
        inner.running = false;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn spawn_task_store_bridge(
    registry: Arc<TaskRegistry>,
    store: BackgroundStore,
    job_id: String,
    session_id: String,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    spawn_task_store_bridge_from_cursor(
        registry,
        store,
        job_id,
        session_id,
        TaskEventCursor::ZERO,
        None,
    )
}

pub(crate) fn spawn_task_store_bridge_from_cursor(
    registry: Arc<TaskRegistry>,
    store: BackgroundStore,
    job_id: String,
    session_id: String,
    initial_cursor: TaskEventCursor,
    task_bridge_state: Option<BackgroundTaskBridgeState>,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let mut bridge_generation = task_bridge_state
        .as_ref()
        .map(|state| state.begin(initial_cursor));
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
    let notification_poller =
        crate::task_notification_poller::TaskNotificationPoller::new(registry.as_ref().clone());
    let bridge_future = async move {
        let stream_id = format!("{session_id}/{}", now_ms());
        let mut cursor = initial_cursor;
        let mut interval =
            tokio::time::interval(Duration::from_millis(TASK_BRIDGE_POLL_INTERVAL_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Whether the most recent append failed, so we warn once per failure
        // streak instead of on every poll tick.
        let mut append_failure_logged = false;
        let mut notification_failure_logged = false;
        // Parent-prompt stop means "parent finished", not "exit immediately".
        // Detached tasks may still be Running and will emit Finished later.
        let mut parent_done = false;
        let mut parent_done_at: Option<tokio::time::Instant> = None;
        // Require one cursor-stable poll after every task first appears
        // settled. Besides catching a concurrently recorded Finished event,
        // this avoids treating an empty registry as final in the same instant
        // the parent completion signal is received.
        let mut settled_cursor: Option<TaskEventCursor> = None;
        // Terminal snapshots are checkpointed as soon as they appear after
        // parent completion, even while sibling tasks are still running.
        let mut checkpointed_terminal_tasks: Vec<(String, String)> = Vec::new();
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = &mut stop_rx, if !parent_done => {
                    parent_done = true;
                    parent_done_at = Some(tokio::time::Instant::now());
                }
            }
            let flush_persisted = flush_task_store_bridge_batch(
                &registry,
                &store,
                &job_id,
                &stream_id,
                &mut cursor,
                &mut append_failure_logged,
            );
            if flush_persisted {
                if let Some(state) = task_bridge_state.as_ref() {
                    state.record_persisted_cursor(cursor);
                }
            }
            if !parent_done {
                continue;
            }

            let notifications_persisted = persist_task_notifications_after_parent(
                &registry,
                &notification_poller,
                &store,
                &job_id,
                &session_id,
                &mut notification_failure_logged,
            );
            let snapshots = registry.snapshots();
            let terminal_tasks = terminal_task_signature(&snapshots);
            let checkpoint_persisted =
                if flush_persisted && terminal_tasks != checkpointed_terminal_tasks {
                    if persist_task_store_bridge_checkpoint(
                        &store,
                        &job_id,
                        &stream_id,
                        cursor,
                        &snapshots,
                        &mut append_failure_logged,
                    ) {
                        checkpointed_terminal_tasks = terminal_tasks.clone();
                        true
                    } else {
                        false
                    }
                } else {
                    flush_persisted && terminal_tasks == checkpointed_terminal_tasks
                };

            let settled_ready = if snapshots.iter().any(|snapshot| !task_is_settled(snapshot))
                || !checkpoint_persisted
                || !notifications_persisted
            {
                settled_cursor = None;
                false
            } else if settled_cursor == Some(cursor) {
                true
            } else {
                settled_cursor = Some(cursor);
                false
            };

            let timed_out = parent_done_at.is_some_and(|started| {
                started.elapsed() >= Duration::from_millis(TASK_BRIDGE_POST_PARENT_MAX_MS)
            });
            if timed_out {
                tracing::warn!(
                    target: "background_tasks",
                    job_id = %job_id,
                    "task store bridge timed out waiting for settled tasks or a durable checkpoint"
                );
            }
            if settled_ready || timed_out {
                match (task_bridge_state.as_ref(), bridge_generation.as_mut()) {
                    (Some(state), Some(generation)) => match state.finish_if_current(*generation) {
                        Ok(()) => break,
                        Err(current_generation) => {
                            *generation = current_generation;
                            settled_cursor = None;
                            parent_done_at = Some(tokio::time::Instant::now());
                        }
                    },
                    _ => break,
                }
            }
        }
    };
    let handle = tokio::spawn(bridge_future);
    (stop_tx, handle)
}
