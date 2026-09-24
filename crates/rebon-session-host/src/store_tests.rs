//! The store transactions, exercised against real temp directories.
//!
//! Lifted out of store.rs so neither file is unreadable. Declared
//! with #[path] from store.rs, so super:: still resolves to crate::store and
//! every one of these tests reaches the same private items it always did.

use std::collections::HashSet;

use super::*;

/// Deliberately unspawnable. This used to be the bare name `rebon`,
/// which is NOT inert: the OS resolves a bare name against the
/// working directory and PATH, and a test binary runs out of
/// `target/debug/deps` — where a `rebon.exe` build artifact sits.
/// Every test that reached the spawn path was starting real
/// processes. Keep a path shape here so `resolve_supervisor_exe`
/// rejects it outright.
fn rebon_exe() -> PathBuf {
    PathBuf::from("./__rebon-test-supervisor-must-not-spawn__")
}

fn allow_all(_: &BackgroundRuntimeFields) -> anyhow::Result<()> {
    Ok(())
}

/// Seed a fresh supervisor roster owned by the current (live) process so
/// `ensure_supervisor_running` short-circuits without spawning a child
/// from `rebon_exe_path` (which is intentionally unspawnable in tests).
fn seed_live_supervisor(store: &BackgroundStore) {
    store
        .write_roster(&BackgroundRoster {
            supervisor_pid: std::process::id(),
            supervisor_pid_identity: process_identity(std::process::id()),
            updated_at_ms: now_ms(),
            jobs: Vec::new(),
        })
        .unwrap();
}

fn store() -> (tempfile::TempDir, BackgroundStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = BackgroundStore::new(dir.path());
    (dir, store)
}

/// Both spellings of every kind, and `None` for anything else — the caller
/// decides what an unknown kind means, so the table must not guess one.
#[test]
fn permission_option_kinds_read_both_spellings_and_nothing_else() {
    use rebon_proto::PermissionOptionKind as Kind;
    for (spelling, kind) in [
        ("AllowOnce", Kind::AllowOnce),
        ("allow_once", Kind::AllowOnce),
        ("AllowAlways", Kind::AllowAlways),
        ("allow_always", Kind::AllowAlways),
        ("RejectOnce", Kind::RejectOnce),
        ("reject_once", Kind::RejectOnce),
        ("RejectAlways", Kind::RejectAlways),
        ("reject_always", Kind::RejectAlways),
    ] {
        assert_eq!(
            parse_permission_option_kind(spelling),
            Some(kind),
            "{spelling}"
        );
    }
    for unknown in ["ALLOW_ALWAYS", "allow-always", "allowalways", "", "grant"] {
        assert_eq!(parse_permission_option_kind(unknown), None, "{unknown:?}");
    }
}

#[test]
fn ipc_token_is_thirty_two_bytes_of_lowercase_hex() {
    let token = generate_ipc_token().expect("operating system entropy is available");

    assert_eq!(token.len(), 64);
    assert!(token
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
}

/// A state file from a build that predates the field still reads. Every
/// installed worker writes one of these, and a client that cannot read them
/// loses every running job the moment it updates.
#[test]
fn a_state_file_without_retry_still_reads() {
    let (_dir, store) = store();
    let job = store
        .create_job("hello".into(), PathBuf::from("."), runtime())
        .unwrap();

    let path = store.job_dir(&job.identity.job_id).join("state.json");
    let mut raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        raw.as_object_mut().unwrap().remove("retry").is_none(),
        "absent by default, so an older reader sees the file it always saw"
    );
    std::fs::write(&path, serde_json::to_string(&raw).unwrap()).unwrap();

    let reloaded = store.read_state(&job.identity.job_id).unwrap();
    assert_eq!(reloaded.outcome.retry, None);
}

#[test]
fn retry_progress_survives_a_round_trip() {
    let (_dir, store) = store();
    let job = store
        .create_job("hello".into(), PathBuf::from("."), runtime())
        .unwrap();

    store
        .update_state(&job.identity.job_id, |state| {
            state.outcome.retry = Some(BackgroundRetryProgress {
                attempt: 2,
                max_retries: 10,
            });
            Ok(())
        })
        .unwrap();

    let reloaded = store.read_state(&job.identity.job_id).unwrap();
    let retry = reloaded.outcome.retry.expect("published");
    assert_eq!(retry.attempt, 2);
    assert_eq!(retry.max_retries, 10);
    assert_eq!(retry.to_string(), "Retry 2/10", "what the UI renders");
}

#[test]
fn create_job_accepts_short_cjk_prompts() {
    let (_dir, store) = store();
    let job = store
        .create_job("你好".into(), PathBuf::from("."), runtime())
        .unwrap();
    assert_eq!(job.identity.prompt, "你好");
}

#[test]
fn create_job_rejects_whitespace_only_prompts() {
    let (_dir, store) = store();
    let error = store
        .create_job("   \n".into(), PathBuf::from("."), runtime())
        .unwrap_err();
    assert!(error.to_string().contains("empty"));
}

fn runtime() -> BackgroundRuntimeFields {
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

fn pending_prompt(id: &str, text: &str, images: Vec<BackgroundImageAttachment>) -> PendingPrompt {
    PendingPrompt::new(id.into(), text.into(), images, now_ms()).unwrap()
}

fn pending_text(state: &BackgroundJobState) -> Option<&str> {
    state.pending_prompt().map(|prompt| prompt.text.as_str())
}

fn fenced_failed_session(store: &BackgroundStore, session_id: &str) -> BackgroundJobState {
    let mut state = store
        .create_job("original prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some(session_id.into());
    state.process.status = BackgroundJobStatus::Failed;
    state.process.pid = Some(std::process::id());
    state.process.pid_identity = process_identity(std::process::id());
    state.process.process_owner_fenced = true;
    state.identity.pending_prompts = vec![pending_prompt(
        "pp-fenced-failed",
        "accepted follow-up",
        vec![BackgroundImageAttachment {
            id: 7,
            data: "accepted-image".into(),
            media_type: "image/png".into(),
            filename: None,
            source_path: None,
        }],
    )];
    state.outcome.error = Some("worker exit could not be verified".into());
    store.write_state(&state).unwrap();
    store.read_state(&state.identity.job_id).unwrap()
}

#[test]
fn new_jobs_capture_the_launchers_path() {
    let state = BackgroundJobState::new("prompt".into(), ".".into(), runtime(), None);

    assert_eq!(state.process.process_path, std::env::var("PATH").ok());
}

#[test]
fn legacy_jobs_without_a_process_path_still_deserialize() {
    let state = BackgroundJobState::new("prompt".into(), ".".into(), runtime(), None);
    let mut encoded = serde_json::to_value(state).unwrap();
    encoded.as_object_mut().unwrap().remove("processPath");

    let decoded: BackgroundJobState = serde_json::from_value(encoded).unwrap();

    assert_eq!(decoded.process.process_path, None);
}

/// The creating process is the one that knows which Node runtime was
/// vetted; the worker that runs the session is a different process.
#[test]
fn new_jobs_capture_the_resolved_node_runtime() {
    let state = BackgroundJobState::new("prompt".into(), ".".into(), runtime(), None);

    assert_eq!(
        state.process.node_runtime_path,
        std::env::var(rebon_node_runtime::NODE_EXECUTABLE_ENV)
            .ok()
            .filter(|path| !path.is_empty())
    );
}

#[test]
fn legacy_jobs_without_a_node_runtime_still_deserialize() {
    let state = BackgroundJobState::new("prompt".into(), ".".into(), runtime(), None);
    let mut encoded = serde_json::to_value(state).unwrap();
    encoded.as_object_mut().unwrap().remove("nodeRuntimePath");

    let decoded: BackgroundJobState = serde_json::from_value(encoded).unwrap();

    assert_eq!(decoded.process.node_runtime_path, None);
}

/// The field is round-tripped under the camelCase name the rest of the
/// state file uses, and is omitted entirely when there is nothing to say.
#[test]
fn a_recorded_node_runtime_round_trips_and_an_absent_one_is_not_written() {
    let mut state = BackgroundJobState::new("prompt".into(), ".".into(), runtime(), None);
    state.process.node_runtime_path = Some("/opt/node/bin/node".into());
    let encoded = serde_json::to_value(&state).unwrap();
    assert_eq!(encoded["nodeRuntimePath"], "/opt/node/bin/node");
    let decoded: BackgroundJobState = serde_json::from_value(encoded).unwrap();
    assert_eq!(
        decoded.process.node_runtime_path.as_deref(),
        Some("/opt/node/bin/node")
    );

    state.process.node_runtime_path = None;
    let encoded = serde_json::to_value(&state).unwrap();
    assert!(encoded.get("nodeRuntimePath").is_none());
}

#[test]
fn runtime_capability_mode_round_trips_and_legacy_defaults_to_normal() {
    let mut fields = runtime();
    fields.capability_mode = rebon_types::AgentCapabilityMode::Minimal;

    let encoded = serde_json::to_value(&fields).unwrap();
    assert_eq!(encoded["capabilityMode"], "minimal");
    assert_eq!(
        serde_json::from_value::<BackgroundRuntimeFields>(encoded).unwrap(),
        fields
    );

    let normal = serde_json::to_value(runtime()).unwrap();
    assert!(normal.get("capabilityMode").is_none());
    assert_eq!(
        serde_json::from_value::<BackgroundRuntimeFields>(normal)
            .unwrap()
            .capability_mode,
        rebon_types::AgentCapabilityMode::Normal
    );
}

#[test]
fn runtime_fast_mode_serializes_camel_case_and_round_trips() {
    let mut fields = runtime();
    fields.fast_mode = Some(false);

    let encoded = serde_json::to_value(&fields).unwrap();
    assert_eq!(encoded["fastMode"], false);
    assert!(encoded.get("fast_mode").is_none());
    assert_eq!(
        serde_json::from_value::<BackgroundRuntimeFields>(encoded).unwrap(),
        fields
    );

    let legacy = serde_json::to_value(runtime()).unwrap();
    assert!(legacy.get("fastMode").is_none());
    assert_eq!(
        serde_json::from_value::<BackgroundRuntimeFields>(legacy)
            .unwrap()
            .fast_mode,
        None
    );
}

#[test]
fn permission_snapshot_tool_input_is_backward_compatible() {
    let legacy = serde_json::json!({
        "queryId": 1,
        "tool": "Bash",
        "toolCallId": "tool-1",
        "sessionId": "session-1",
        "options": []
    });
    let snapshot: BackgroundPermissionQuerySnapshot = serde_json::from_value(legacy).unwrap();
    assert_eq!(snapshot.endpoint, None);
    assert_eq!(snapshot.title, None);
    assert_eq!(snapshot.tool_input, None);
    assert_eq!(snapshot.metadata, None);

    let mut snapshot = snapshot;
    snapshot.tool_input = Some(serde_json::json!({"command":"cargo test"}));
    let encoded = serde_json::to_value(snapshot).unwrap();
    assert_eq!(encoded["toolInput"]["command"], "cargo test");
}

#[test]
fn question_answers_become_structured_updated_input() {
    let snapshot = BackgroundPermissionQuerySnapshot {
        query_id: 9,
        turn_generation: 0,
        endpoint: None,
        tool: Some("AskUserQuestion".into()),
        tool_call_id: Some("tool-1".into()),
        session_id: Some("session-1".into()),
        title: Some("Answer questions".into()),
        message: None,
        tool_input: Some(serde_json::json!({
            "questions": [{
                "header": "Mode",
                "question": "Choose a mode",
                "options": [
                    {"label": "Fast", "description": "Finish quickly", "preview": "fast-preview"},
                    {"label": "Safe", "description": "Check everything"}
                ]
            }]
        })),
        metadata: None,
        options: vec![BackgroundPermissionOptionSnapshot {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: "AllowOnce".into(),
        }],
    };

    let updated_input = build_ask_user_question_updated_input(
        &snapshot,
        &[ForegroundQuestionAnswer {
            selected_options: vec![0],
            other_text: Some("ship it".into()),
        }],
    )
    .unwrap();

    assert_eq!(updated_input["answers"]["Choose a mode"], "Fast");
    assert_eq!(
        updated_input["annotations"]["Choose a mode"]["preview"],
        "fast-preview"
    );
    assert_eq!(
        updated_input["annotations"]["Choose a mode"]["notes"],
        "ship it"
    );
}

#[test]
fn resumable_finished_event_keeps_background_projection_running() {
    let descriptor = BackgroundTaskDescriptor {
        task_id: "agent-resumable".into(),
        title: "Resumable agent".into(),
        kind: "local_agent".into(),
        status: "running".into(),
        is_backgrounded: true,
        start_time_ms: 1,
        end_time_ms: None,
        last_progress: Some("first turn done".into()),
        error: None,
        prompt: None,
        parent_tool_call_id: None,
        agent_id: Some("agent-resumable".into()),
        agent_name: None,
        agent_type: Some("Explore".into()),
        model: Some("mock".into()),
        token_count: Some(10),
        tool_use_count: Some(0),
        result: None,
    };
    let batch = BackgroundTaskEventBatch {
        schema: 1,
        stream_id: "session/run".into(),
        from_cursor: 0,
        through_cursor: 1,
        cursor_was_stale: false,
        reset_tasks: Vec::new(),
        events: vec![BackgroundTaskEvent {
            cursor: 1,
            task_id: "agent-resumable".into(),
            timestamp_ms: 20,
            task: Some(descriptor),
            event: BackgroundTaskEventKind::Finished {
                status: "running".into(),
                error: None,
            },
        }],
    };
    let snapshots = project_background_task_snapshots(&[BackgroundJobEvent {
        timestamp_ms: 20,
        kind: "task_live_batch".into(),
        data: serde_json::to_value(batch).unwrap(),
    }]);

    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].task.status, "running");
    assert_eq!(snapshots[0].task.end_time_ms, None);
}

#[test]
fn resumable_finished_event_keeps_idle_descriptor_idle() {
    // The bridge's descriptor says "idle" for a worker parked at its turn
    // boundary; the Finished event for the same turn says "running"
    // because the task is not terminal. The projection must not flatten
    // the parked worker back to "running" — that is exactly how a
    // finished agent shows as working forever.
    let descriptor = BackgroundTaskDescriptor {
        task_id: "agent-idle".into(),
        title: "Idle agent".into(),
        kind: "local_agent".into(),
        status: "idle".into(),
        is_backgrounded: true,
        start_time_ms: 1,
        end_time_ms: None,
        last_progress: Some("turn done".into()),
        error: None,
        prompt: None,
        parent_tool_call_id: None,
        agent_id: Some("agent-idle".into()),
        agent_name: None,
        agent_type: Some("verification".into()),
        model: Some("mock".into()),
        token_count: Some(10),
        tool_use_count: Some(0),
        result: None,
    };
    let batch = BackgroundTaskEventBatch {
        schema: 1,
        stream_id: "session/run".into(),
        from_cursor: 0,
        through_cursor: 1,
        cursor_was_stale: false,
        reset_tasks: Vec::new(),
        events: vec![BackgroundTaskEvent {
            cursor: 1,
            task_id: "agent-idle".into(),
            timestamp_ms: 20,
            task: Some(descriptor),
            event: BackgroundTaskEventKind::Finished {
                status: "running".into(),
                error: None,
            },
        }],
    };
    let snapshots = project_background_task_snapshots(&[BackgroundJobEvent {
        timestamp_ms: 20,
        kind: "task_live_batch".into(),
        data: serde_json::to_value(batch).unwrap(),
    }]);

    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].task.status, "idle");
    assert_eq!(snapshots[0].task.end_time_ms, None);
}

#[test]
fn task_reply_ipc_has_a_stable_wire_shape() {
    let request = BackgroundIpcRequest::ReplyTask {
        task_id: "agent-1".into(),
        message: "Continue".into(),
    };
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(
        encoded,
        serde_json::json!({
            "replyTask": {"task_id": "agent-1", "message": "Continue"}
        })
    );
    assert_eq!(
        serde_json::from_value::<BackgroundIpcRequest>(encoded).unwrap(),
        request
    );
}

#[test]
fn permission_mode_ipc_has_a_stable_wire_shape() {
    let request = BackgroundIpcRequest::SetPermissionMode {
        mode: "bypassPermissions".into(),
    };
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(
        encoded,
        serde_json::json!({
            "setPermissionMode": {"mode": "bypassPermissions"}
        })
    );
    assert_eq!(
        serde_json::from_value::<BackgroundIpcRequest>(encoded).unwrap(),
        request
    );
}

#[test]
fn task_prompt_and_follow_up_messages_project_into_transcript() {
    let descriptor = BackgroundTaskDescriptor {
        task_id: "agent-chat".into(),
        title: "Chat with agent".into(),
        kind: "local_agent".into(),
        status: "running".into(),
        is_backgrounded: true,
        start_time_ms: 1,
        end_time_ms: None,
        last_progress: None,
        error: None,
        prompt: Some("Inspect the repository".into()),
        parent_tool_call_id: None,
        agent_id: Some("agent-chat".into()),
        agent_name: Some("Explore".into()),
        agent_type: Some("Explore".into()),
        model: None,
        token_count: None,
        tool_use_count: None,
        result: None,
    };
    let batch = BackgroundTaskEventBatch {
        schema: 1,
        stream_id: "session/run".into(),
        from_cursor: 0,
        through_cursor: 2,
        cursor_was_stale: true,
        reset_tasks: vec![descriptor],
        events: vec![
            BackgroundTaskEvent {
                cursor: 1,
                task_id: "agent-chat".into(),
                timestamp_ms: 2,
                task: None,
                event: BackgroundTaskEventKind::UserMessage {
                    text: "Focus on mobile".into(),
                },
            },
            BackgroundTaskEvent {
                cursor: 2,
                task_id: "agent-chat".into(),
                timestamp_ms: 3,
                task: None,
                event: BackgroundTaskEventKind::AssistantTurnComplete {
                    text: "I found the relevant files".into(),
                },
            },
        ],
    };

    let snapshots = project_background_task_snapshots(&[BackgroundJobEvent {
        timestamp_ms: 3,
        kind: "task_live_batch".into(),
        data: serde_json::to_value(batch).unwrap(),
    }]);

    assert!(matches!(
        snapshots[0].transcript.as_slice(),
        [
            BackgroundTaskTranscriptEntry::User {
                text: initial,
                timestamp_ms: 1,
            },
            BackgroundTaskTranscriptEntry::User {
                text: follow_up,
                timestamp_ms: 2,
            },
            BackgroundTaskTranscriptEntry::Assistant {
                text: answer,
                timestamp_ms: 3,
            },
        ] if initial == "Inspect the repository"
            && follow_up == "Focus on mobile"
            && answer == "I found the relevant files"
    ));
}

#[test]
fn projected_task_transcript_eviction_keeps_tool_groups_intact() {
    let task_id = "agent-many-tools";
    let descriptor = BackgroundTaskDescriptor {
        task_id: task_id.into(),
        title: "Many tools".into(),
        kind: "local_agent".into(),
        status: "running".into(),
        is_backgrounded: true,
        start_time_ms: 1,
        end_time_ms: None,
        last_progress: None,
        error: None,
        prompt: Some("preserved prompt".into()),
        parent_tool_call_id: None,
        agent_id: Some(task_id.into()),
        agent_name: Some("Explore".into()),
        agent_type: Some("Explore".into()),
        model: None,
        token_count: None,
        tool_use_count: None,
        result: None,
    };
    let mut events = Vec::new();
    for index in 0..301u64 {
        let tool_use_id = format!("tool-{index}");
        events.push(BackgroundTaskEvent {
            cursor: index * 2 + 1,
            task_id: task_id.into(),
            timestamp_ms: index * 2 + 2,
            task: None,
            event: BackgroundTaskEventKind::ToolStart {
                tool_use_id: tool_use_id.clone(),
                name: "Bash".into(),
                input: serde_json::json!({"command": format!("command-{index}")}),
            },
        });
        events.push(BackgroundTaskEvent {
            cursor: index * 2 + 2,
            task_id: task_id.into(),
            timestamp_ms: index * 2 + 3,
            task: None,
            event: BackgroundTaskEventKind::ToolFinish {
                tool_use_id,
                name: "Bash".into(),
                output: Some(serde_json::json!({"stdout": "ok"})),
                error: None,
            },
        });
    }
    let batch = BackgroundTaskEventBatch {
        schema: 1,
        stream_id: "session/run".into(),
        from_cursor: 0,
        through_cursor: events.len() as u64,
        cursor_was_stale: true,
        reset_tasks: vec![descriptor],
        events,
    };

    let snapshots = project_background_task_snapshots(&[BackgroundJobEvent {
        timestamp_ms: 700,
        kind: "task_live_batch".into(),
        data: serde_json::to_value(batch).unwrap(),
    }]);
    let transcript = &snapshots[0].transcript;
    let starts = transcript
        .iter()
        .filter_map(|entry| match entry {
            BackgroundTaskTranscriptEntry::ToolStart { tool_use_id, .. } => Some(tool_use_id),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let finishes = transcript
        .iter()
        .filter_map(|entry| match entry {
            BackgroundTaskTranscriptEntry::ToolFinish { tool_use_id, .. } => Some(tool_use_id),
            _ => None,
        })
        .collect::<HashSet<_>>();

    assert!(transcript.len() <= 600);
    assert_eq!(starts, finishes);
    assert!(matches!(
        transcript.first(),
        Some(BackgroundTaskTranscriptEntry::User { text, .. }) if text == "preserved prompt"
    ));
}

#[test]
fn projected_task_transcript_bounds_tool_output_without_truncating_assistant_text() {
    let task_id = "agent-large-output";
    let descriptor = BackgroundTaskDescriptor {
        task_id: task_id.into(),
        title: "Large output".into(),
        kind: "local_agent".into(),
        status: "running".into(),
        is_backgrounded: true,
        start_time_ms: 1,
        end_time_ms: None,
        last_progress: None,
        error: None,
        prompt: Some("inspect output".into()),
        parent_tool_call_id: None,
        agent_id: Some(task_id.into()),
        agent_name: Some("Explore".into()),
        agent_type: Some("Explore".into()),
        model: None,
        token_count: None,
        tool_use_count: None,
        result: None,
    };
    let batch = BackgroundTaskEventBatch {
        schema: 1,
        stream_id: "session/run".into(),
        from_cursor: 0,
        through_cursor: 3,
        cursor_was_stale: true,
        reset_tasks: vec![descriptor],
        events: vec![
            BackgroundTaskEvent {
                cursor: 1,
                task_id: task_id.into(),
                timestamp_ms: 2,
                task: None,
                event: BackgroundTaskEventKind::ToolStart {
                    tool_use_id: "tool-large".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "python inspect.py"}),
                },
            },
            BackgroundTaskEvent {
                cursor: 2,
                task_id: task_id.into(),
                timestamp_ms: 3,
                task: None,
                event: BackgroundTaskEventKind::ToolFinish {
                    tool_use_id: "tool-large".into(),
                    name: "Bash".into(),
                    output: Some(serde_json::json!({
                        "stdout": "x".repeat(MAX_BACKGROUND_TASK_TOOL_FIELD_CHARS * 4)
                    })),
                    error: None,
                },
            },
            BackgroundTaskEvent {
                cursor: 3,
                task_id: task_id.into(),
                timestamp_ms: 4,
                task: None,
                event: BackgroundTaskEventKind::AssistantTurnComplete {
                    text: "complete assistant answer".into(),
                },
            },
        ],
    };

    let snapshots = project_background_task_snapshots(&[BackgroundJobEvent {
        timestamp_ms: 4,
        kind: "task_live_batch".into(),
        data: serde_json::to_value(batch).unwrap(),
    }]);
    let transcript = &snapshots[0].transcript;
    let output = transcript.iter().find_map(|entry| match entry {
        BackgroundTaskTranscriptEntry::ToolFinish { output, .. } => output.as_ref(),
        _ => None,
    });

    assert_eq!(
        output.and_then(|value| value.get("truncated")),
        Some(&serde_json::Value::Bool(true))
    );
    assert!(transcript.iter().any(|entry| {
        matches!(entry, BackgroundTaskTranscriptEntry::Assistant { text, .. } if text == "complete assistant answer")
    }));
}

#[test]
fn thinking_and_answer_streams_project_as_distinct_live_log_entries() {
    let task_id = "agent-acp-live";
    let events = vec![
        BackgroundTaskEvent {
            cursor: 1,
            task_id: task_id.into(),
            timestamp_ms: 10,
            task: None,
            event: BackgroundTaskEventKind::ThinkingDelta {
                delta: "inspect ".into(),
                snapshot: "inspect ".into(),
            },
        },
        BackgroundTaskEvent {
            cursor: 2,
            task_id: task_id.into(),
            timestamp_ms: 11,
            task: None,
            event: BackgroundTaskEventKind::ThinkingDelta {
                delta: "code".into(),
                snapshot: "inspect code".into(),
            },
        },
        BackgroundTaskEvent {
            cursor: 3,
            task_id: task_id.into(),
            timestamp_ms: 12,
            task: None,
            event: BackgroundTaskEventKind::ThinkingEnd,
        },
        BackgroundTaskEvent {
            cursor: 4,
            task_id: task_id.into(),
            timestamp_ms: 13,
            task: None,
            event: BackgroundTaskEventKind::ThinkingDelta {
                delta: "verify".into(),
                snapshot: "verify".into(),
            },
        },
        BackgroundTaskEvent {
            cursor: 5,
            task_id: task_id.into(),
            timestamp_ms: 14,
            task: None,
            event: BackgroundTaskEventKind::ThinkingEnd,
        },
        BackgroundTaskEvent {
            cursor: 6,
            task_id: task_id.into(),
            timestamp_ms: 15,
            task: None,
            event: BackgroundTaskEventKind::AssistantTextDelta {
                delta: "hello ".into(),
                snapshot: "hello ".into(),
            },
        },
        BackgroundTaskEvent {
            cursor: 7,
            task_id: task_id.into(),
            timestamp_ms: 16,
            task: None,
            event: BackgroundTaskEventKind::AssistantTextDelta {
                delta: "world".into(),
                snapshot: "hello world".into(),
            },
        },
        BackgroundTaskEvent {
            cursor: 8,
            task_id: task_id.into(),
            timestamp_ms: 17,
            task: None,
            event: BackgroundTaskEventKind::Finished {
                status: "completed".into(),
                error: None,
            },
        },
    ];
    let batch = BackgroundTaskEventBatch {
        schema: 1,
        stream_id: "session/run".into(),
        from_cursor: 0,
        through_cursor: 8,
        cursor_was_stale: false,
        reset_tasks: Vec::new(),
        events,
    };

    let snapshots = project_background_task_snapshots(&[BackgroundJobEvent {
        timestamp_ms: 17,
        kind: "task_live_batch".into(),
        data: serde_json::to_value(batch).unwrap(),
    }]);

    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        snapshots[0].log_preview,
        [
            "Thinking: inspect code",
            "Thinking: verify",
            "hello world",
            "agent completed"
        ]
    );
    assert!(matches!(
        snapshots[0].transcript.as_slice(),
        [
            BackgroundTaskTranscriptEntry::Thinking { text: first, .. },
            BackgroundTaskTranscriptEntry::Thinking { text: second, .. },
            BackgroundTaskTranscriptEntry::Assistant { text: answer, .. }
        ] if first == "inspect code" && second == "verify" && answer == "hello world"
    ));
    assert_eq!(snapshots[0].task.status, "completed");
}

#[test]
fn task_event_batch_round_trips_and_appends() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let batch = BackgroundTaskEventBatch {
        schema: 1,
        stream_id: String::from("session/run"),
        from_cursor: 0,
        through_cursor: 5,
        cursor_was_stale: false,
        reset_tasks: Vec::new(),
        events: vec![
            BackgroundTaskEvent {
                cursor: 2,
                task_id: String::from("agent-1"),
                timestamp_ms: 10,
                task: Some(BackgroundTaskDescriptor {
                    task_id: String::from("agent-1"),
                    title: String::from("Inspect events"),
                    prompt: Some(String::from("Inspect events")),
                    kind: String::from("local_agent"),
                    status: String::from("running"),
                    is_backgrounded: false,
                    start_time_ms: 1,
                    end_time_ms: None,
                    last_progress: Some(String::from("searching")),
                    error: None,
                    parent_tool_call_id: Some(String::from("tool-agent-1")),
                    agent_id: Some(String::from("agent-1")),
                    agent_name: Some(String::from("Explore")),
                    agent_type: Some(String::from("Explore")),
                    model: Some(String::from("test-model")),
                    token_count: Some(12),
                    tool_use_count: Some(1),
                    result: None,
                }),
                event: BackgroundTaskEventKind::ToolStart {
                    tool_use_id: String::from("tool-1"),
                    name: String::from("Grep"),
                    input: serde_json::json!({"pattern":"TaskLiveEvent"}),
                },
            },
            BackgroundTaskEvent {
                cursor: 3,
                task_id: String::from("agent-1"),
                timestamp_ms: 11,
                task: None,
                event: BackgroundTaskEventKind::ToolProgress {
                    tool_use_id: String::from("tool-1"),
                    name: String::from("Grep"),
                    message: String::from("searching"),
                },
            },
            BackgroundTaskEvent {
                cursor: 4,
                task_id: String::from("agent-1"),
                timestamp_ms: 12,
                task: None,
                event: BackgroundTaskEventKind::ToolFinish {
                    tool_use_id: String::from("tool-1"),
                    name: String::from("Grep"),
                    output: Some(serde_json::json!({
                        "matches": ["crates/rebon-session-host/src/lib.rs:1"]
                    })),
                    error: None,
                },
            },
            BackgroundTaskEvent {
                cursor: 5,
                task_id: String::from("agent-1"),
                timestamp_ms: 13,
                task: None,
                event: BackgroundTaskEventKind::AssistantTurnComplete {
                    text: String::from("Found the projection."),
                },
            },
        ],
    };

    store
        .append_task_event_batch(&state.identity.job_id, &batch)
        .unwrap();
    let line = fs::read_to_string(store.events_path(&state.identity.job_id))
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .to_string();
    let event: BackgroundJobEvent = serde_json::from_str(&line).unwrap();
    assert_eq!(event.kind, "task_live_batch");
    assert_eq!(
        serde_json::from_value::<BackgroundTaskEventBatch>(event.data).unwrap(),
        batch
    );

    let snapshots = store.read_task_snapshots(&state.identity.job_id).unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].task.task_id, "agent-1");
    assert_eq!(
        snapshots[0].task.parent_tool_call_id.as_deref(),
        Some("tool-agent-1")
    );
    assert_eq!(snapshots[0].task.agent_name.as_deref(), Some("Explore"));
    assert_eq!(snapshots[0].task.agent_type.as_deref(), Some("Explore"));
    assert_eq!(snapshots[0].task.tool_use_count, Some(1));
    assert!(snapshots[0]
        .log_preview
        .iter()
        .any(|line| line.contains("Grep")));
    assert!(matches!(
        snapshots[0].transcript.as_slice(),
        [
            BackgroundTaskTranscriptEntry::User { text: prompt, .. },
            BackgroundTaskTranscriptEntry::ToolStart {
                tool_use_id,
                name,
                input,
                ..
            },
            BackgroundTaskTranscriptEntry::ToolProgress { .. },
            BackgroundTaskTranscriptEntry::ToolFinish {
                output: Some(output),
                error: None,
                ..
            },
            BackgroundTaskTranscriptEntry::Assistant { text, .. }
        ] if prompt == "Inspect events"
            && tool_use_id == "tool-1"
            && name == "Grep"
            && input.get("pattern").and_then(serde_json::Value::as_str)
                == Some("TaskLiveEvent")
            && output.get("matches").is_some()
            && text == "Found the projection."
    ));
}

#[test]
fn background_task_descriptor_accepts_legacy_payload_without_agent_name() {
    let descriptor: BackgroundTaskDescriptor = serde_json::from_value(serde_json::json!({
        "taskId": "agent-legacy",
        "title": "Inspect legacy task",
        "kind": "local_agent",
        "status": "running",
        "isBackgrounded": true,
        "startTimeMs": 1
    }))
    .unwrap();

    assert_eq!(descriptor.agent_name, None);
}

#[test]
fn concurrent_event_appends_keep_json_lines_intact() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let mut writers = Vec::new();
    for writer in 0..4 {
        let store = store.clone();
        let job_id = state.identity.job_id.clone();
        writers.push(std::thread::spawn(move || {
            for index in 0..25 {
                store
                    .append_event(
                        &job_id,
                        "concurrent",
                        serde_json::json!({"writer": writer, "index": index}),
                    )
                    .unwrap();
            }
        }));
    }
    for writer in writers {
        writer.join().unwrap();
    }

    let text = fs::read_to_string(store.events_path(&state.identity.job_id)).unwrap();
    let events = text
        .lines()
        .map(serde_json::from_str::<BackgroundJobEvent>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(events.len(), 101);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "concurrent")
            .count(),
        100
    );
}

#[test]
fn state_writes_count_events_and_use_unique_temp_names() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();

    store
        .append_event(&state.identity.job_id, "first", serde_json::json!({}))
        .unwrap();
    store
        .append_event(&state.identity.job_id, "second", serde_json::json!({}))
        .unwrap();

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.outcome.event_count, 3);
    let tmp_count = fs::read_dir(store.job_dir(&state.identity.job_id))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains("state.json."))
        .count();
    assert_eq!(tmp_count, 0);
}

#[test]
fn create_job_writes_state_without_transcript_copy() {
    let (_dir, store) = store();
    let state = store
        .create_job(
            "build the thing".to_string(),
            PathBuf::from("/tmp/project"),
            BackgroundRuntimeFields {
                provider: Some("openai".into()),
                model: Some("gpt".into()),
                fast_mode: None,
                channels: vec!["server:one".into()],
                development_channels: Vec::new(),
                provider_format: None,
                ui_mode: Some("inline".into()),
                effort_level: None,
                permission_mode: None,
                capability_mode: rebon_types::AgentCapabilityMode::Normal,
                settings: Vec::new(),
                add_dirs: Vec::new(),
                plugin_dirs: Vec::new(),
                mcp_configs: Vec::new(),
                strict_mcp_config: false,
            },
        )
        .unwrap();

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.identity.prompt, "build the thing");
    assert_eq!(loaded.identity.session_id, None);
    assert!(store.state_path(&state.identity.job_id).exists());
    assert!(!store
        .job_dir(&state.identity.job_id)
        .join("transcript.jsonl")
        .exists());
}

#[test]
fn events_and_log_tail_are_append_only() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    store
        .append_event(&state.identity.job_id, "one", serde_json::json!({"n": 1}))
        .unwrap();
    store
        .append_log_line(&state.identity.job_id, "first")
        .unwrap();
    store
        .append_log_line(&state.identity.job_id, "second")
        .unwrap();

    let events = store.read_events_tail(&state.identity.job_id, 2).unwrap();
    assert_eq!(events.last().unwrap().kind, "one");
    let logs = store.read_log_tail(&state.identity.job_id, 1).unwrap();
    assert_eq!(logs, vec!["second".to_string()]);
}

#[test]
fn tail_reader_handles_crlf_and_unterminated_final_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tail.log");
    fs::write(&path, b"zero\r\none\r\ntwo").unwrap();

    assert_eq!(
        read_tail_lines(&path, 2).unwrap(),
        vec!["one".to_string(), "two".to_string()]
    );
}

#[test]
fn tail_reader_does_not_decode_discarded_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tail.log");
    fs::write(
        &path,
        [vec![0xff, b'\n'], b"keep-one\nkeep-two\n".to_vec()].concat(),
    )
    .unwrap();

    assert_eq!(
        read_tail_lines(&path, 2).unwrap(),
        vec!["keep-one".to_string(), "keep-two".to_string()]
    );
}

#[test]
fn bounded_tail_reader_skips_partial_oversized_prefix_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tail.log");
    let mut contents = vec![b'x'; 4096];
    contents.extend_from_slice(b"\nassistant\nfinal\n");
    fs::write(&path, contents).unwrap();

    assert_eq!(
        read_tail_lines_bounded(&path, 2, 64).unwrap(),
        vec!["assistant".to_string(), "final".to_string()]
    );
}

#[test]
fn bounded_tail_reader_drops_a_partial_final_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tail.log");
    let mut contents = vec![b'x'; 4096];
    contents.push(b'\n');
    fs::write(&path, contents).unwrap();

    assert!(read_tail_lines_bounded(&path, 5, 64).unwrap().is_empty());
}

#[test]
fn read_events_from_offset_follows_only_new_appends() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let start = store.events_len(&state.identity.job_id).unwrap();

    store
        .append_event(
            &state.identity.job_id,
            "live_a",
            serde_json::json!({"n": 1}),
        )
        .unwrap();
    store
        .append_event(
            &state.identity.job_id,
            "live_b",
            serde_json::json!({"n": 2}),
        )
        .unwrap();

    let (events, offset) = store
        .read_events_from_offset(&state.identity.job_id, start)
        .unwrap();
    assert_eq!(
        events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        vec!["live_a", "live_b"]
    );
    assert_eq!(offset, store.events_len(&state.identity.job_id).unwrap());

    let (rest, same_offset) = store
        .read_events_from_offset(&state.identity.job_id, offset)
        .unwrap();
    assert!(rest.is_empty());
    assert_eq!(same_offset, offset);

    store
        .append_event(
            &state.identity.job_id,
            "live_c",
            serde_json::json!({"n": 3}),
        )
        .unwrap();
    let (tail, _) = store
        .read_events_from_offset(&state.identity.job_id, offset)
        .unwrap();
    assert_eq!(
        tail.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        vec!["live_c"]
    );

    // An offset beyond the file (log recreated) restarts from the top.
    let (all, _) = store
        .read_events_from_offset(&state.identity.job_id, u64::MAX)
        .unwrap();
    assert!(all.iter().any(|e| e.kind == "live_a"));
    assert!(all.iter().any(|e| e.kind == "live_c"));
}

#[test]
fn bounded_event_reads_stream_the_full_log_in_chunks() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    for index in 0..40 {
        store
            .append_event(
                &state.identity.job_id,
                &format!("evt_{index}"),
                serde_json::json!({"n": index, "pad": "x".repeat(64)}),
            )
            .unwrap();
    }
    let (all, full_offset) = store
        .read_events_from_offset(&state.identity.job_id, 0)
        .unwrap();

    // A tiny budget forces many chunks (and single records larger than the
    // budget must still make progress); the streamed sequence and final
    // offset must match the unbounded read exactly.
    let mut streamed = Vec::new();
    let mut offset = 0u64;
    loop {
        let (chunk, new_offset) = store
            .read_events_from_offset_bounded(&state.identity.job_id, offset, 16)
            .unwrap();
        assert!(new_offset >= offset, "no shrink happened in this test");
        streamed.extend(chunk);
        if new_offset == offset {
            break;
        }
        offset = new_offset;
    }
    assert_eq!(offset, full_offset);
    assert_eq!(
        streamed.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        all.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>()
    );

    // A shrunken file is reported as a regression to offset 0, never a
    // silent restart that would let a streaming caller mix generations.
    let (none, reset) = store
        .read_events_from_offset_bounded(&state.identity.job_id, u64::MAX, 16)
        .unwrap();
    assert!(none.is_empty());
    assert_eq!(reset, 0);
}

#[test]
fn missing_events_reads_do_not_create_job_directories() {
    let (_dir, store) = store();
    let job_id = "bg-missing-events";

    assert!(store.read_events(job_id).unwrap().is_empty());
    assert!(store.read_events_tail(job_id, 10).unwrap().is_empty());
    assert_eq!(store.events_len(job_id).unwrap(), 0);
    assert_eq!(
        store.read_events_from_offset(job_id, 999).unwrap(),
        (Vec::new(), 0)
    );
    assert!(!store.job_dir(job_id).exists());
    assert!(!store.jobs_dir().exists());
}

#[test]
fn incremental_events_wait_for_newline_and_recover_after_truncate() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let start = store.events_len(&state.identity.job_id).unwrap();
    let partial = BackgroundJobEvent {
        timestamp_ms: now_ms(),
        kind: "partial".into(),
        data: serde_json::json!({"complete": false}),
    };
    let encoded = serde_json::to_vec(&partial).unwrap();
    let mut file = OpenOptions::new()
        .append(true)
        .open(store.events_path(&state.identity.job_id))
        .unwrap();
    file.write_all(&encoded).unwrap();
    file.flush().unwrap();

    let (before_newline, unchanged) = store
        .read_events_from_offset(&state.identity.job_id, start)
        .unwrap();
    assert!(before_newline.is_empty());
    assert_eq!(unchanged, start);

    file.write_all(b"\n").unwrap();
    file.flush().unwrap();
    drop(file);
    let (completed, completed_offset) = store
        .read_events_from_offset(&state.identity.job_id, start)
        .unwrap();
    assert_eq!(
        completed
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["partial"]
    );
    assert!(completed_offset > start);

    let replacement = BackgroundJobEvent {
        timestamp_ms: 1,
        kind: "after_truncate".into(),
        data: serde_json::json!({}),
    };
    let mut replacement_bytes = serde_json::to_vec(&replacement).unwrap();
    replacement_bytes.push(b'\n');
    fs::write(store.events_path(&state.identity.job_id), replacement_bytes).unwrap();

    let (recovered, recovered_offset) = store
        .read_events_from_offset(&state.identity.job_id, completed_offset)
        .unwrap();
    assert_eq!(
        recovered
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["after_truncate"]
    );
    assert_eq!(
        recovered_offset,
        store.events_len(&state.identity.job_id).unwrap()
    );
}

#[test]
fn concurrent_incremental_event_reads_only_return_complete_lines() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let mut offset = store.events_len(&state.identity.job_id).unwrap();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_store = store.clone();
    let writer_job_id = state.identity.job_id.clone();
    let writer_done = done.clone();
    let writer = std::thread::spawn(move || {
        for index in 0..64 {
            writer_store
                .append_event(
                    &writer_job_id,
                    &format!("concurrent_{index}"),
                    serde_json::json!({"index": index}),
                )
                .unwrap();
        }
        writer_done.store(true, std::sync::atomic::Ordering::Release);
    });

    let mut kinds = Vec::new();
    while !done.load(std::sync::atomic::Ordering::Acquire) {
        let (events, next_offset) = store
            .read_events_from_offset(&state.identity.job_id, offset)
            .unwrap();
        assert!(next_offset >= offset);
        offset = next_offset;
        kinds.extend(events.into_iter().map(|event| event.kind));
        std::thread::yield_now();
    }
    writer.join().unwrap();
    let (events, next_offset) = store
        .read_events_from_offset(&state.identity.job_id, offset)
        .unwrap();
    kinds.extend(events.into_iter().map(|event| event.kind));
    offset = next_offset;

    assert_eq!(
        kinds,
        (0..64)
            .map(|index| format!("concurrent_{index}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(offset, store.events_len(&state.identity.job_id).unwrap());
}

/// A logged update carries the position the stream gave it, and one the
/// owner never streamed carries nothing — which is how a reader tells
/// "already seen live" from "only the file has this".
#[test]
fn a_logged_update_carries_the_stamp_it_was_streamed_at() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let update = rebon_types::SessionUpdateParams {
        session_id: "sess-one".into(),
        update: rebon_types::SessionUpdate::TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
        },
    };
    let stamp = StreamStamp {
        epoch: 0xA5,
        cursor: 17,
    };

    store
        .append_session_update_for_turn_at(&state.identity.job_id, 3, &update, stamp)
        .unwrap();
    store
        .append_session_update_for_turn(&state.identity.job_id, 3, &update)
        .unwrap();

    let (events, _) = store
        .read_events_from_offset(&state.identity.job_id, 0)
        .unwrap();
    let updates: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "session_update")
        .collect();
    let stamps: Vec<_> = updates.iter().map(|event| event.stream_stamp()).collect();
    assert_eq!(stamps, vec![Some(stamp), None]);
    assert_eq!(updates[0].data["turnGeneration"], 3);
    let round_trip: rebon_types::SessionUpdateParams =
        serde_json::from_value(updates[0].data.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&round_trip).unwrap(),
        serde_json::to_value(&update).unwrap(),
        "the stamp rides beside the update, not inside it"
    );
}

#[test]
fn append_session_update_refreshes_running_summary() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    store.write_state(&state).unwrap();
    let update = rebon_types::SessionUpdateParams {
        session_id: "sess-one".into(),
        update: rebon_types::SessionUpdate::ToolCall {
            tool_call_id: "tool-1".into(),
            title: "Read src/lib.rs".into(),
            kind: rebon_types::ToolKind::Read,
            status: rebon_types::ToolCallStatus::InProgress,
            content: None,
            locations: None,
            raw_input: None,
            raw_output: None,
        },
    };

    store
        .append_session_update(&state.identity.job_id, &update)
        .unwrap();

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Running);
    assert_eq!(
        loaded.outcome.summary.as_deref(),
        Some("reading: Read src/lib.rs")
    );
    assert!(loaded.outcome.summary_updated_at_ms.is_some());
    let events = store.read_events_tail(&state.identity.job_id, 5).unwrap();
    assert!(events.iter().any(|event| event.kind == "summary_updated"));
}

#[test]
fn append_session_update_throttles_running_summary_refreshes() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    store.write_state(&state).unwrap();

    let first = rebon_types::SessionUpdateParams {
        session_id: "sess-one".into(),
        update: rebon_types::SessionUpdate::AgentMessageChunk {
            content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                text: "first summary".into(),
                annotations: None,
            }),
        },
    };
    let second = rebon_types::SessionUpdateParams {
        session_id: "sess-one".into(),
        update: rebon_types::SessionUpdate::AgentMessageChunk {
            content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                text: "second summary".into(),
                annotations: None,
            }),
        },
    };

    store
        .append_session_update(&state.identity.job_id, &first)
        .unwrap();
    let first_loaded = store.read_state(&state.identity.job_id).unwrap();
    store
        .append_session_update(&state.identity.job_id, &second)
        .unwrap();
    let throttled = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(throttled.outcome.summary, first_loaded.outcome.summary);

    store
        .update_state(&state.identity.job_id, |state| {
            state.outcome.summary_updated_at_ms =
                Some(now_ms().saturating_sub(BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS + 1));
            Ok(())
        })
        .unwrap();
    store
        .append_session_update(&state.identity.job_id, &second)
        .unwrap();
    let refreshed = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        refreshed.outcome.summary.as_deref(),
        Some("responding: second summary")
    );
}

#[test]
fn turn_end_summary_uses_latest_session_update_even_when_live_write_was_throttled() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    store.write_state(&state).unwrap();

    for text in ["first summary", "final summary"] {
        let update = rebon_types::SessionUpdateParams {
            session_id: "sess-one".into(),
            update: rebon_types::SessionUpdate::AgentMessageChunk {
                content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: text.into(),
                    annotations: None,
                }),
            },
        };
        store
            .append_session_update(&state.identity.job_id, &update)
            .unwrap();
    }

    let throttled = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        throttled.outcome.summary.as_deref(),
        Some("responding: first summary")
    );
    assert_eq!(
        background_job_success_summary_from_store(&store, &state.identity.job_id).as_deref(),
        Some("responding: final summary")
    );
}

#[test]
fn turn_end_summary_prefers_model_backed_agent_view_summary_event() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    store.write_state(&state).unwrap();

    let update = rebon_types::SessionUpdateParams {
        session_id: "sess-one".into(),
        update: rebon_types::SessionUpdate::AgentMessageChunk {
            content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                text: "low-level final chunk".into(),
                annotations: None,
            }),
        },
    };
    store
        .append_session_update(&state.identity.job_id, &update)
        .unwrap();
    store
        .append_event(
            &state.identity.job_id,
            "agent_view_summary_updated",
            serde_json::json!({ "summary": "fixed cached PR row status" }),
        )
        .unwrap();

    assert_eq!(
        background_job_success_summary_from_store(&store, &state.identity.job_id).as_deref(),
        Some("fixed cached PR row status")
    );
}

#[test]
fn turn_end_summary_ignores_running_model_summary_events() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    store.write_state(&state).unwrap();

    let update = rebon_types::SessionUpdateParams {
        session_id: "sess-one".into(),
        update: rebon_types::SessionUpdate::AgentMessageChunk {
            content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                text: "final session update".into(),
                annotations: None,
            }),
        },
    };
    store
        .append_session_update(&state.identity.job_id, &update)
        .unwrap();
    store
        .append_event(
            &state.identity.job_id,
            "agent_view_summary_updated",
            serde_json::json!({
                "summary": "running stale model summary",
                "phase": "running",
            }),
        )
        .unwrap();

    assert_eq!(
        background_job_success_summary_from_store(&store, &state.identity.job_id).as_deref(),
        Some("responding: final session update")
    );
}

#[test]
fn list_jobs_reports_present_but_unreadable_state_instead_of_hiding_it() {
    let (_dir, store) = store();
    let broken = store.job_dir("broken-job");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("state.json"), "not-json").unwrap();
    // Event-only directories remain valid non-job storage.
    std::fs::create_dir_all(store.job_dir("supervisor")).unwrap();
    std::fs::write(store.events_path("supervisor"), "").unwrap();

    let error = store.list_jobs().unwrap_err();
    assert!(error
        .to_string()
        .contains("failed to enumerate background job state for broken-job"));
}

#[test]
fn stale_pid_reconciliation_marks_definitely_dead_running_job() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.process.pid = Some(u32::MAX - 1);
    state.identity.pending_prompts = vec![pending_prompt("pp-stale", "queued reply", Vec::new())];
    store.write_state(&state).unwrap();

    let mut listed = store.list_jobs().unwrap();
    let loaded = listed.pop().unwrap();
    if process_is_running(u32::MAX - 1) == Some(false) {
        assert_eq!(loaded.process.status, BackgroundJobStatus::Failed);
        assert!(loaded.outcome.error.is_some());
        // The queued reply survives the crash so a respawn can re-run it.
        assert_eq!(pending_text(&loaded), Some("queued reply"));
    } else {
        assert_eq!(loaded.process.status, BackgroundJobStatus::Running);
    }
}

/// A turn that fails on a quota wall or a sub-agent error leaves the job
/// terminal while its worker is still alive; the worker then goes away
/// without handing back its owner record. Reconciliation used to skip any
/// job that was not Running/NeedsInput, so nothing ever cleared that
/// record: removal and respawn both refused the job forever, and every IPC
/// request kept dialing a dead port. The conclusion the job already
/// reached is kept — only the owner is given up.
#[test]
fn stale_pid_reconciliation_frees_a_terminal_job_from_its_dead_owner() {
    let (_dir, store) = store();
    // The current pid under a mismatched identity is the one owner record
    // every platform can prove is gone; a made-up pid is only ever
    // "unknown" on Windows, which makes the assertions below vacuous.
    let current_pid = std::process::id();
    let Some(stale_identity) = process_identity(current_pid).map(|id| format!("{id}-stale")) else {
        return;
    };
    if !matches!(
        recorded_process_is_running(current_pid, Some(&stale_identity)),
        Ok(false)
    ) {
        return;
    }

    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Failed;
    state.outcome.error = Some("weekly limit reached".into());
    state.process.pid = Some(current_pid);
    state.process.pid_identity = Some(stale_identity);
    state.process.ipc_port = Some(65535);
    state.process.ipc_token = Some("token".into());
    store.write_state(&state).unwrap();

    store.reconcile_stale_pid(&mut state).unwrap();

    assert_eq!(state.process.pid, None);
    assert_eq!(state.process.pid_identity, None);
    assert_eq!(state.process.ipc_port, None);
    assert_eq!(state.process.ipc_token, None);
    assert_eq!(
        state.process.status,
        BackgroundJobStatus::Failed,
        "a job that already concluded keeps its own outcome"
    );
    assert_eq!(state.outcome.error.as_deref(), Some("weekly limit reached"));
    store
        .remove_job(&state.identity.job_id)
        .expect("a job with no live owner can be removed");
}

#[test]
fn fenced_dead_owner_reconciliation_preserves_accepted_follow_up() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Failed;
    state.process.pid = Some(u32::MAX - 1);
    state.process.process_owner_fenced = true;
    state.identity.pending_prompts = vec![pending_prompt(
        "pp-fenced-dead",
        "accepted follow-up",
        vec![BackgroundImageAttachment {
            id: 9,
            data: "accepted-image".into(),
            media_type: "image/png".into(),
            filename: None,
            source_path: None,
        }],
    )];
    store.write_state(&state).unwrap();

    store.reconcile_stale_pid(&mut state).unwrap();

    if process_is_running(u32::MAX - 1) == Some(false) {
        assert_eq!(state.process.pid, None);
        assert_eq!(state.process.pid_identity, None);
        assert!(!state.process.process_owner_fenced);
        assert_eq!(pending_text(&state), Some("accepted follow-up"));
        assert_eq!(state.identity.pending_prompts[0].images.len(), 1);
    } else {
        assert_eq!(state.process.pid, Some(u32::MAX - 1));
        assert!(state.process.process_owner_fenced);
    }
}

#[test]
fn adopt_existing_session_records_idle_or_queued_job() {
    let (_dir, store) = store();
    let runtime = BackgroundRuntimeFields {
        model: Some("gpt".into()),
        ..runtime()
    };

    let idle = adopt_existing_background_session(
        &store,
        "Continue session sess-one".into(),
        PathBuf::from("."),
        runtime.clone(),
        "sess-one".into(),
        Some("session one".into()),
        false,
        JobPlacement::Background,
        &rebon_exe(),
    )
    .unwrap();
    assert_eq!(idle.process.status, BackgroundJobStatus::Idle);
    assert_eq!(idle.identity.session_id.as_deref(), Some("sess-one"));
    assert_eq!(idle.identity.name, "session one");
}

#[test]
fn adopt_existing_session_by_session_id_reuses_job_without_duplication() {
    let (_dir, store) = store();
    let first = adopt_existing_background_session(
        &store,
        "first prompt".into(),
        PathBuf::from("."),
        runtime(),
        "sess-one".into(),
        Some("first name".into()),
        false,
        JobPlacement::Background,
        &rebon_exe(),
    )
    .unwrap();

    let second = adopt_existing_background_session(
        &store,
        "second prompt".into(),
        PathBuf::from("."),
        runtime(),
        "sess-one".into(),
        Some("second name".into()),
        false,
        JobPlacement::Background,
        &rebon_exe(),
    )
    .unwrap();

    assert_eq!(second.identity.job_id, first.identity.job_id);
    assert_eq!(second.identity.prompt, "second prompt");
    assert_eq!(second.identity.name, "second name");
    assert_eq!(second.process.status, BackgroundJobStatus::Idle);
    assert_eq!(store.list_jobs().unwrap().len(), 1);
}

#[test]
fn queue_existing_session_rejects_fenced_owner_and_preserves_accepted_follow_up() {
    let (_dir, store) = store();
    let expected = fenced_failed_session(&store, "sess-fenced");
    let replacement_image = BackgroundImageAttachment {
        id: 8,
        data: "replacement-image".into(),
        media_type: "image/jpeg".into(),
        filename: None,
        source_path: None,
    };

    let error = queue_existing_background_session_with_images(
        &store,
        "replacement prompt".into(),
        None,
        vec![replacement_image],
        PathBuf::from("replacement"),
        runtime(),
        "sess-fenced".into(),
        Some("replacement name".into()),
        &rebon_exe(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("ownership is fenced"));
    assert_eq!(
        store.read_state(&expected.identity.job_id).unwrap(),
        expected
    );
}

#[test]
fn adopt_existing_session_rejects_fenced_owner_without_creating_a_duplicate() {
    let (_dir, store) = store();
    let expected = fenced_failed_session(&store, "sess-fenced");

    let error = adopt_existing_background_session(
        &store,
        "replacement prompt".into(),
        PathBuf::from("replacement"),
        runtime(),
        "sess-fenced".into(),
        Some("replacement name".into()),
        false,
        JobPlacement::Background,
        &rebon_exe(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("ownership is fenced"));
    assert_eq!(
        store.read_state(&expected.identity.job_id).unwrap(),
        expected
    );
    assert_eq!(store.list_jobs().unwrap().len(), 1);
}

#[test]
fn fenced_owner_rejects_queue_and_reply_mutations() {
    let (_dir, store) = store();
    let expected = fenced_failed_session(&store, "sess-fenced");

    let queue_error =
        queue_background_job_in_store(&store, &expected.identity.job_id, false, &rebon_exe())
            .unwrap_err();
    assert!(queue_error.to_string().contains("ownership is fenced"));
    assert_eq!(
        store.read_state(&expected.identity.job_id).unwrap(),
        expected
    );

    let inactive_reply_error = queue_background_reply_if_inactive_with_images(
        &store,
        &expected.identity.job_id,
        "replacement reply".into(),
        Vec::new(),
        &rebon_exe(),
    )
    .unwrap_err();
    assert!(inactive_reply_error
        .to_string()
        .contains("ownership is fenced"));
    assert_eq!(
        store.read_state(&expected.identity.job_id).unwrap(),
        expected
    );

    let reply_error = reply_to_background_job_in_store_with_images(
        &store,
        &expected.identity.job_id,
        "replacement reply".into(),
        Vec::new(),
        false,
        false,
        &rebon_exe(),
    )
    .unwrap_err();
    assert!(reply_error.to_string().contains("ownership is fenced"));
    assert_eq!(
        store.read_state(&expected.identity.job_id).unwrap(),
        expected
    );
}

#[test]
fn fenced_owner_rejects_the_idle_finalizer() {
    let (_dir, store) = store();
    let expected = fenced_failed_session(&store, "sess-fenced");

    let idle_error = mark_background_job_idle(&store, &expected.identity.job_id).unwrap_err();
    assert!(idle_error.to_string().contains("ownership is fenced"));
    assert_eq!(
        store.read_state(&expected.identity.job_id).unwrap(),
        expected
    );
}

#[test]
fn fenced_owner_rejects_remove_and_respawn_without_duplication() {
    let (_dir, store) = store();
    let expected = fenced_failed_session(&store, "sess-fenced");

    let remove_error = store.remove_job(&expected.identity.job_id).unwrap_err();
    assert!(remove_error.to_string().contains("ownership is fenced"));
    assert_eq!(
        store.read_state(&expected.identity.job_id).unwrap(),
        expected
    );

    let respawn_error =
        respawn_background_job_in_store(&store, &expected.identity.job_id, false, &rebon_exe())
            .unwrap_err();
    assert!(respawn_error.to_string().contains("ownership is fenced"));
    assert_eq!(
        store.read_state(&expected.identity.job_id).unwrap(),
        expected
    );
    assert_eq!(store.list_jobs().unwrap().len(), 1);
}

#[test]
fn respawn_all_skips_fenced_owner_without_duplication() {
    let (_dir, store) = store();
    let expected = fenced_failed_session(&store, "sess-fenced");

    let respawned = respawn_all_background_jobs_in_store(&store, false, &rebon_exe()).unwrap();

    assert!(respawned.is_empty());
    assert_eq!(
        store.read_state(&expected.identity.job_id).unwrap(),
        expected
    );
    assert_eq!(store.list_jobs().unwrap().len(), 1);
}

#[test]
fn queue_after_verified_owner_exit_preserves_accepted_follow_up() {
    let (_dir, store) = store();
    let expected = fenced_failed_session(&store, "sess-fenced");
    store
        .update_state(&expected.identity.job_id, |state| {
            state.process.pid = None;
            state.process.pid_identity = None;
            state.process.process_owner_fenced = false;
            Ok(())
        })
        .unwrap();

    queue_background_job_in_store(&store, &expected.identity.job_id, false, &rebon_exe()).unwrap();

    let queued = store.read_state(&expected.identity.job_id).unwrap();
    assert_eq!(queued.process.status, BackgroundJobStatus::Queued);
    assert_eq!(
        queued.identity.pending_prompts,
        expected.identity.pending_prompts
    );
}

#[test]
fn existing_session_reuse_after_owner_exit_preserves_unfinished_follow_up() {
    let (_dir, store) = store();
    let fenced = fenced_failed_session(&store, "sess-fenced");
    store
        .update_state(&fenced.identity.job_id, |state| {
            state.process.pid = None;
            state.process.pid_identity = None;
            state.process.process_owner_fenced = false;
            Ok(())
        })
        .unwrap();
    let expected = store.read_state(&fenced.identity.job_id).unwrap();

    let error = queue_existing_background_session_with_images(
        &store,
        "replacement prompt".into(),
        None,
        Vec::new(),
        PathBuf::from("replacement"),
        runtime(),
        "sess-fenced".into(),
        Some("replacement name".into()),
        &rebon_exe(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("accepted follow-up"));
    assert_eq!(
        store.read_state(&expected.identity.job_id).unwrap(),
        expected
    );
}

#[test]
fn inactive_replies_append_after_unfinished_accepted_follow_up() {
    let (_dir, store) = store();
    // Queueing a reply revives the supervisor. Without a live roster
    // this test reached the real spawn path and started whatever
    // `rebon_exe()` resolved to.
    seed_live_supervisor(&store);
    let fenced = fenced_failed_session(&store, "sess-fenced");
    store
        .update_state(&fenced.identity.job_id, |state| {
            state.process.pid = None;
            state.process.pid_identity = None;
            state.process.process_owner_fenced = false;
            Ok(())
        })
        .unwrap();

    queue_background_reply_if_inactive_with_images(
        &store,
        &fenced.identity.job_id,
        "replacement reply".into(),
        Vec::new(),
        &rebon_exe(),
    )
    .unwrap();
    reply_to_background_job_in_store_with_images(
        &store,
        &fenced.identity.job_id,
        "second replacement".into(),
        Vec::new(),
        false,
        false,
        &rebon_exe(),
    )
    .unwrap();

    let queued = store.read_state(&fenced.identity.job_id).unwrap();
    assert_eq!(queued.process.status, BackgroundJobStatus::Queued);
    assert_eq!(
        queued
            .identity
            .pending_prompts
            .iter()
            .map(|prompt| prompt.text.as_str())
            .collect::<Vec<_>>(),
        vec![
            "accepted follow-up",
            "replacement reply",
            "second replacement"
        ]
    );
}

#[test]
fn background_job_state_defaults_legacy_jobs_to_origin_cwd() {
    let state = BackgroundJobState::new("prompt".into(), "F:/repo".into(), runtime(), None);
    let json = serde_json::to_value(state).unwrap();
    assert!(json.get("isolateInWorktree").is_none());

    let restored: BackgroundJobState = serde_json::from_value(json).unwrap();
    assert!(!restored.workspace.isolate_in_worktree);
    assert!(restored.process.pid_identity.is_none());
}

#[test]
fn owner_lifecycle_fields_are_backward_compatible_and_default_safe() {
    let mut state = BackgroundJobState::new("prompt".into(), "F:/repo".into(), runtime(), None);
    state.process.spawn_admitted = true;
    state.process.owner_detached_group = true;
    let mut json = serde_json::to_value(state).unwrap();
    json.as_object_mut().unwrap().remove("spawnAdmitted");
    json.as_object_mut().unwrap().remove("ownerDetachedGroup");

    let restored: BackgroundJobState = serde_json::from_value(json).unwrap();
    assert!(!restored.process.spawn_admitted);
    assert!(!restored.process.owner_detached_group);
}

#[test]
fn recorded_owner_snapshot_sets_fences_and_clears_every_owner_field_together() {
    let mut state = BackgroundJobState::new("prompt".into(), "F:/repo".into(), runtime(), None);
    let owner = RecordedOwnerSnapshot::owned(
        42,
        Some("identity".into()),
        true,
        false,
        Some(41000),
        Some("token".into()),
        7,
    );
    state.set_recorded_owner(owner.clone());
    assert_eq!(state.recorded_owner(), owner);

    let fenced = owner.fenced(true);
    state.set_recorded_owner(fenced.clone());
    assert_eq!(state.recorded_owner(), fenced);
    assert!(state.process.process_owner_fenced);
    assert_eq!(state.process.ipc_port, None);
    assert_eq!(state.process.ipc_token, None);

    state.clear_recorded_owner();
    assert_eq!(state.recorded_owner(), RecordedOwnerSnapshot::unowned(7));
}

#[test]
fn background_job_state_defaults_legacy_jobs_to_best_effort_worktree_policy() {
    let state = BackgroundJobState::new("prompt".into(), "F:/repo".into(), runtime(), None);
    let json = serde_json::to_value(&state).unwrap();
    assert!(json.get("requireWorktree").is_none());
    assert!(json.get("preserveWorktreeOnSuccess").is_none());

    let legacy: BackgroundJobState = serde_json::from_value(json).unwrap();
    assert!(!legacy.workspace.require_worktree);
    assert!(!legacy.workspace.preserve_worktree_on_success);

    let mut strict = state;
    strict.workspace.require_worktree = true;
    strict.workspace.preserve_worktree_on_success = true;
    let restored: BackgroundJobState =
        serde_json::from_value(serde_json::to_value(strict).unwrap()).unwrap();
    assert!(restored.workspace.require_worktree);
    assert!(restored.workspace.preserve_worktree_on_success);
}

#[test]
fn an_ipc_reply_without_images_still_parses() {
    let without_images: BackgroundIpcRequest =
        serde_json::from_str(r#"{"reply":{"message":"continue"}}"#).unwrap();
    assert_eq!(
        without_images,
        BackgroundIpcRequest::Reply {
            message: "continue".into(),
            images: Vec::new(),
        }
    );
}

/// A dispatch made from inside a worker records who made it, so stopping
/// that worker can release it. Without this the ownership rule has
/// nothing to read.
#[test]
fn launching_from_a_worker_records_the_worker_as_the_parent() {
    let (_dir, store) = store();
    let project_dir = tempfile::tempdir().unwrap();
    seed_live_supervisor(&store);

    let job = launch_background_prompt(
        &store,
        BackgroundLaunchOptions {
            prompt: "check the migration".into(),
            images: Vec::new(),
            cwd: project_dir.path().to_path_buf(),
            isolate_in_worktree: true,
            require_worktree: false,
            preserve_worktree_on_success: false,
            queue_session: false,
            runtime: runtime(),
            name: None,
            agent_type: None,
            parent_job_id: Some("bg-parent-worker".into()),
        },
        &rebon_exe(),
        allow_all,
    )
    .unwrap();

    assert_eq!(
        job.identity.parent_job_id.as_deref(),
        Some("bg-parent-worker")
    );
    assert_eq!(
        store
            .read_state(&job.identity.job_id)
            .unwrap()
            .identity
            .parent_job_id
            .as_deref(),
        Some("bg-parent-worker"),
        "ownership has to survive the round trip through disk"
    );
}

#[test]
fn launch_background_prompt_applies_subagent_frontmatter_from_project_dir() {
    let (_dir, store) = store();
    let project_dir = tempfile::tempdir().unwrap();
    let agents_dir = project_dir.path().join(".rebon").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("project-fixture.md"),
        "---\nname: project-fixture\ndescription: fixture\nmodel: sonnet\neffort: high\n---\n\nFixture body.",
    )
    .unwrap();
    seed_live_supervisor(&store);

    let job = launch_background_prompt(
        &store,
        BackgroundLaunchOptions {
            prompt: "investigate the flaky test".into(),
            images: Vec::new(),
            cwd: project_dir.path().to_path_buf(),
            isolate_in_worktree: true,
            require_worktree: false,
            preserve_worktree_on_success: false,
            queue_session: false,
            runtime: runtime(),
            name: None,
            agent_type: Some("project-fixture".into()),
            parent_job_id: None,
        },
        &rebon_exe(),
        allow_all,
    )
    .unwrap();

    let loaded = store.read_state(&job.identity.job_id).unwrap();
    assert_eq!(
        loaded.identity.agent_type.as_deref(),
        Some("project-fixture")
    );
    assert!(loaded.workspace.isolate_in_worktree);
    assert_eq!(loaded.identity.runtime.model.as_deref(), Some("sonnet"));
    assert_eq!(
        loaded.identity.runtime.effort_level.as_deref(),
        Some("high")
    );

    let events = store.read_events_tail(&job.identity.job_id, 10).unwrap();
    let applied_event = events
        .iter()
        .find(|event| event.kind == "agent_runtime_applied")
        .expect("agent_runtime_applied event emitted");
    assert_eq!(
        applied_event.data["agentType"],
        serde_json::json!("project-fixture")
    );
    assert_eq!(
        applied_event.data["applied"]["model"],
        serde_json::json!("sonnet")
    );
}

#[test]
fn launch_background_prompt_persists_a_required_and_preserved_worktree_policy() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);

    let job = launch_background_prompt(
        &store,
        BackgroundLaunchOptions {
            prompt: "implement the queue row".into(),
            images: Vec::new(),
            cwd: PathBuf::from("."),
            isolate_in_worktree: true,
            require_worktree: true,
            preserve_worktree_on_success: true,
            queue_session: false,
            runtime: runtime(),
            name: None,
            agent_type: None,
            parent_job_id: None,
        },
        &rebon_exe(),
        allow_all,
    )
    .unwrap();

    let loaded = store.read_state(&job.identity.job_id).unwrap();
    assert!(loaded.workspace.isolate_in_worktree);
    assert!(loaded.workspace.require_worktree);
    assert!(loaded.workspace.preserve_worktree_on_success);
}

#[test]
fn queue_tool_provenance_comes_from_launch_metadata_not_prompt_text() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);

    let spoofed = launch_background_prompt(
        &store,
        BackgroundLaunchOptions {
            prompt: format!(
                "{} This text came from an ordinary --bg prompt.",
                AGENT_QUEUE_SUPERVISOR_PROMPT_PREFIX
            ),
            images: Vec::new(),
            cwd: PathBuf::from("."),
            isolate_in_worktree: false,
            require_worktree: false,
            preserve_worktree_on_success: false,
            queue_session: false,
            runtime: runtime(),
            name: None,
            agent_type: None,
            parent_job_id: None,
        },
        &rebon_exe(),
        allow_all,
    )
    .unwrap();
    let trusted = launch_background_prompt(
        &store,
        BackgroundLaunchOptions {
            prompt: "queue supervisor charter".into(),
            images: Vec::new(),
            cwd: PathBuf::from("."),
            isolate_in_worktree: false,
            require_worktree: false,
            preserve_worktree_on_success: false,
            queue_session: true,
            runtime: runtime(),
            name: None,
            agent_type: None,
            parent_job_id: None,
        },
        &rebon_exe(),
        allow_all,
    )
    .unwrap();

    assert!(
        !store
            .read_state(&spoofed.identity.job_id)
            .unwrap()
            .identity
            .queue_session
    );
    assert!(
        store
            .read_state(&trusted.identity.job_id)
            .unwrap()
            .identity
            .queue_session
    );
}

/// A tree being torn down must not grow: the stop walk visits each job
/// once, so a child dispatched after its parent was stopped would never
/// be reached and would outlive the worker that asked for it.
#[test]
fn a_stopped_parent_cannot_dispatch_new_children() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);
    let mut parent = store
        .create_job("parent".into(), PathBuf::from("."), runtime())
        .unwrap();
    parent.process.status = BackgroundJobStatus::Stopped;
    store.write_state(&parent).unwrap();

    let err = launch_background_prompt(
        &store,
        BackgroundLaunchOptions {
            prompt: "dispatched too late".into(),
            images: Vec::new(),
            cwd: PathBuf::from("."),
            isolate_in_worktree: false,
            require_worktree: false,
            preserve_worktree_on_success: false,
            queue_session: false,
            runtime: runtime(),
            name: None,
            agent_type: None,
            parent_job_id: Some(parent.identity.job_id.clone()),
        },
        &rebon_exe(),
        allow_all,
    )
    .unwrap_err();

    assert!(err.to_string().contains("was stopped"), "{err}");
}

/// Ownership has to be true from the first moment a job is visible.
/// The parent link used to arrive in a follow-up write, leaving a window
/// where a parent's stop walk read the job as nobody's child and skipped
/// it — the child then outlived the worker that started it.
#[test]
fn a_launched_child_carries_its_parent_from_its_very_first_state_write() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);

    let job = launch_background_prompt(
        &store,
        BackgroundLaunchOptions {
            prompt: "dispatched from a worker".into(),
            images: Vec::new(),
            cwd: PathBuf::from("."),
            isolate_in_worktree: false,
            require_worktree: false,
            preserve_worktree_on_success: false,
            queue_session: false,
            runtime: runtime(),
            name: None,
            agent_type: None,
            parent_job_id: Some("bg-parent".into()),
        },
        &rebon_exe(),
        allow_all,
    )
    .unwrap();

    assert_eq!(job.identity.parent_job_id.as_deref(), Some("bg-parent"));
    assert_eq!(
        store
            .read_state(&job.identity.job_id)
            .unwrap()
            .identity
            .parent_job_id
            .as_deref(),
        Some("bg-parent")
    );
    // The `created` event is the earliest record there is; the link is
    // in it, so no reader ever sees the job unparented.
    let created = store
        .read_events(&job.identity.job_id)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "created")
        .expect("created event");
    assert_eq!(created.data["parentJobId"], serde_json::json!("bg-parent"));
}

#[test]
fn launch_background_prompt_records_image_attachments() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);
    let job = launch_background_prompt(
        &store,
        BackgroundLaunchOptions {
            prompt: "inspect screenshot".into(),
            images: vec![BackgroundImageAttachment {
                id: 1,
                data: "abc".into(),
                media_type: "image/png".into(),
                filename: Some("shot.png".into()),
                source_path: None,
            }],
            cwd: PathBuf::from("."),
            isolate_in_worktree: false,
            require_worktree: false,
            preserve_worktree_on_success: false,
            queue_session: false,
            runtime: runtime(),
            name: None,
            agent_type: None,
            parent_job_id: None,
        },
        &rebon_exe(),
        allow_all,
    )
    .unwrap();

    let loaded = store.read_state(&job.identity.job_id).unwrap();
    assert_eq!(loaded.identity.prompt_images.len(), 1);
    assert!(!loaded.workspace.isolate_in_worktree);
    assert!(!loaded.workspace.require_worktree);
    assert!(!loaded.workspace.preserve_worktree_on_success);
    assert_eq!(loaded.identity.prompt_images[0].media_type, "image/png");
    assert!(matches!(
        loaded.identity.prompt_images[0].to_content_block(),
        rebon_types::ContentBlock::Image(image)
            if image.mime_type == "image/png" && image.data == "abc"
    ));
}

#[test]
fn queue_existing_session_records_resume_images_without_pending_copy() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);
    let existing = adopt_existing_background_session(
        &store,
        "old prompt".into(),
        PathBuf::from("."),
        runtime(),
        "sess-transcript".into(),
        None,
        false,
        JobPlacement::Background,
        &rebon_exe(),
    )
    .unwrap();
    store
        .update_state(&existing.identity.job_id, |state| {
            state.process.status = BackgroundJobStatus::Failed;
            state.outcome.error = Some("file locked".into());
            state.outcome.summary = Some(background_job_failure_summary("file locked"));
            Ok(())
        })
        .unwrap();
    let image = BackgroundImageAttachment {
        id: 11,
        data: "resume-image".into(),
        media_type: "image/jpeg".into(),
        filename: None,
        source_path: None,
    };
    let job = queue_existing_background_session_with_images(
        &store,
        "resume transcript".into(),
        None,
        vec![image.clone()],
        PathBuf::from("."),
        runtime(),
        "sess-transcript".into(),
        None,
        &rebon_exe(),
    )
    .unwrap();

    assert_eq!(job.identity.job_id, existing.identity.job_id);
    assert_eq!(job.identity.session_id.as_deref(), Some("sess-transcript"));
    assert_eq!(job.identity.prompt_images, vec![image]);
    assert!(job.identity.pending_prompts.is_empty());
    assert!(job.outcome.error.is_none());
    assert!(job.outcome.summary.is_none());
}

#[test]
fn queue_existing_session_preserves_identified_rich_prompt_on_reused_job() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);
    let existing = adopt_existing_background_session(
        &store,
        "old prompt".into(),
        PathBuf::from("."),
        runtime(),
        "sess-identified-reuse".into(),
        None,
        false,
        JobPlacement::Background,
        &rebon_exe(),
    )
    .unwrap();
    let image = BackgroundImageAttachment {
        id: 12,
        data: "identified-image".into(),
        media_type: "image/png".into(),
        filename: Some("shot.png".into()),
        source_path: None,
    };

    let job = queue_existing_background_session_with_images(
        &store,
        "identified resume".into(),
        Some("u-mobile-resume-rich".into()),
        vec![image.clone()],
        PathBuf::from("."),
        runtime(),
        "sess-identified-reuse".into(),
        None,
        &rebon_exe(),
    )
    .unwrap();

    assert_eq!(job.identity.job_id, existing.identity.job_id);
    assert_eq!(job.identity.prompt, "identified resume");
    assert!(job.identity.prompt_images.is_empty());
    assert_eq!(job.identity.pending_prompts.len(), 1);
    assert_eq!(job.identity.pending_prompts[0].id, "u-mobile-resume-rich");
    assert_eq!(job.identity.pending_prompts[0].text, "identified resume");
    assert_eq!(job.identity.pending_prompts[0].images, vec![image]);
}

#[test]
fn queue_existing_session_preserves_identified_text_prompt_on_new_job() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);

    let job = queue_existing_background_session_with_images(
        &store,
        "first identified resume".into(),
        Some("u-mobile-resume-text".into()),
        Vec::new(),
        PathBuf::from("."),
        runtime(),
        "sess-identified-new".into(),
        None,
        &rebon_exe(),
    )
    .unwrap();

    assert_eq!(job.process.status, BackgroundJobStatus::Queued);
    assert_eq!(
        job.identity.session_id.as_deref(),
        Some("sess-identified-new")
    );
    assert_eq!(job.identity.pending_prompts.len(), 1);
    assert_eq!(job.identity.pending_prompts[0].id, "u-mobile-resume-text");
    assert_eq!(
        job.identity.pending_prompts[0].text,
        "first identified resume"
    );
    assert!(job.identity.pending_prompts[0].images.is_empty());
    assert_eq!(store.list_jobs().unwrap().len(), 1);
}

#[test]
fn create_attached_job_records_idle_session_reference() {
    let (_dir, store) = store();
    let runtime = BackgroundRuntimeFields {
        model: Some("gpt".into()),
        ..runtime()
    };

    let job = create_attached_background_job(
        &store,
        "fix the bug".into(),
        Vec::new(),
        PathBuf::from("."),
        runtime,
        "sess-attached".into(),
        None,
        allow_all,
    )
    .unwrap();

    assert_eq!(job.process.status, BackgroundJobStatus::Idle);
    assert_eq!(job.identity.session_id.as_deref(), Some("sess-attached"));
    assert!(!store
        .job_dir(&job.identity.job_id)
        .join("transcript.jsonl")
        .exists());
}

#[test]
fn reply_to_idle_job_queues_pending_prompt_without_copying_transcript() {
    let (_dir, store) = store();
    let runtime = BackgroundRuntimeFields {
        model: Some("gpt".into()),
        ..runtime()
    };
    let idle = adopt_existing_background_session(
        &store,
        "Continue session sess-one".into(),
        PathBuf::from("."),
        runtime,
        "sess-one".into(),
        Some("session one".into()),
        false,
        JobPlacement::Background,
        &rebon_exe(),
    )
    .unwrap();
    store
        .update_state(&idle.identity.job_id, |state| {
            state.outcome.error = Some("file locked".into());
            state.outcome.summary = Some(background_job_failure_summary("file locked"));
            Ok(())
        })
        .unwrap();

    reply_to_background_job_in_store(
        &store,
        &idle.identity.job_id,
        "finish this".into(),
        false,
        true,
        &rebon_exe(),
    )
    .unwrap();

    let loaded = store.read_state(&idle.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
    assert_eq!(pending_text(&loaded), Some("finish this"));
    assert_eq!(loaded.identity.prompt, "Continue session sess-one");
    assert!(loaded.outcome.error.is_none());
    assert!(loaded.outcome.summary.is_none());
    assert!(!store
        .job_dir(&idle.identity.job_id)
        .join("transcript.jsonl")
        .exists());
}

#[test]
fn permission_answer_is_bound_to_the_full_endpoint_generation() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.process.pid = Some(std::process::id());
    state.process.pid_identity = process_identity(std::process::id());
    state.process.ipc_port = Some(41002);
    state.process.ipc_token = Some("replacement-endpoint-token".into());
    let replacement_endpoint = BackgroundIpcEndpoint {
        pid: std::process::id(),
        port: 41002,
        token: "replacement-endpoint-token".into(),
    };
    state.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
        query_id: 17,
        turn_generation: 2,
        endpoint: Some(replacement_endpoint),
        tool: Some("Read".into()),
        tool_call_id: Some("tool-replacement".into()),
        session_id: Some("session-replacement".into()),
        title: Some("Read files".into()),
        message: None,
        tool_input: None,
        metadata: None,
        options: vec![BackgroundPermissionOptionSnapshot {
            option_id: "allow_once".into(),
            label: "Allow once".into(),
            kind: "AllowOnce".into(),
        }],
    });
    store.write_state(&state).unwrap();
    let stale_endpoint = BackgroundIpcEndpoint {
        pid: std::process::id(),
        port: 41001,
        token: "old-endpoint-token".into(),
    };

    let error = store
        .answer_permission_query_for_endpoint_with_updated_input(
            &state.identity.job_id,
            17,
            Some(&stale_endpoint),
            Some("allow_once".into()),
            None,
            None,
        )
        .unwrap_err();

    assert!(error
        .to_string()
        .contains("different IPC endpoint generation"));
    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .outcome
            .pending_permission
            .as_ref()
            .map(|permission| permission.endpoint.as_ref()),
        Some(Some(&BackgroundIpcEndpoint {
            pid: std::process::id(),
            port: 41002,
            token: "replacement-endpoint-token".into(),
        }))
    );
}

#[test]
fn terminal_job_with_pending_permission_rejects_new_replies() {
    let (_dir, store) = store();
    let idle = adopt_existing_background_session(
        &store,
        "initial prompt".into(),
        PathBuf::from("."),
        runtime(),
        "sess-permission-terminal".into(),
        None,
        false,
        JobPlacement::Background,
        &rebon_exe(),
    )
    .unwrap();
    store
        .update_state(&idle.identity.job_id, |state| {
            state.process.status = BackgroundJobStatus::Succeeded;
            state.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
                query_id: 17,
                turn_generation: 1,
                endpoint: None,
                tool: Some("Read".into()),
                tool_call_id: Some("tool-17".into()),
                session_id: state.identity.session_id.clone(),
                title: Some("Read files".into()),
                message: None,
                tool_input: None,
                metadata: None,
                options: Vec::new(),
            });
            Ok(())
        })
        .unwrap();

    let reply_error = reply_to_background_job_in_store(
        &store,
        &idle.identity.job_id,
        "next turn".into(),
        false,
        false,
        &rebon_exe(),
    )
    .unwrap_err();
    assert!(reply_error.to_string().contains("waiting for permission"));
    let queue_error = queue_background_reply_if_inactive(
        &store,
        &idle.identity.job_id,
        "next turn".into(),
        &rebon_exe(),
    )
    .unwrap_err();
    assert!(queue_error.to_string().contains("waiting for permission"));

    let loaded = store.read_state(&idle.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
    assert_eq!(
        loaded
            .outcome
            .pending_permission
            .as_ref()
            .map(|pending| pending.query_id),
        Some(17)
    );
    assert!(loaded.identity.pending_prompts.is_empty());
}

#[test]
fn rich_reply_to_idle_job_updates_text_and_images_atomically() {
    let (_dir, store) = store();
    let idle = adopt_existing_background_session(
        &store,
        "initial prompt".into(),
        PathBuf::from("."),
        runtime(),
        "sess-rich".into(),
        None,
        false,
        JobPlacement::Background,
        &rebon_exe(),
    )
    .unwrap();
    let image = BackgroundImageAttachment {
        id: 4,
        data: "rich-image".into(),
        media_type: "image/webp".into(),
        filename: Some("followup.webp".into()),
        source_path: None,
    };

    reply_to_background_job_in_store_with_images(
        &store,
        &idle.identity.job_id,
        "inspect this".into(),
        vec![image.clone()],
        false,
        false,
        &rebon_exe(),
    )
    .unwrap();

    let loaded = store.read_state(&idle.identity.job_id).unwrap();
    assert_eq!(pending_text(&loaded), Some("inspect this"));
    assert_eq!(loaded.identity.pending_prompts[0].images, vec![image]);
    assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
}

#[test]
fn non_interrupting_reply_queues_only_inactive_jobs() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);
    let image = BackgroundImageAttachment {
        id: 7,
        data: "safe-image".into(),
        media_type: "image/png".into(),
        filename: Some("safe.png".into()),
        source_path: None,
    };

    for status in [
        BackgroundJobStatus::Idle,
        BackgroundJobStatus::Succeeded,
        BackgroundJobStatus::Failed,
        BackgroundJobStatus::Stopped,
    ] {
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.identity.session_id = Some(format!("session-{}", status.as_str()));
        state.process.status = status;
        state.outcome.error = Some("file locked".into());
        state.outcome.summary = Some(background_job_failure_summary("file locked"));
        store.write_state(&state).unwrap();

        let message = format!("continue from {}", status.as_str());
        queue_background_reply_if_inactive_with_images(
            &store,
            &state.identity.job_id,
            message.clone(),
            vec![image.clone()],
            &rebon_exe(),
        )
        .unwrap();

        let loaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
        assert_eq!(pending_text(&loaded), Some(message.as_str()));
        assert_eq!(
            loaded.identity.pending_prompts[0].images,
            vec![image.clone()]
        );
        assert!(loaded.outcome.error.is_none());
        assert!(loaded.outcome.summary.is_none());
        assert!(store
            .read_events_tail(&state.identity.job_id, 10)
            .unwrap()
            .iter()
            .any(|event| {
                event.kind == "reply_queued"
                    && event.data["nonInterrupting"] == serde_json::json!(true)
            }));
    }
}

#[test]
fn non_interrupting_reply_preserves_a_lingering_worker_owner() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("session-live-owner".into());
    state.process.status = BackgroundJobStatus::Succeeded;
    state.process.pid = Some(std::process::id());
    state.process.pid_identity = process_identity(std::process::id());
    state.process.ipc_port = Some(41001);
    state.process.ipc_token = Some("live-owner-token".into());
    store.write_state(&state).unwrap();

    queue_background_reply_if_inactive(
        &store,
        &state.identity.job_id,
        "continue on existing worker".into(),
        &rebon_exe(),
    )
    .unwrap();

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
    assert_eq!(loaded.process.pid, state.process.pid);
    assert_eq!(loaded.process.pid_identity, state.process.pid_identity);
    assert_eq!(loaded.process.ipc_port, state.process.ipc_port);
    assert_eq!(loaded.process.ipc_token, state.process.ipc_token);
}

#[test]
fn non_interrupting_reply_appends_to_active_jobs_without_changing_status() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);

    for status in [
        BackgroundJobStatus::Queued,
        BackgroundJobStatus::Running,
        BackgroundJobStatus::NeedsInput,
    ] {
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.identity.session_id = Some(format!("session-{}", status.as_str()));
        state.process.status = status;
        store.write_state(&state).unwrap();

        queue_background_reply_if_inactive(
            &store,
            &state.identity.job_id,
            "must not interrupt".into(),
            &rebon_exe(),
        )
        .unwrap();

        let loaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(loaded.process.status, status);
        assert_eq!(pending_text(&loaded), Some("must not interrupt"));
        assert!(store
            .read_events_tail(&state.identity.job_id, 10)
            .unwrap()
            .iter()
            .any(|event| event.kind == "reply_queued"));
    }
}

#[test]
fn reply_to_running_job_appends_without_interrupting() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    state.identity.session_id = Some("sess-one".into());
    store.write_state(&state).unwrap();

    reply_to_background_job_in_store(
        &store,
        &state.identity.job_id,
        "later".into(),
        false,
        true,
        &rebon_exe(),
    )
    .unwrap();

    let loaded = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(loaded.process.status, BackgroundJobStatus::Running);
    assert_eq!(pending_text(&loaded), Some("later"));
}

#[test]
fn respawn_creates_new_queued_job_without_copying_session() {
    let (_dir, store) = store();
    let mut state = store
        .create_job_with_name(
            "prompt".into(),
            PathBuf::from("."),
            BackgroundRuntimeFields {
                provider: Some("openai".into()),
                model: Some("gpt".into()),
                fast_mode: None,
                channels: vec!["server:one".into()],
                development_channels: Vec::new(),
                provider_format: None,
                ui_mode: Some("inline".into()),
                effort_level: None,
                permission_mode: None,
                capability_mode: rebon_types::AgentCapabilityMode::Normal,
                settings: Vec::new(),
                add_dirs: Vec::new(),
                plugin_dirs: Vec::new(),
                mcp_configs: Vec::new(),
                strict_mcp_config: false,
            },
            Some("rerunnable".into()),
        )
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.identity.session_id = Some("sess-old".into());
    state.identity.agent_type = Some("code-reviewer".into());
    state.process.completed_at_ms = Some(now_ms());
    store.write_state(&state).unwrap();

    let new_job =
        respawn_background_job_in_store(&store, &state.identity.job_id, false, &rebon_exe())
            .unwrap();

    assert_ne!(new_job.identity.job_id, state.identity.job_id);
    assert_eq!(new_job.process.status, BackgroundJobStatus::Queued);
    assert_eq!(new_job.identity.session_id, None);
    assert_eq!(new_job.identity.prompt, state.identity.prompt);
    assert_eq!(new_job.identity.name, "rerunnable");
    assert_eq!(new_job.identity.runtime, state.identity.runtime);
    assert_eq!(
        new_job.identity.agent_type.as_deref(),
        Some("code-reviewer")
    );
    let source_events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
    assert!(source_events.iter().any(|event| event.kind == "respawned"));
    let new_events = store
        .read_events_tail(&new_job.identity.job_id, 10)
        .unwrap();
    assert!(new_events.iter().any(|event| event.kind == "respawn_of"));
}

#[test]
fn presentation_update_pins_renames_and_reorders_jobs() {
    let (_dir, store) = store();
    let first = store
        .create_job("first prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let second = store
        .create_job("second prompt".into(), PathBuf::from("."), runtime())
        .unwrap();

    let renamed = store
        .update_job_presentation(
            &first.identity.job_id,
            Some("renamed".into()),
            Some(true),
            None,
        )
        .unwrap();
    assert_eq!(renamed.identity.name, "renamed");
    assert!(renamed.identity.pinned);
    store
        .update_job_presentation(&first.identity.job_id, None, Some(false), None)
        .unwrap();

    store
        .move_job_before(&second.identity.job_id, &first.identity.job_id)
        .unwrap();
    let jobs = store.list_jobs().unwrap();
    let second_index = jobs
        .iter()
        .position(|job| job.identity.job_id == second.identity.job_id)
        .unwrap();
    let first_index = jobs
        .iter()
        .position(|job| job.identity.job_id == first.identity.job_id)
        .unwrap();
    assert!(second_index < first_index);
}

#[test]
fn respawn_all_skips_active_jobs() {
    let (_dir, store) = store();
    let mut finished = store
        .create_job_with_name(
            "finished prompt".into(),
            PathBuf::from("."),
            BackgroundRuntimeFields {
                provider: Some("openai".into()),
                model: Some("gpt".into()),
                fast_mode: None,
                channels: Vec::new(),
                development_channels: Vec::new(),
                provider_format: None,
                ui_mode: None,
                effort_level: Some("high".into()),
                permission_mode: Some("plan".into()),
                capability_mode: rebon_types::AgentCapabilityMode::Normal,
                settings: Vec::new(),
                add_dirs: Vec::new(),
                plugin_dirs: Vec::new(),
                mcp_configs: Vec::new(),
                strict_mcp_config: false,
            },
            Some("finished".into()),
        )
        .unwrap();
    finished.process.status = BackgroundJobStatus::Failed;
    finished.identity.session_id = Some("sess-finished".into());
    store.write_state(&finished).unwrap();
    let running = store
        .create_job("running prompt".into(), PathBuf::from("."), runtime())
        .unwrap();

    let new_jobs = respawn_all_background_jobs_in_store(&store, false, &rebon_exe()).unwrap();

    assert_eq!(new_jobs.len(), 1);
    assert_ne!(new_jobs[0].identity.job_id, finished.identity.job_id);
    assert_eq!(new_jobs[0].identity.name, "finished");
    assert_eq!(new_jobs[0].identity.session_id, None);
    assert_eq!(new_jobs[0].identity.runtime, finished.identity.runtime);
    let running_events = store
        .read_events_tail(&running.identity.job_id, 10)
        .unwrap();
    assert!(!running_events.iter().any(|event| event.kind == "respawned"));
    let finished_events = store
        .read_events_tail(&finished.identity.job_id, 10)
        .unwrap();
    assert!(finished_events
        .iter()
        .any(|event| event.kind == "respawned"));
}

#[test]
fn respawn_rejects_active_jobs() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Running;
    store.write_state(&state).unwrap();

    let err = respawn_background_job_in_store(&store, &state.identity.job_id, false, &rebon_exe())
        .unwrap_err();
    assert!(err.to_string().contains("still active"));
}

#[test]
fn removal_reservation_rejects_new_work_before_metadata_is_deleted() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    store.write_state(&state).unwrap();

    let reserved = store.reserve_job_removal(&state.identity.job_id).unwrap();
    assert!(reserved.process.removal_reserved);
    let error = reply_to_background_job_in_store_with_images(
        &store,
        &state.identity.job_id,
        "late follow-up".into(),
        Vec::new(),
        false,
        false,
        &rebon_exe(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("reserved for removal"));
    let current = store.read_state(&state.identity.job_id).unwrap();
    assert!(current.process.removal_reserved);
    assert!(current.identity.pending_prompts.is_empty());
}

#[test]
fn concurrent_remove_and_reply_commit_exactly_one_outcome() {
    for iteration in 0..20 {
        let (_dir, store) = store();
        let mut state = store
            .create_job(format!("prompt {iteration}"), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Succeeded;
        store.write_state(&state).unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let remove_store = store.clone();
        let remove_job_id = state.identity.job_id.clone();
        let remove_barrier = barrier.clone();
        let remove = std::thread::spawn(move || {
            remove_barrier.wait();
            remove_store.remove_job(&remove_job_id)
        });
        let reply_store = store.clone();
        let reply_job_id = state.identity.job_id.clone();
        let reply_barrier = barrier.clone();
        let reply = std::thread::spawn(move || {
            reply_barrier.wait();
            reply_to_background_job_in_store_with_images(
                &reply_store,
                &reply_job_id,
                "accepted follow-up".into(),
                Vec::new(),
                false,
                false,
                &rebon_exe(),
            )
        });
        barrier.wait();

        let removed = remove.join().unwrap().is_ok();
        let replied = reply.join().unwrap().is_ok();
        assert_ne!(removed, replied, "iteration {iteration}");
        if removed {
            assert!(!store.state_path(&state.identity.job_id).exists());
        } else {
            let current = store.read_state(&state.identity.job_id).unwrap();
            assert_eq!(current.process.status, BackgroundJobStatus::Queued);
            assert_eq!(pending_text(&current), Some("accepted follow-up"));
            assert!(!current.process.removal_reserved);
        }
    }
}

#[test]
fn linked_respawn_source_cannot_be_removed_before_target_materializes() {
    let (_dir, store) = store();
    let mut source = store
        .create_job("original prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let follow_up_image = BackgroundImageAttachment {
        id: 17,
        data: "accepted-image".into(),
        media_type: "image/png".into(),
        filename: None,
        source_path: None,
    };
    source.process.status = BackgroundJobStatus::Failed;
    source.identity.session_id = Some("sess-linked-respawn".into());
    source.identity.pending_prompts = vec![pending_prompt(
        "pp-linked-respawn",
        "accepted follow-up",
        vec![follow_up_image.clone()],
    )];
    let target_job_id = generate_job_id();
    source.identity.respawned_job_id = Some(target_job_id.clone());
    store.write_state(&source).unwrap();

    let remove_error = store.remove_job(&source.identity.job_id).unwrap_err();
    assert!(remove_error.to_string().contains("not fully materialized"));
    let preserved = store.read_state(&source.identity.job_id).unwrap();
    assert_eq!(pending_text(&preserved), Some("accepted follow-up"));
    assert_eq!(
        preserved.identity.pending_prompts[0].images,
        vec![follow_up_image.clone()]
    );
    assert!(!preserved.process.removal_reserved);

    let mut unrelated_target =
        BackgroundJobState::new("unrelated target".into(), ".".into(), runtime(), None);
    unrelated_target.identity.job_id = target_job_id.clone();
    unrelated_target.process.status = BackgroundJobStatus::Failed;
    store.write_state(&unrelated_target).unwrap();
    let mismatched_error = store.remove_job(&source.identity.job_id).unwrap_err();
    assert!(mismatched_error
        .to_string()
        .contains("not fully materialized"));
    assert_eq!(
        store
            .read_state(&source.identity.job_id)
            .unwrap()
            .pending_prompt()
            .map(|prompt| prompt.text.clone()),
        Some("accepted follow-up".to_string())
    );
    store.remove_job(&target_job_id).unwrap();

    let respawned =
        respawn_background_job_in_store(&store, &source.identity.job_id, false, &rebon_exe())
            .unwrap();
    assert_eq!(respawned.identity.job_id, target_job_id);
    assert_eq!(respawned.identity.prompt, "original prompt");
    assert_eq!(pending_text(&respawned), Some("accepted follow-up"));
    assert_eq!(
        respawned.identity.pending_prompts[0].images,
        vec![follow_up_image]
    );

    store.remove_job(&source.identity.job_id).unwrap();
    assert!(!store.state_path(&source.identity.job_id).exists());
    assert!(store.state_path(&target_job_id).exists());
}

#[test]
fn concurrent_respawns_share_one_durable_target() {
    let (_dir, store) = store();
    let mut source = store
        .create_job("original prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    source.process.status = BackgroundJobStatus::Failed;
    source.identity.session_id = Some("sess-concurrent-respawn".into());
    source.identity.pending_prompts = vec![pending_prompt(
        "pp-concurrent-respawn",
        "accepted follow-up",
        Vec::new(),
    )];
    store.write_state(&source).unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let first_store = store.clone();
    let first_job_id = source.identity.job_id.clone();
    let first_barrier = barrier.clone();
    let first = std::thread::spawn(move || {
        first_barrier.wait();
        respawn_background_job_in_store(&first_store, &first_job_id, false, &rebon_exe()).unwrap()
    });
    let second_store = store.clone();
    let second_job_id = source.identity.job_id.clone();
    let second_barrier = barrier.clone();
    let second = std::thread::spawn(move || {
        second_barrier.wait();
        respawn_background_job_in_store(&second_store, &second_job_id, false, &rebon_exe()).unwrap()
    });
    barrier.wait();

    let first = first.join().unwrap();
    let second = second.join().unwrap();
    assert_eq!(first.identity.job_id, second.identity.job_id);
    assert_eq!(first.identity.prompt, "original prompt");
    assert_eq!(pending_text(&first), Some("accepted follow-up"));
    assert_eq!(store.list_jobs().unwrap().len(), 2);
    let linked = store.read_state(&source.identity.job_id).unwrap();
    assert_eq!(
        linked.identity.respawned_job_id.as_deref(),
        Some(first.identity.job_id.as_str())
    );
    let reply_error = reply_to_background_job_in_store_with_images(
        &store,
        &source.identity.job_id,
        "duplicate work".into(),
        Vec::new(),
        false,
        false,
        &rebon_exe(),
    )
    .unwrap_err();
    assert!(reply_error.to_string().contains("already respawned"));
    assert!(
        respawn_all_background_jobs_in_store(&store, false, &rebon_exe())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn malformed_pending_prompt_items_recover_fifo_independently() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let mut json = serde_json::to_value(&state).unwrap();
    let object = json.as_object_mut().unwrap();
    object.insert("status".into(), serde_json::json!("running"));
    object.insert("turnGeneration".into(), serde_json::json!(7));
    object.insert(
        "pendingPrompts".into(),
        serde_json::json!([
            {
                "id": "pp-kept",
                "text": " first ",
                "images": [],
                "enqueuedAtMs": 1,
                "claimedTurnGeneration": 7
            },
            {
                "id": "pp-kept",
                "text": "second",
                "images": "malformed",
                "enqueuedAtMs": "bad",
                "claimedTurnGeneration": 0
            },
            {
                "id": "../invalid",
                "text": "third",
                "claimedTurnGeneration": 9
            },
            {"id": "pp-missing-text", "images": []},
            42,
            "fourth"
        ]),
    );
    fs::write(
        store.state_path(&state.identity.job_id),
        serde_json::to_vec_pretty(&json).unwrap(),
    )
    .unwrap();

    let recovered = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        recovered
            .identity
            .pending_prompts
            .iter()
            .map(|prompt| prompt.text.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second", "third", "fourth"]
    );
    assert_eq!(recovered.identity.pending_prompts[0].id, "pp-kept");
    assert_eq!(
        recovered.identity.pending_prompts[0].claimed_turn_generation,
        Some(7)
    );
    assert!(recovered.identity.pending_prompts[1..]
        .iter()
        .all(|prompt| prompt.claimed_turn_generation.is_none()));
    assert!(recovered.identity.pending_prompts[1].images.is_empty());
    assert!(recovered.identity.pending_prompts[1].enqueued_at_ms > 0);
    let ids = recovered
        .identity
        .pending_prompts
        .iter()
        .map(|prompt| prompt.id.as_str())
        .collect::<HashSet<_>>();
    assert_eq!(ids.len(), recovered.identity.pending_prompts.len());
    assert!(recovered
        .identity
        .pending_prompts
        .iter()
        .all(|prompt| validate_pending_prompt_id(&prompt.id).is_ok()));

    store
        .update_state(&state.identity.job_id, |_| Ok(()))
        .unwrap();
    let rewritten: serde_json::Value =
        serde_json::from_slice(&fs::read(store.state_path(&state.identity.job_id)).unwrap())
            .unwrap();
    assert_eq!(rewritten["pendingPrompts"].as_array().unwrap().len(), 4);

    let mut invalid_top_level = rewritten;
    invalid_top_level["pendingPrompts"] = serde_json::json!({"text": "not an array"});
    fs::write(
        store.state_path(&state.identity.job_id),
        serde_json::to_vec_pretty(&invalid_top_level).unwrap(),
    )
    .unwrap();
    assert!(store.read_state(&state.identity.job_id).is_err());
}

#[test]
fn malformed_pending_prompt_image_does_not_drop_valid_siblings() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let mut json = serde_json::to_value(&state).unwrap();
    json["pendingPrompts"] = serde_json::json!([{
        "id": "pp-images",
        "text": "with images",
        "images": [
            {
                "id": 1,
                "data": "valid-image",
                "mediaType": "image/png"
            },
            {"id": "invalid"}
        ],
        "enqueuedAtMs": 1
    }]);
    fs::write(
        store.state_path(&state.identity.job_id),
        serde_json::to_vec_pretty(&json).unwrap(),
    )
    .unwrap();

    let recovered = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(recovered.identity.pending_prompts[0].images.len(), 1);
    assert_eq!(
        recovered.identity.pending_prompts[0].images[0].data,
        "valid-image"
    );
}

#[test]
fn pending_prompt_generation_recovery_drops_untrusted_completion_ack() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let mut base = serde_json::to_value(&state).unwrap();
    base["turnGeneration"] = serde_json::json!(7);
    let read_prompt = |claimed: serde_json::Value, completed: serde_json::Value| {
        let mut json = base.clone();
        json["pendingPrompts"] = serde_json::json!([{
            "id": "pp-generation",
            "text": "recover me",
            "enqueuedAtMs": 1,
            "claimedTurnGeneration": claimed,
            "completedTurnGeneration": completed
        }]);
        fs::write(
            store.state_path(&state.identity.job_id),
            serde_json::to_vec_pretty(&json).unwrap(),
        )
        .unwrap();
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .identity
            .pending_prompts
            .remove(0)
    };

    let completion_without_claim = read_prompt(serde_json::Value::Null, serde_json::json!(1));
    assert_eq!(completion_without_claim.claimed_turn_generation, None);
    assert_eq!(completion_without_claim.completed_turn_generation, None);

    let completion_newer_than_claim = read_prompt(serde_json::json!(3), serde_json::json!(4));
    assert_eq!(completion_newer_than_claim.claimed_turn_generation, Some(3));
    assert_eq!(completion_newer_than_claim.completed_turn_generation, None);

    let future_claim = read_prompt(serde_json::json!(8), serde_json::json!(7));
    assert_eq!(future_claim.claimed_turn_generation, None);
    assert_eq!(future_claim.completed_turn_generation, None);

    let reclaimed_after_completion = read_prompt(serde_json::json!(5), serde_json::json!(3));
    assert_eq!(reclaimed_after_completion.claimed_turn_generation, Some(5));
    assert_eq!(
        reclaimed_after_completion.completed_turn_generation,
        Some(3)
    );
}

#[test]
fn pending_prompt_writes_and_appends_remain_strict() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let expected = store.read_state(&state.identity.job_id).unwrap();

    let mut invalid_id = expected.clone();
    invalid_id.identity.pending_prompts = vec![PendingPrompt {
        id: "../invalid".into(),
        text: "recoverable only when read from disk".into(),
        images: Vec::new(),
        coordinator_report_paths: Vec::new(),
        enqueued_at_ms: now_ms(),
        claimed_turn_generation: None,
        completed_turn_generation: None,
    }];
    assert!(store.write_state(&invalid_id).is_err());
    assert_eq!(store.read_state(&state.identity.job_id).unwrap(), expected);

    let duplicate = pending_prompt("pp-duplicate", "first", Vec::new());
    let mut duplicate_ids = expected.clone();
    duplicate_ids.identity.pending_prompts = vec![
        duplicate.clone(),
        PendingPrompt {
            text: "second".into(),
            ..duplicate
        },
    ];
    assert!(store.write_state(&duplicate_ids).is_err());
    assert_eq!(store.read_state(&state.identity.job_id).unwrap(), expected);

    let mut illegal_claim = expected.clone();
    let mut claimed = pending_prompt("pp-claimed", "claimed", Vec::new());
    claimed.claimed_turn_generation = Some(0);
    illegal_claim.identity.pending_prompts = vec![claimed.clone()];
    assert!(store.write_state(&illegal_claim).is_err());
    assert_eq!(store.read_state(&state.identity.job_id).unwrap(), expected);

    let mut completion_without_claim = expected.clone();
    completion_without_claim.process.turn_generation = 7;
    let mut unclaimed = pending_prompt("pp-unclaimed-completion", "unclaimed", Vec::new());
    unclaimed.completed_turn_generation = Some(1);
    completion_without_claim.identity.pending_prompts = vec![unclaimed];
    assert!(store.write_state(&completion_without_claim).is_err());
    assert_eq!(store.read_state(&state.identity.job_id).unwrap(), expected);

    let mut completion_newer_than_claim = expected.clone();
    completion_newer_than_claim.process.turn_generation = 7;
    let mut completed = pending_prompt("pp-newer-completion", "completed", Vec::new());
    completed.claimed_turn_generation = Some(3);
    completed.completed_turn_generation = Some(4);
    completion_newer_than_claim.identity.pending_prompts = vec![completed];
    assert!(store.write_state(&completion_newer_than_claim).is_err());
    assert_eq!(store.read_state(&state.identity.job_id).unwrap(), expected);

    let mut future_claim = expected.clone();
    future_claim.process.turn_generation = 7;
    let mut future = pending_prompt("pp-future-claim", "future", Vec::new());
    future.claimed_turn_generation = Some(8);
    future_claim.identity.pending_prompts = vec![future.clone()];
    assert!(store.write_state(&future_claim).is_err());
    assert_eq!(store.read_state(&state.identity.job_id).unwrap(), expected);

    let mut append_target = expected.clone();
    assert!(append_target.append_pending_prompt(claimed).is_err());
    assert!(append_target.append_pending_prompt(future).is_err());
    assert!(append_target.identity.pending_prompts.is_empty());
}

#[test]
fn pending_prompt_claims_are_a_single_generation_prefix() {
    let mut state = BackgroundJobState::new("prompt".into(), ".".into(), runtime(), None);
    state.process.turn_generation = 7;
    let mut first = pending_prompt("pp-prefix-a", "first", Vec::new());
    first.claimed_turn_generation = Some(7);
    first.completed_turn_generation = Some(7);
    let mut second = pending_prompt("pp-prefix-b", "second", Vec::new());
    second.claimed_turn_generation = Some(7);
    state.identity.pending_prompts = vec![
        first,
        second,
        pending_prompt("pp-prefix-c", "third", Vec::new()),
    ];
    state.validate_pending_prompts().unwrap();

    let mut gap = state.clone();
    gap.identity.pending_prompts[1].claimed_turn_generation = None;
    gap.identity.pending_prompts[2].claimed_turn_generation = Some(7);
    assert!(gap.validate_pending_prompts().is_err());

    let mut mixed_generation = state;
    mixed_generation.identity.pending_prompts[1].claimed_turn_generation = Some(6);
    assert!(mixed_generation.validate_pending_prompts().is_err());
}

#[test]
fn pending_prompt_normalization_keeps_valid_claimed_prefix_and_drops_claimed_gap() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let mut json = serde_json::to_value(&state).unwrap();
    json["turnGeneration"] = serde_json::json!(4);
    json["pendingPrompts"] = serde_json::json!([
        {
            "id": "pp-normalize-a",
            "text": "first",
            "enqueuedAtMs": 1,
            "claimedTurnGeneration": 4
        },
        {
            "id": "pp-normalize-b",
            "text": "second",
            "enqueuedAtMs": 2,
            "claimedTurnGeneration": 4
        },
        {
            "id": "pp-normalize-c",
            "text": "third",
            "enqueuedAtMs": 3
        },
        {
            "id": "pp-normalize-d",
            "text": "fourth",
            "enqueuedAtMs": 4,
            "claimedTurnGeneration": 4
        }
    ]);
    fs::write(
        store.state_path(&state.identity.job_id),
        serde_json::to_vec_pretty(&json).unwrap(),
    )
    .unwrap();

    let normalized = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        normalized
            .identity
            .pending_prompts
            .iter()
            .map(|prompt| prompt.claimed_turn_generation)
            .collect::<Vec<_>>(),
        vec![Some(4), Some(4), None, None]
    );
}

/// A job file written by v0.4.0 or earlier still opens, and the one thing
/// that shape carried which this version no longer models — the single
/// `pendingPrompt` string and its `pendingPromptImages` sidecar — is
/// dropped instead of being rewritten into the queue. The job itself, its
/// transcript and its status survive; only the un-run follow-up is gone.
#[test]
fn a_pre_0_4_1_job_file_opens_without_its_single_pending_prompt() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    let image = BackgroundImageAttachment {
        id: 41,
        data: "old-image".into(),
        media_type: "image/png".into(),
        filename: Some("old.png".into()),
        source_path: None,
    };
    let mut json = serde_json::to_value(&state).unwrap();
    let object = json.as_object_mut().unwrap();
    object.remove("pendingPrompts");
    object.insert("pendingPrompt".into(), serde_json::json!("old follow-up"));
    object.insert(
        "pendingPromptImages".into(),
        serde_json::to_value(vec![image]).unwrap(),
    );
    object.insert("status".into(), serde_json::json!("running"));
    object.insert("turnGeneration".into(), serde_json::json!(7));
    std::fs::write(
        store.state_path(&state.identity.job_id),
        serde_json::to_string_pretty(&json).unwrap(),
    )
    .unwrap();

    let opened = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(opened.identity.job_id, state.identity.job_id);
    assert_eq!(opened.identity.prompt, "prompt");
    assert!(opened.identity.pending_prompts.is_empty());

    store
        .update_state(&state.identity.job_id, |_| Ok(()))
        .unwrap();
    let rewritten: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(store.state_path(&state.identity.job_id)).unwrap(),
    )
    .unwrap();
    assert!(rewritten.get("pendingPrompts").is_none());
    assert!(rewritten.get("pendingPrompt").is_none());
    assert!(rewritten.get("pendingPromptImages").is_none());
}

#[test]
fn explicit_pending_prompt_ids_are_fifo_and_idempotent() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-fifo".into());
    state.process.status = BackgroundJobStatus::Idle;
    store.write_state(&state).unwrap();

    let first = append_background_prompt_with_images(
        &store,
        &state.identity.job_id,
        "pp-first".into(),
        "first".into(),
        Vec::new(),
        false,
        &rebon_exe(),
    )
    .unwrap();
    let second = append_background_prompt_with_images(
        &store,
        &state.identity.job_id,
        "pp-second".into(),
        "second".into(),
        Vec::new(),
        false,
        &rebon_exe(),
    )
    .unwrap();
    let duplicate = append_background_prompt_with_images(
        &store,
        &state.identity.job_id,
        "pp-first".into(),
        "first".into(),
        Vec::new(),
        false,
        &rebon_exe(),
    )
    .unwrap();

    assert!(first.appended);
    assert!(second.appended);
    assert!(!duplicate.appended);
    let queued = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        queued
            .identity
            .pending_prompts
            .iter()
            .map(|prompt| (prompt.id.as_str(), prompt.text.as_str()))
            .collect::<Vec<_>>(),
        vec![("pp-first", "first"), ("pp-second", "second")]
    );
    let expected = queued.clone();
    let conflict = append_background_prompt_with_images(
        &store,
        &state.identity.job_id,
        "pp-first".into(),
        "different".into(),
        Vec::new(),
        false,
        &rebon_exe(),
    )
    .unwrap_err();
    assert!(conflict.to_string().contains("different content"));
    assert_eq!(store.read_state(&state.identity.job_id).unwrap(), expected);
}

#[test]
fn internal_pending_prompt_round_trips_report_paths_and_checks_identity() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-internal-prompt".into());
    state.process.status = BackgroundJobStatus::Running;
    store.write_state(&state).unwrap();

    let accepted = append_background_internal_prompt(
        &store,
        &state.identity.job_id,
        "u-internal-task-notification-1".into(),
        "<task-notification />".into(),
        vec!["C:/reports/worker.md".into()],
        false,
        &rebon_exe(),
    )
    .unwrap();
    assert!(accepted.appended);
    let stored = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        stored.identity.pending_prompts[0].coordinator_report_paths,
        vec!["C:/reports/worker.md"]
    );

    let duplicate = append_background_internal_prompt(
        &store,
        &state.identity.job_id,
        "u-internal-task-notification-1".into(),
        "<task-notification />".into(),
        vec!["C:/reports/worker.md".into()],
        false,
        &rebon_exe(),
    )
    .unwrap();
    assert!(!duplicate.appended);

    let conflict = append_background_internal_prompt(
        &store,
        &state.identity.job_id,
        "u-internal-task-notification-1".into(),
        "<task-notification />".into(),
        vec!["C:/reports/different.md".into()],
        false,
        &rebon_exe(),
    )
    .unwrap_err();
    assert!(conflict.to_string().contains("different content"));

    let invalid = append_background_internal_prompt(
        &store,
        &state.identity.job_id,
        "pp-not-internal".into(),
        "<task-notification />".into(),
        Vec::new(),
        false,
        &rebon_exe(),
    )
    .unwrap_err();
    assert!(invalid.to_string().contains("u-internal-"));
}

/// The supervisor spawn must never accept a target the OS would have
/// to go looking for. A bare name resolves against the working
/// directory and PATH, so a same-named build artifact — on Windows,
/// `Rebon.exe` and `rebon.exe` are one file — gets launched in the
/// CLI's place. Every layer above this one is blind to that: spawn
/// reports success for any created process, the roster gate only
/// sees "no supervisor yet", and the callers retry without bound.
#[test]
fn supervisor_spawn_refuses_targets_the_os_would_have_to_search_for() {
    let (_dir, store) = store();

    let bare = spawn_supervisor_process(&store, Path::new("rebon")).unwrap_err();
    assert!(bare.to_string().contains("bare program name"), "{bare:#}");

    let missing =
        spawn_supervisor_process(&store, Path::new("./__no-such-supervisor-exe__")).unwrap_err();
    assert!(
        missing.to_string().contains("cannot be resolved"),
        "{missing:#}"
    );

    fs::create_dir_all(store.daemon_dir()).unwrap();
    let dir_target = spawn_supervisor_process(&store, store.daemon_dir().as_path()).unwrap_err();
    assert!(
        dir_target.to_string().contains("is not a file"),
        "{dir_target:#}"
    );
}

/// A supervisor needs a moment to register itself, and the callers
/// re-check every couple of seconds. Without a cooldown "not
/// registered yet" reads as "never started", so each check spawns
/// another one.
#[test]
fn supervisor_spawn_cooldown_absorbs_checks_while_a_new_one_registers() {
    let (_dir, store) = store();
    fs::create_dir_all(store.daemon_dir()).unwrap();

    assert!(!supervisor_spawn_is_cooling_down(&store));
    record_supervisor_spawn(&store);
    assert!(supervisor_spawn_is_cooling_down(&store));

    // Inside the cooldown the caller is told everything is fine
    // rather than being handed the unspawnable path's error.
    ensure_supervisor_running(&store, &rebon_exe()).unwrap();

    fs::write(
        store.supervisor_spawn_stamp_path(),
        (now_ms() - SUPERVISOR_SPAWN_COOLDOWN_MS - 1).to_string(),
    )
    .unwrap();
    assert!(!supervisor_spawn_is_cooling_down(&store));

    // A stamp from the future (clock change) must not wedge it shut.
    fs::write(
        store.supervisor_spawn_stamp_path(),
        (now_ms() + 60_000).to_string(),
    )
    .unwrap();
    assert!(!supervisor_spawn_is_cooling_down(&store));

    fs::write(store.supervisor_spawn_stamp_path(), "not-a-timestamp").unwrap();
    assert!(!supervisor_spawn_is_cooling_down(&store));
}

/// Pending prompts are drained once claimed. The grant list is what
/// keeps an earlier worker's report readable on a later turn, so it
/// must outlive the prompt that announced it.
#[test]
fn coordinator_report_grants_outlive_the_prompt_that_announced_them() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-grants".into());
    state.process.status = BackgroundJobStatus::Running;
    store.write_state(&state).unwrap();

    for (id, path) in [
        ("u-internal-task-notification-1", "C:/reports/first.md"),
        ("u-internal-task-notification-2", "C:/reports/second.md"),
    ] {
        append_background_internal_prompt(
            &store,
            &state.identity.job_id,
            id.into(),
            "<task-notification />".into(),
            vec![path.into()],
            false,
            &rebon_exe(),
        )
        .unwrap();
    }

    let mut stored = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        stored.identity.coordinator_report_grants,
        vec!["C:/reports/first.md", "C:/reports/second.md"]
    );

    stored.clear_pending_prompts();
    store.write_state(&stored).unwrap();
    let after_claim = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(
        after_claim.identity.coordinator_report_grants,
        vec!["C:/reports/first.md", "C:/reports/second.md"],
        "grants must survive the pending prompt being consumed"
    );
}

#[test]
fn coordinator_report_grants_dedupe_and_stay_bounded() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();

    state.grant_coordinator_report_paths(["C:/reports/a.md", "C:/reports/a.md", "  "]);
    assert_eq!(
        state.identity.coordinator_report_grants,
        vec!["C:/reports/a.md"]
    );

    let many = (0..MAX_COORDINATOR_REPORT_GRANTS + 10)
        .map(|index| format!("C:/reports/bulk-{index}.md"))
        .collect::<Vec<_>>();
    state.grant_coordinator_report_paths(many);
    assert_eq!(
        state.identity.coordinator_report_grants.len(),
        MAX_COORDINATOR_REPORT_GRANTS
    );
    assert_eq!(
        state.identity.coordinator_report_grants.last().unwrap(),
        &format!(
            "C:/reports/bulk-{}.md",
            MAX_COORDINATOR_REPORT_GRANTS + 10 - 1
        ),
        "the newest grant is the one that must survive"
    );
    assert!(!state
        .identity
        .coordinator_report_grants
        .contains(&"C:/reports/a.md".to_string()));
}

#[test]
fn duplicate_pending_prompt_retries_supervisor_wakeup_after_spawn_failure() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-supervisor-retry".into());
    state.process.status = BackgroundJobStatus::Idle;
    store.write_state(&state).unwrap();
    let missing_exe = Path::new("definitely-missing-rebon-executable");

    let first_error = append_background_prompt_with_images(
        &store,
        &state.identity.job_id,
        "pp-supervisor-retry".into(),
        "retry supervisor".into(),
        Vec::new(),
        true,
        missing_exe,
    )
    .unwrap_err();
    assert!(!first_error.to_string().is_empty());
    let after_first = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(after_first.process.status, BackgroundJobStatus::Queued);
    assert_eq!(after_first.identity.pending_prompts.len(), 1);

    let retry_error = append_background_prompt_with_images(
        &store,
        &state.identity.job_id,
        "pp-supervisor-retry".into(),
        "retry supervisor".into(),
        Vec::new(),
        true,
        missing_exe,
    )
    .unwrap_err();
    assert!(!retry_error.to_string().is_empty());
    assert_eq!(
        store
            .read_state(&state.identity.job_id)
            .unwrap()
            .identity
            .pending_prompts,
        after_first.identity.pending_prompts
    );
}

#[test]
fn stale_running_and_needs_input_replies_requeue_for_supervisor() {
    let (_dir, store) = store();
    seed_live_supervisor(&store);
    let current_pid = std::process::id();
    let (stale_pid, stale_identity) = match process_identity(current_pid) {
        Some(identity) => (current_pid, Some(format!("{identity}-stale"))),
        None => (u32::MAX - 1, None),
    };
    let definitely_stale = match stale_identity.as_deref() {
        Some(identity) => {
            matches!(
                recorded_process_is_running(stale_pid, Some(identity)),
                Ok(false)
            )
        }
        None => process_is_running(stale_pid) == Some(false),
    };
    if !definitely_stale {
        return;
    }

    for (index, status) in [
        BackgroundJobStatus::Running,
        BackgroundJobStatus::NeedsInput,
    ]
    .into_iter()
    .enumerate()
    {
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.identity.session_id = Some(format!("sess-stale-{index}"));
        state.process.status = status;
        state.process.turn_generation = 5;
        state.process.pid = Some(stale_pid);
        state.process.pid_identity = stale_identity.clone();
        store.write_state(&state).unwrap();

        let acceptance = append_background_prompt_with_images(
            &store,
            &state.identity.job_id,
            format!("pp-stale-{index}"),
            format!("reply {index}"),
            Vec::new(),
            true,
            &rebon_exe(),
        )
        .unwrap();
        assert!(acceptance.appended);

        let queued = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(queued.process.status, BackgroundJobStatus::Queued);
        assert_eq!(queued.process.pid, None);
        assert_eq!(queued.process.pid_identity, None);
        assert_eq!(queued.identity.pending_prompts, vec![acceptance.prompt]);
        let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
        assert!(events
            .iter()
            .any(|event| event.kind == "stale_pid_reconciled"));
        assert!(events.iter().any(|event| {
            event.kind == "reply_queued" && event.data["resumed"] == serde_json::json!(true)
        }));
    }
}

#[test]
fn concurrent_pending_appends_do_not_overwrite_or_interrupt_running_owner() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-concurrent-append".into());
    state.process.status = BackgroundJobStatus::Running;
    state.process.turn_generation = 9;
    state.process.pid = Some(std::process::id());
    state.process.pid_identity = process_identity(std::process::id());
    state.process.ipc_port = Some(41234);
    state.process.ipc_token = Some("owner-token".into());
    store.write_state(&state).unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let first_store = store.clone();
    let first_job_id = state.identity.job_id.clone();
    let first_barrier = barrier.clone();
    let first = std::thread::spawn(move || {
        first_barrier.wait();
        append_background_prompt_with_images(
            &first_store,
            &first_job_id,
            "pp-concurrent-a".into(),
            "from app".into(),
            Vec::new(),
            false,
            &rebon_exe(),
        )
    });
    let second_store = store.clone();
    let second_job_id = state.identity.job_id.clone();
    let second_barrier = barrier.clone();
    let second = std::thread::spawn(move || {
        second_barrier.wait();
        append_background_prompt_with_images(
            &second_store,
            &second_job_id,
            "pp-concurrent-b".into(),
            "from mobile".into(),
            Vec::new(),
            false,
            &rebon_exe(),
        )
    });
    barrier.wait();
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();

    let queued = store.read_state(&state.identity.job_id).unwrap();
    assert_eq!(queued.identity.pending_prompts.len(), 2);
    assert_eq!(queued.process.status, BackgroundJobStatus::Running);
    assert_eq!(queued.process.turn_generation, 9);
    assert_eq!(queued.process.pid, state.process.pid);
    assert_eq!(queued.process.ipc_port, state.process.ipc_port);
    assert_eq!(queued.process.ipc_token, state.process.ipc_token);
    let ids = queued
        .identity
        .pending_prompts
        .iter()
        .map(|prompt| prompt.id.as_str())
        .collect::<HashSet<_>>();
    assert_eq!(ids, HashSet::from(["pp-concurrent-a", "pp-concurrent-b"]));
}

#[test]
fn pending_prompt_limit_rejects_without_mutating_state() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.identity.session_id = Some("sess-limit".into());
    state.process.status = BackgroundJobStatus::Running;
    state.identity.pending_prompts = (0..MAX_PENDING_PROMPTS)
        .map(|index| {
            pending_prompt(
                &format!("pp-limit-{index}"),
                &format!("prompt {index}"),
                Vec::new(),
            )
        })
        .collect();
    store.write_state(&state).unwrap();
    let expected = store.read_state(&state.identity.job_id).unwrap();

    let error = append_background_prompt_with_images(
        &store,
        &state.identity.job_id,
        "pp-over-limit".into(),
        "too many".into(),
        Vec::new(),
        false,
        &rebon_exe(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("maximum"));
    assert_eq!(store.read_state(&state.identity.job_id).unwrap(), expected);
}

#[test]
fn remove_job_deletes_terminal_job_metadata() {
    let (_dir, store) = store();
    let mut state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();
    state.process.status = BackgroundJobStatus::Succeeded;
    state.process.completed_at_ms = Some(now_ms());
    store.write_state(&state).unwrap();

    store.remove_job(&state.identity.job_id).unwrap();

    assert!(!store.job_dir(&state.identity.job_id).exists());
    assert!(store.read_state(&state.identity.job_id).is_err());
}

/// A finished job bound to `session_id`, run from `cwd`, with a scratchpad
/// that has something in it.
fn finished_session_job(
    store: &BackgroundStore,
    cwd: &Path,
    session_id: &str,
) -> (BackgroundJobState, PathBuf) {
    let mut state = store
        .create_job("prompt".into(), cwd.to_path_buf(), runtime())
        .unwrap();
    state.identity.session_id = Some(session_id.to_string());
    state.process.status = BackgroundJobStatus::Succeeded;
    state.process.completed_at_ms = Some(now_ms());
    store.write_state(&state).unwrap();
    let scratchpad = PathBuf::from(rebon_session::scratchpad_dir_for(
        &state.identity.cwd,
        session_id,
    ));
    fs::create_dir_all(&scratchpad).unwrap();
    fs::write(scratchpad.join("notes.txt"), "x").unwrap();
    (state, scratchpad)
}

#[test]
fn remove_job_deletes_the_scratchpad_of_the_session_it_ended() {
    let (_dir, store) = store();
    let project = tempfile::tempdir().unwrap();
    let (state, scratchpad) = finished_session_job(&store, project.path(), "sess-ended");

    store.remove_job(&state.identity.job_id).unwrap();

    assert!(!scratchpad.exists());
}

#[test]
fn remove_job_keeps_the_scratchpad_another_job_still_names() {
    let (_dir, store) = store();
    let project = tempfile::tempdir().unwrap();
    let (first, scratchpad) = finished_session_job(&store, project.path(), "sess-shared");
    let (second, _) = finished_session_job(&store, project.path(), "sess-shared");

    store.remove_job(&first.identity.job_id).unwrap();
    assert!(scratchpad.join("notes.txt").exists());

    store.remove_job(&second.identity.job_id).unwrap();
    assert!(
        !scratchpad.exists(),
        "the last job for the session takes it"
    );
}

#[test]
fn remove_job_keeps_the_scratchpad_of_a_session_someone_holds() {
    let (_dir, store) = store();
    let project = tempfile::tempdir().unwrap();
    let (state, scratchpad) = finished_session_job(&store, project.path(), "sess-held");
    // A terminal the session was attached to, still open.
    let lock = rebon_session::try_acquire_session_active_lock(
        &store.root().join("projects"),
        &state.identity.cwd,
        "sess-held",
    )
    .unwrap()
    .expect("lock is free");

    store.remove_job(&state.identity.job_id).unwrap();
    assert!(scratchpad.join("notes.txt").exists());

    drop(lock);
    rebon_session::remove_scratchpad_for(&state.identity.cwd, "sess-held");
}

#[test]
fn remove_job_rejects_active_and_invalid_ids() {
    let (_dir, store) = store();
    let state = store
        .create_job("prompt".into(), PathBuf::from("."), runtime())
        .unwrap();

    let active_err = store.remove_job(&state.identity.job_id).unwrap_err();
    assert!(active_err.to_string().contains("still active"));
    assert!(store.job_dir(&state.identity.job_id).exists());
    let invalid_err = store.remove_job("../outside").unwrap_err();
    assert!(invalid_err
        .to_string()
        .contains("invalid background job id"));
}
