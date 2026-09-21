//! The bottom `@Main / @agent` switcher's rows, and the one projection
//! that feeds them a task the runtime never spawned.
//!
//! Everything else that turns a `TaskSnapshot` into text lives in the
//! tasks plugin (`rebon_plugin_tasks::ui`), because the plugin owns both
//! the task and the surfaces that show it. These rows stay here for one
//! reason: the switcher lists an attached remote worker's background jobs
//! beside this session's own tasks, and
//! [`remote_background_task_snapshot`] is what makes a
//! `rebon_session_host::BackgroundTaskSnapshot` look like one. Blending the
//! two stores is the terminal's business, and pulling it into the plugin
//! would put `rebon-session-host` — an engine-and-tool-sized crate — behind
//! a feature switch that has no use for the rest of it.
//!
//! The Agent View has no `ui-registry` seat to be registered on: it is a
//! full-screen surface, not a dialog, and `ViewSpec` has no shape for one.
//! Until it does, its rows are built here.

use rebon_plugin_tasks::runtime::{TaskData, TaskSnapshot, TaskStatus as CoordStatus};

/// Live agent/task row shown in the bottom `@Main / @agent` switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSwitcherRowEntry {
    pub task_id: Option<String>,
    pub agent_name: String,
    pub agent_color: Option<String>,
    pub is_idle: bool,
    pub activity: String,
    pub metrics: Option<String>,
}

/// Build the live-agent rows used by the bottom session switcher.
/// Index 0 is the main session when at least one live agent exists. Live
/// LocalAgent and InProcessTeammate tasks follow.
pub fn build_agent_switcher_rows(
    snapshots: &[TaskSnapshot],
    is_leader_idle: bool,
) -> Vec<AgentSwitcherRowEntry> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    build_agent_switcher_rows_at(snapshots, is_leader_idle, now_ms)
}

fn build_agent_switcher_rows_at(
    snapshots: &[TaskSnapshot],
    is_leader_idle: bool,
    now_ms: u64,
) -> Vec<AgentSwitcherRowEntry> {
    let mut indexed_agents = snapshots
        .iter()
        .filter(|snap| !snap.status.is_terminal())
        .filter_map(|snap| match &snap.data {
            TaskData::LocalAgent(data)
                if !rebon_plugin_tasks::runtime::is_agent_snapshot_idle(snap) =>
            {
                Some((
                    snap.start_time_ms,
                    snap.id.as_str().to_owned(),
                    AgentSwitcherRowEntry {
                        task_id: Some(snap.id.as_str().to_owned()),
                        agent_name: snap
                            .metadata_str("display_name")
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .unwrap_or(&data.agent_type)
                            .to_string(),
                        agent_color: Some("blue".to_string()),
                        is_idle: false,
                        activity: agent_switcher_activity(snap),
                        metrics: agent_switcher_metrics(snap, data.token_count, now_ms),
                    },
                ))
            }
            TaskData::InProcessTeammate(data) if !data.is_idle => Some((
                snap.start_time_ms,
                snap.id.as_str().to_owned(),
                AgentSwitcherRowEntry {
                    task_id: Some(snap.id.as_str().to_owned()),
                    agent_name: data.identity.agent_name.clone(),
                    agent_color: data.identity.color.clone(),
                    is_idle: false,
                    activity: agent_switcher_activity(snap),
                    metrics: agent_switcher_metrics(snap, data.token_count, now_ms),
                },
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    indexed_agents.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let agents = indexed_agents
        .into_iter()
        .map(|(_, _, entry)| entry)
        .collect::<Vec<_>>();
    if agents.is_empty() {
        return Vec::new();
    }

    let mut entries = Vec::with_capacity(agents.len() + 1);
    entries.push(AgentSwitcherRowEntry {
        task_id: None,
        agent_name: "Main".to_string(),
        agent_color: None,
        is_idle: is_leader_idle,
        activity: if is_leader_idle { "idle" } else { "active" }.to_string(),
        metrics: None,
    });
    entries.extend(agents);
    entries
}

fn agent_switcher_metrics(
    snapshot: &TaskSnapshot,
    token_count: u64,
    now_ms: u64,
) -> Option<String> {
    let mut parts = Vec::with_capacity(2);
    if snapshot.start_time_ms > 0 {
        parts.push(
            rebon_plugin_agents::surface::coordinator_status::format_duration_short(
                now_ms.saturating_sub(snapshot.start_time_ms),
            ),
        );
    }
    if token_count > 0 {
        parts.push(format!(
            "↑ {} tokens",
            rebon_plugin_agents::surface::progress_line::format_number(token_count)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

pub fn is_external_acp_agent_snapshot(snapshot: &TaskSnapshot) -> bool {
    matches!(
        &snapshot.data,
        TaskData::LocalAgent(data)
            if data
                .model
                .as_deref()
                .is_some_and(|model| model.trim_start().starts_with("acp:"))
    )
}

/// Project a cross-process background task snapshot (from an attached remote
/// worker's persisted live events) into the coordinator's [`TaskSnapshot`]
/// shape, so every switcher/footer/view surface renders remote agents through
/// the same code path as in-process ones. Structured transcript events are
/// preserved when available; older projections fall back to the log preview.
pub fn remote_background_task_snapshot(
    snapshot: &rebon_session_host::BackgroundTaskSnapshot,
) -> TaskSnapshot {
    use rebon_plugin_tasks::runtime::{LocalAgentData, LocalAgentTranscriptEntry, TaskId};
    use rebon_session_host::BackgroundTaskTranscriptEntry;

    let task = &snapshot.task;
    // The worker seeds the descriptor prompt as the transcript's first
    // row for clients that render the transcript on its own. This view
    // renders `LocalAgentData::prompt` as a separate leading user row,
    // so the seeded copy is dropped here — keeping both would show the
    // prompt twice. Snapshots from workers that predate the seeding have
    // no leading row to drop and are unaffected.
    let seeded_prompt = task
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty());
    let entries = match (seeded_prompt, snapshot.transcript.first()) {
        (Some(prompt), Some(BackgroundTaskTranscriptEntry::User { text, .. }))
            if text == prompt =>
        {
            &snapshot.transcript[1..]
        }
        _ => snapshot.transcript.as_slice(),
    };
    let has_structured_transcript = !entries.is_empty();
    let transcript: Vec<LocalAgentTranscriptEntry> = if !has_structured_transcript {
        snapshot
            .log_preview
            .iter()
            .map(|line| LocalAgentTranscriptEntry::Assistant { text: line.clone() })
            .collect()
    } else {
        entries
            .iter()
            .map(|entry| match entry {
                BackgroundTaskTranscriptEntry::User { text, .. } => {
                    LocalAgentTranscriptEntry::User { text: text.clone() }
                }
                BackgroundTaskTranscriptEntry::Thinking { text, .. } => {
                    LocalAgentTranscriptEntry::Thinking { text: text.clone() }
                }
                BackgroundTaskTranscriptEntry::Assistant { text, .. } => {
                    LocalAgentTranscriptEntry::Assistant { text: text.clone() }
                }
                BackgroundTaskTranscriptEntry::ToolStart {
                    tool_use_id,
                    name,
                    input,
                    activity,
                    ..
                } => LocalAgentTranscriptEntry::ToolStart {
                    tool_use_id: tool_use_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    activity: activity.clone(),
                },
                BackgroundTaskTranscriptEntry::ToolProgress {
                    tool_use_id,
                    name,
                    message,
                    ..
                } => LocalAgentTranscriptEntry::ToolProgress {
                    tool_use_id: tool_use_id.clone(),
                    name: name.clone(),
                    message: message.clone(),
                },
                BackgroundTaskTranscriptEntry::ToolFinish {
                    tool_use_id,
                    name,
                    output,
                    error,
                    ..
                } => {
                    let outcome = error
                        .as_ref()
                        .map(|error| Err(error.clone()))
                        .unwrap_or_else(|| Ok(output.clone().unwrap_or(serde_json::Value::Null)));
                    LocalAgentTranscriptEntry::ToolFinish {
                        tool_use_id: tool_use_id.clone(),
                        name: name.clone(),
                        ok: outcome.is_ok(),
                        summary: error
                            .as_ref()
                            .map(|error| format!("{name} error: {error}"))
                            .unwrap_or_else(|| format!("{name} ok")),
                        outcome,
                    }
                }
            })
            .collect()
    };
    let streaming_text = if task.status == "running" && has_structured_transcript {
        transcript.iter().rev().find_map(|entry| match entry {
            LocalAgentTranscriptEntry::Assistant { text } => Some(text.clone()),
            _ => None,
        })
    } else {
        None
    };
    let data = TaskData::LocalAgent(LocalAgentData {
        prompt: task.prompt.clone().unwrap_or_else(|| task.title.clone()),
        agent_type: task
            .agent_type
            .clone()
            .unwrap_or_else(|| String::from("agent")),
        model: task.model.clone(),
        system: None,
        allowed_tools: None,
        token_count: task.token_count.unwrap_or(0),
        tool_use_count: task.tool_use_count.unwrap_or(0),
        transcript,
        streaming_text,
        pending_messages: Vec::new(),
        retrieved: false,
    });
    let mut projected =
        TaskSnapshot::new_pending(TaskId::new(&task.task_id), task.title.clone(), data);
    projected.status = remote_background_task_status(&task.status);
    projected.last_progress = task.last_progress.clone();
    projected.error = task.error.clone();
    projected.result = task.result.clone();
    projected.is_backgrounded = task.is_backgrounded;
    projected.start_time_ms = task.start_time_ms;
    projected.end_time_ms = task.end_time_ms;
    projected
}

fn remote_background_task_status(status: &str) -> CoordStatus {
    match status {
        "pending" => CoordStatus::Pending,
        "running" => CoordStatus::Running,
        "failed" => CoordStatus::Failed,
        "killed" | "cancelled" | "stopped" => CoordStatus::Killed,
        // "completed" and anything unknown stay terminal, so schema drift can
        // never leave a ghost "live" agent in the switcher.
        _ => CoordStatus::Completed,
    }
}

fn agent_switcher_activity(snapshot: &TaskSnapshot) -> String {
    let status = match snapshot.status {
        CoordStatus::Pending => "pending",
        CoordStatus::Running => "running",
        CoordStatus::Completed => "done",
        CoordStatus::Killed => "stopped",
        CoordStatus::Failed => "error",
    };
    if matches!(&snapshot.data, TaskData::LocalAgent(_)) {
        return if snapshot.status == CoordStatus::Running {
            if rebon_plugin_tasks::runtime::is_agent_snapshot_idle(snapshot) {
                "idle"
            } else {
                "working"
            }
        } else {
            status
        }
        .to_string();
    }

    let (status, base) = match &snapshot.data {
        TaskData::InProcessTeammate(data) => {
            let status = if snapshot.status == CoordStatus::Running {
                if data.is_idle {
                    "idle"
                } else {
                    "working"
                }
            } else {
                status
            };
            let base = snapshot
                .metadata_str("current_task")
                .map(str::trim)
                .filter(|task| !task.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if snapshot.title.starts_with('@') {
                        data.prompt.clone()
                    } else {
                        snapshot.title.clone()
                    }
                });
            (status, base)
        }
        _ => (status, snapshot.title.clone()),
    };
    if base.trim().is_empty() {
        status.to_string()
    } else {
        // Status leads: the row is right-truncated to the terminal
        // width, so a status appended after a long prompt would never
        // be visible.
        format!("{status} · {base}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_plugin_tasks::runtime::{TaskKind as CK, TaskStatus as CS};
    use rebon_plugin_tasks::test_support::task_snapshot as snap;

    #[test]
    fn build_agent_switcher_rows_includes_main_local_agents_and_teammates() {
        let mut snapshots = vec![
            snap("alice", CK::InProcessTeammate, CS::Running, "research"),
            snap("done", CK::LocalAgent, CS::Completed, "done"),
            snap("explorer", CK::LocalAgent, CS::Running, "explore"),
        ];
        snapshots[0].start_time_ms = 2;
        snapshots[2].start_time_ms = 1;
        let rows = build_agent_switcher_rows(&snapshots, true);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].agent_name, "Main");
        assert_eq!(rows[0].task_id, None);
        assert_eq!(rows[0].activity, "idle");
        assert_eq!(rows[1].task_id.as_deref(), Some("explorer"));
        assert_eq!(rows[1].activity, "working");
        assert_eq!(rows[2].task_id.as_deref(), Some("alice"));
    }

    #[test]
    fn teammate_switcher_uses_current_turn_instead_of_spawn_prompt() {
        let mut snapshot = snap(
            "renderer",
            CK::InProcessTeammate,
            CS::Running,
            "initial EPUB audit",
        );
        snapshot.title = "@renderer".into();
        snapshot.metadata = serde_json::json!({
            "current_task": "verify the renderer implementation"
        });

        let label = rebon_plugin_tasks::ui::tasks_view::snapshot_label(&snapshot);
        let rows = build_agent_switcher_rows_at(&[snapshot], true, 1_000);

        assert!(label.contains("verify the renderer implementation"));
        assert!(!label.contains("initial EPUB audit"));
        assert_eq!(
            rows[1].activity,
            "working · verify the renderer implementation"
        );
    }

    #[test]
    fn agent_switcher_local_agents_use_display_name_before_agent_type() {
        for (agent_type, display_name) in [
            ("verification", "verify-final-app-stability"),
            ("Explore", "explore-engine-query-flow"),
        ] {
            let mut snapshot = snap(
                display_name,
                CK::LocalAgent,
                CS::Running,
                "custom agent task",
            );
            snapshot.metadata = serde_json::json!({
                "display_name": display_name
            });
            let TaskData::LocalAgent(data) = &mut snapshot.data else {
                panic!("expected local agent data");
            };
            data.agent_type = agent_type.into();

            let rows = build_agent_switcher_rows(&[snapshot], true);

            assert_eq!(rows[1].agent_name, display_name);
            assert_eq!(rows[1].activity, "working");
        }
    }

    #[test]
    fn agent_switcher_metrics_update_elapsed_time_and_show_tokens() {
        let mut snapshot = snap("explorer", CK::LocalAgent, CS::Running, "explore");
        snapshot.start_time_ms = 1_000;
        let TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent data");
        };
        data.token_count = 30_600;

        let first = build_agent_switcher_rows_at(&[snapshot.clone()], true, 35_000);
        let next_second = build_agent_switcher_rows_at(&[snapshot], true, 36_000);

        assert_eq!(first[0].metrics, None);
        assert_eq!(first[1].metrics.as_deref(), Some("0:34 · ↑ 30,600 tokens"));
        assert_eq!(
            next_second[1].metrics.as_deref(),
            Some("0:35 · ↑ 30,600 tokens")
        );
    }

    #[test]
    fn agent_switcher_metrics_cover_pending_teammates_and_missing_values() {
        let mut pending = snap("researcher", CK::InProcessTeammate, CS::Pending, "research");
        pending.start_time_ms = 10_000;
        let TaskData::InProcessTeammate(data) = &mut pending.data else {
            panic!("expected teammate data");
        };
        data.token_count = 1_234;
        let missing = snap("explorer", CK::LocalAgent, CS::Running, "explore");

        let pending_rows = build_agent_switcher_rows_at(&[pending], true, 75_000);
        let missing_rows = build_agent_switcher_rows_at(&[missing], true, 75_000);

        assert_eq!(
            pending_rows[1].metrics.as_deref(),
            Some("1:05 · ↑ 1,234 tokens")
        );
        assert_eq!(missing_rows[1].metrics, None);
    }

    #[test]
    fn agent_switcher_local_agent_falls_back_to_agent_type_without_display_name() {
        let mut snapshot = snap("explorer", CK::LocalAgent, CS::Running, "explore");
        let TaskData::LocalAgent(data) = &mut snapshot.data else {
            panic!("expected local agent data");
        };
        data.agent_type = "Explore".into();

        let rows = build_agent_switcher_rows(&[snapshot], true);

        assert_eq!(rows[1].agent_name, "Explore");
    }

    #[test]
    fn agent_switcher_teammate_activity_leads_with_status() {
        let mut running = snap(
            "alice",
            CK::InProcessTeammate,
            CS::Running,
            "design a very long transactional semantics fix",
        );
        running.title = "@alice".into();

        let rows = build_agent_switcher_rows(&[running.clone()], true);
        assert_eq!(
            rows[1].activity,
            "working · design a very long transactional semantics fix"
        );

        if let TaskData::InProcessTeammate(data) = &mut running.data {
            data.is_idle = true;
        }
        assert!(build_agent_switcher_rows(&[running], true).is_empty());
    }

    #[test]
    fn agent_switcher_hides_idle_local_agents_and_keeps_active_agents() {
        let active = snap("active", CK::LocalAgent, CS::Running, "active work");
        let mut idle = snap("idle", CK::LocalAgent, CS::Running, "finished work");
        idle.metadata["_runtime_is_idle"] = serde_json::Value::Bool(true);

        let rows = build_agent_switcher_rows(&[idle, active], true);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].agent_name, "Main");
        assert_eq!(rows[1].task_id.as_deref(), Some("active"));
    }

    #[test]
    fn agent_switcher_hides_all_rows_when_only_main_is_active() {
        let mut local = snap("explorer", CK::LocalAgent, CS::Running, "done");
        local.metadata["_runtime_is_idle"] = serde_json::Value::Bool(true);
        let mut teammate = snap("alice", CK::InProcessTeammate, CS::Running, "done");
        if let TaskData::InProcessTeammate(data) = &mut teammate.data {
            data.is_idle = true;
        }

        assert!(build_agent_switcher_rows(&[local, teammate], false).is_empty());
    }

    #[test]
    fn agent_switcher_rows_ignore_streaming_progress_for_running_agent() {
        let mut snapshots = vec![snap("explorer", CK::LocalAgent, CS::Running, "explore")];
        snapshots[0].last_progress = Some("下面是按入口追踪的结果".into());

        let rows = build_agent_switcher_rows(&snapshots, true);

        assert_eq!(rows[1].activity, "working");
        assert!(!rows[1].activity.contains("入口"));
    }

    #[test]
    fn agent_switcher_rows_show_pending_for_queued_agent() {
        let snapshots = vec![snap("explorer", CK::LocalAgent, CS::Pending, "explore")];

        let rows = build_agent_switcher_rows(&snapshots, true);

        assert_eq!(rows[1].activity, "pending");
    }

    fn remote_task(task_id: &str, status: &str) -> rebon_session_host::BackgroundTaskSnapshot {
        rebon_session_host::BackgroundTaskSnapshot {
            task: rebon_session_host::BackgroundTaskDescriptor {
                task_id: task_id.into(),
                title: "Inspect remote code".into(),
                kind: "local_agent".into(),
                status: status.into(),
                is_backgrounded: true,
                start_time_ms: 7,
                end_time_ms: None,
                last_progress: Some("reading files".into()),
                error: None,
                prompt: None,
                parent_tool_call_id: Some("tool-remote".into()),
                agent_id: Some(task_id.into()),
                agent_name: Some("Explore".into()),
                agent_type: Some("Explore".into()),
                model: Some("test-model".into()),
                token_count: Some(42),
                tool_use_count: Some(3),
                result: None,
            },
            updated_at_ms: 8,
            log_preview: vec!["agent started".into(), "Grep src/".into()],
            transcript: Vec::new(),
        }
    }

    #[test]
    fn remote_background_task_projects_into_live_switcher_row() {
        let projected = remote_background_task_snapshot(&remote_task("agent-remote", "running"));

        assert_eq!(projected.id.as_str(), "agent-remote");
        assert_eq!(projected.status, CS::Running);
        assert_eq!(projected.kind, CK::LocalAgent);
        assert_eq!(projected.start_time_ms, 7);
        let TaskData::LocalAgent(data) = &projected.data else {
            panic!("expected local agent data");
        };
        assert_eq!(data.agent_type, "Explore");
        assert_eq!(data.prompt, "Inspect remote code");
        assert_eq!(data.token_count, 42);
        assert_eq!(data.tool_use_count, 3);
        assert_eq!(data.transcript.len(), 2);

        let rows = build_agent_switcher_rows(&[projected], true);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].task_id.as_deref(), Some("agent-remote"));
        assert_eq!(rows[1].agent_name, "Explore");
    }

    #[test]
    fn remote_background_task_preserves_in_flight_assistant_stream() {
        use rebon_session_host::BackgroundTaskTranscriptEntry;

        let mut snapshot = remote_task("agent-acp-live", "running");
        snapshot.task.model = Some("acp:claude:agent-default".into());
        snapshot.transcript = vec![BackgroundTaskTranscriptEntry::Assistant {
            text: "first live chunk".into(),
            timestamp_ms: 0,
        }];

        let projected = remote_background_task_snapshot(&snapshot);
        let TaskData::LocalAgent(data) = projected.data else {
            panic!("expected local agent data");
        };
        assert_eq!(data.streaming_text.as_deref(), Some("first live chunk"));

        snapshot.task.status = "completed".into();
        let completed = remote_background_task_snapshot(&snapshot);
        let TaskData::LocalAgent(data) = completed.data else {
            panic!("expected local agent data");
        };
        assert_eq!(data.streaming_text, None);
    }

    #[test]
    fn remote_background_task_preserves_structured_tool_results() {
        use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;
        use rebon_session_host::BackgroundTaskTranscriptEntry;

        let mut snapshot = remote_task("agent-remote", "running");
        snapshot.transcript = vec![
            BackgroundTaskTranscriptEntry::ToolStart {
                tool_use_id: "tool-grep".into(),
                name: "Grep".into(),
                input: serde_json::json!({ "pattern": "TaskSnapshot" }),
                activity: "Grep TaskSnapshot".into(),
                timestamp_ms: 0,
            },
            BackgroundTaskTranscriptEntry::ToolFinish {
                tool_use_id: "tool-grep".into(),
                name: "Grep".into(),
                output: Some(serde_json::json!({ "matches": ["src/lib.rs:1"] })),
                error: None,
                timestamp_ms: 0,
            },
            BackgroundTaskTranscriptEntry::ToolStart {
                tool_use_id: "tool-bash".into(),
                name: "Bash".into(),
                input: serde_json::json!({ "command": "false" }),
                activity: "Running command".into(),
                timestamp_ms: 0,
            },
            BackgroundTaskTranscriptEntry::ToolFinish {
                tool_use_id: "tool-bash".into(),
                name: "Bash".into(),
                output: None,
                error: Some("exit 1".into()),
                timestamp_ms: 0,
            },
        ];

        let projected = remote_background_task_snapshot(&snapshot);
        let TaskData::LocalAgent(data) = projected.data else {
            panic!("expected local agent data");
        };
        assert!(matches!(
            &data.transcript[1],
            LocalAgentTranscriptEntry::ToolFinish {
                tool_use_id,
                outcome: Ok(output),
                ..
            } if tool_use_id == "tool-grep" && output.get("matches").is_some()
        ));
        assert!(matches!(
            &data.transcript[3],
            LocalAgentTranscriptEntry::ToolFinish {
                tool_use_id,
                ok: false,
                outcome: Err(error),
                ..
            } if tool_use_id == "tool-bash" && error == "exit 1"
        ));
    }

    #[test]
    fn remote_background_task_replays_thinking_and_the_seeded_prompt_exactly_once() {
        use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;
        use rebon_session_host::BackgroundTaskTranscriptEntry;

        let mut snapshot = remote_task("agent-remote", "running");
        snapshot.task.prompt = Some("Inspect the repository".into());
        snapshot.transcript = vec![
            BackgroundTaskTranscriptEntry::User {
                text: "Inspect the repository".into(),
                timestamp_ms: 0,
            },
            BackgroundTaskTranscriptEntry::Thinking {
                text: "tracing the remote flow".into(),
                timestamp_ms: 0,
            },
            BackgroundTaskTranscriptEntry::Assistant {
                text: "on it".into(),
                timestamp_ms: 0,
            },
            BackgroundTaskTranscriptEntry::User {
                text: "focus on mobile".into(),
                timestamp_ms: 0,
            },
        ];

        let projected = remote_background_task_snapshot(&snapshot);
        let TaskData::LocalAgent(data) = projected.data else {
            panic!("expected local agent data");
        };

        // The view renders `prompt` as its own leading row, so the
        // seeded copy is gone while the genuine follow-up survives.
        assert_eq!(data.prompt, "Inspect the repository");
        assert!(matches!(
            data.transcript.as_slice(),
            [
                LocalAgentTranscriptEntry::Thinking { text: thinking },
                LocalAgentTranscriptEntry::Assistant { text: answer },
                LocalAgentTranscriptEntry::User { text: follow_up },
            ] if thinking == "tracing the remote flow"
                && answer == "on it"
                && follow_up == "focus on mobile"
        ));
    }

    #[test]
    fn remote_background_task_keeps_leading_user_rows_that_are_not_the_prompt() {
        use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;
        use rebon_session_host::BackgroundTaskTranscriptEntry;

        // Workers that predate prompt seeding send no `prompt`, so a
        // leading user row is real conversation and must be kept.
        let mut snapshot = remote_task("agent-remote", "running");
        snapshot.transcript = vec![BackgroundTaskTranscriptEntry::User {
            text: "focus on mobile".into(),
            timestamp_ms: 0,
        }];

        let projected = remote_background_task_snapshot(&snapshot);
        let TaskData::LocalAgent(data) = projected.data else {
            panic!("expected local agent data");
        };

        assert_eq!(data.prompt, "Inspect remote code");
        assert!(matches!(
            data.transcript.as_slice(),
            [LocalAgentTranscriptEntry::User { text }] if text == "focus on mobile"
        ));
    }

    #[test]
    fn remote_background_task_falls_back_to_log_preview_when_only_the_prompt_was_seeded() {
        use rebon_session_host::BackgroundTaskTranscriptEntry;

        let mut snapshot = remote_task("agent-remote", "running");
        snapshot.task.prompt = Some("Inspect the repository".into());
        snapshot.transcript = vec![BackgroundTaskTranscriptEntry::User {
            text: "Inspect the repository".into(),
            timestamp_ms: 0,
        }];

        let projected = remote_background_task_snapshot(&snapshot);
        let TaskData::LocalAgent(data) = projected.data else {
            panic!("expected local agent data");
        };

        assert_eq!(data.transcript.len(), 2, "log preview lines should show");
        assert_eq!(data.streaming_text, None);
    }

    #[test]
    fn remote_background_task_maps_terminal_and_unknown_statuses_out_of_switcher() {
        for status in ["completed", "killed", "stopped", "cancelled", "mystery"] {
            let projected = remote_background_task_snapshot(&remote_task("agent-done", status));
            assert!(
                projected.status.is_terminal(),
                "status {status} must project as terminal"
            );
            assert!(build_agent_switcher_rows(&[projected], true).is_empty());
        }
    }

    #[test]
    fn build_agent_switcher_rows_tie_breaks_by_task_id() {
        let snapshots = vec![
            snap("explorer-b", CK::LocalAgent, CS::Running, "explore"),
            snap("explorer-a", CK::LocalAgent, CS::Running, "explore"),
        ];
        let rows = build_agent_switcher_rows(&snapshots, true);
        assert_eq!(rows[1].task_id.as_deref(), Some("explorer-a"));
        assert_eq!(rows[2].task_id.as_deref(), Some("explorer-b"));
    }

    #[test]
    fn build_agent_switcher_rows_hides_main_without_live_agents() {
        let snapshots = vec![snap("shell", CK::LocalShell, CS::Running, "cmd")];
        assert!(build_agent_switcher_rows(&snapshots, true).is_empty());
    }
}
