//! What a `TaskSnapshot` looks like: the bridge from the runtime's own
//! record of a task to the display types the projectors next door take.
//!
//! [`crate::runtime`] knows how to run a task — the registry, the
//! snapshot, the `spawn_*_task` helpers. [`crate::ui::tasks`] knows how to
//! show one — `format_*_line`, `build_dialog_layout`, `build_pill_row`,
//! the detail reducers. This module is what turns the first into the
//! second. Both halves read one `TaskKind` and one `TaskStatus`, so
//! nothing is converted on the way; what is left is the per-kind
//! dispatch that picks the right projector and feeds it the typed
//! fields it needs.
//!
//! # What this module does
//!
//! 1. [`snapshot_label`] — build a one-line display label for a
//!    snapshot by running it through the appropriate
//!    `crate::ui::tasks::background_task::format_*_line` projector.
//! 2. [`build_dialog_input`] — roll the task snapshots up into the
//!    `TasksDialogInput` shape expected by
//!    [`crate::ui::tasks::tasks_dialog::build_dialog_layout`].
//! 3. [`count_running_by_bucket`] — counts used by the dialog subtitle
//!    builder (`build_subtitle`).
//! 4. [`build_pill_row_input`] — teammate-pill input for
//!    [`crate::ui::tasks::background_status::build_pill_row`], projected
//!    from live `InProcessTeammate` snapshots.
//!
//! Deliberately free of any terminal type: it produces `String`s and
//! plain structs, and whichever surface owns the pixels paints them.

use crate::runtime::{BashTaskKind, TaskData, TaskKind, TaskSnapshot, TaskStatus as CoordStatus};
use crate::ui::tasks::async_agent_detail::{
    build_async_agent_detail, AsyncAgentDetail, AsyncAgentDetailInput,
};
#[cfg(test)]
use crate::ui::tasks::background_status::TeammatePillInput;
use crate::ui::tasks::background_task::{
    default_activity_limit, format_dream_line, format_in_process_teammate_line,
    format_local_agent_line, format_local_bash_line, format_local_workflow_line,
    format_monitor_mcp_line, format_remote_agent_line, BackgroundTaskLine, DreamLineInput,
    LocalAgentLineInput, LocalBashLineInput, LocalWorkflowLineInput, MonitorMcpLineInput,
    RemoteAgentLineInput, TeammateLineInput,
};
#[cfg(test)]
use crate::ui::tasks::common::TaskStatus as RtStatus;
use crate::ui::tasks::remote_progress::RemoteSessionInput;
use crate::ui::tasks::shell_detail::{build_shell_detail, ShellDetail, ShellDetailInput};
use crate::ui::tasks::tasks_dialog::{DialogItemInput, TasksDialogInput};

/// Build the one-line display label for a [`TaskSnapshot`] by
/// dispatching to the appropriate [`crate::ui::tasks`] projector.
///
/// The result is the text that [`crate::ui::tasks::tasks_dialog`] displays
/// next to each item in the list view.
///
/// Dispatches off [`TaskData`] so every per-kind projector is fed
/// the typed fields it needs (command / prompt / agent identity /
/// etc.) without the caller reaching into the snapshot's extension
/// data manually.
pub fn snapshot_label(snapshot: &TaskSnapshot) -> String {
    let status = snapshot.status;
    let limit = default_activity_limit();
    let line = match &snapshot.data {
        TaskData::LocalShell(data) => format_local_bash_line(
            &LocalBashLineInput {
                is_monitor: data.display_kind == BashTaskKind::Monitor,
                description: snapshot.title.clone(),
                command: data.command.clone(),
                status,
            },
            limit,
        ),
        TaskData::LocalAgent(_) => format_local_agent_line(
            &LocalAgentLineInput {
                description: snapshot.title.clone(),
                status,
                notified: snapshot.notified,
            },
            limit,
        ),
        TaskData::RemoteAgent(data) => format_remote_agent_line(
            &RemoteAgentLineInput {
                title: data.title.clone(),
                session: RemoteSessionInput {
                    status,
                    is_remote_review: data.is_remote_review,
                    todo_completed: 0,
                    todo_total: 0,
                    review: None,
                },
            },
            limit,
        ),
        TaskData::InProcessTeammate(data) => format_in_process_teammate_line(
            &TeammateLineInput {
                agent_name: data.identity.agent_name.clone(),
                agent_color: data.identity.color.clone(),
                activity: snapshot
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
                    }),
            },
            limit,
        ),
        TaskData::LocalWorkflow(data) => format_local_workflow_line(
            &LocalWorkflowLineInput {
                display: data
                    .summary
                    .clone()
                    .or_else(|| snapshot.last_progress.clone())
                    .unwrap_or_else(|| data.workflow_name.clone()),
                status,
                agent_count: data.agent_count,
                notified: snapshot.notified,
            },
            limit,
        ),
        TaskData::Monitor(data) => format_local_bash_line(
            &LocalBashLineInput {
                is_monitor: true,
                description: data.description.clone(),
                command: data.redacted_target.clone(),
                status,
            },
            limit,
        ),
        TaskData::MonitorMcp(data) => format_monitor_mcp_line(
            &MonitorMcpLineInput {
                description: data.description.clone(),
                status,
                notified: snapshot.notified,
            },
            limit,
        ),
        TaskData::Dream(data) => format_dream_line(&DreamLineInput {
            description: snapshot.title.clone(),
            phase: match data.phase {
                crate::runtime::DreamPhase::Starting => "starting".into(),
                crate::runtime::DreamPhase::Updating => "updating".into(),
            },
            sessions_reviewing: data.sessions_reviewing,
            files_touched: data.files_touched.len() as u64,
            status,
            notified: snapshot.notified,
        }),
    };
    render_background_line(&line)
}

/// Render a [`BackgroundTaskLine`] to a single-line string without
/// design-system primitives — plain text spans + parens.
///
/// The coordinator runtime currently only produces `LocalBash` /
/// `LocalAgent` snapshots via [`snapshot_label`], so the
/// non-coordinator arms (`RemoteAgent*`, `InProcessTeammate`,
/// `LocalWorkflow`, `MonitorMcp`, `Dream`) are unreachable in
/// practice. We still handle them exhaustively so this function is
/// reusable the moment those task kinds grow a runtime.
fn render_background_line(line: &BackgroundTaskLine) -> String {
    match line {
        BackgroundTaskLine::LocalBash { display, progress }
        | BackgroundTaskLine::LocalAgent {
            description: display,
            progress,
        }
        | BackgroundTaskLine::MonitorMcp {
            description: display,
            progress,
        }
        | BackgroundTaskLine::LocalWorkflow { display, progress } => {
            format!("{display} ({}{})", progress.display_label, progress.suffix)
        }
        BackgroundTaskLine::RemoteAgent {
            title, progress, ..
        } => format!("{title} · {}", format_remote_progress(progress)),
        BackgroundTaskLine::RemoteAgentReview { progress } => format_remote_progress(progress),
        BackgroundTaskLine::InProcessTeammate {
            agent_label,
            activity,
            ..
        } => format!("{agent_label}: {activity}"),
        BackgroundTaskLine::Dream {
            description,
            phase,
            detail,
            progress,
        } => format!(
            "{description} · {phase} · {detail} ({}{})",
            progress.display_label, progress.suffix
        ),
    }
}

/// Project a [`crate::ui::tasks::remote_progress::RemoteProgressLine`] enum
/// to a plain-text one-liner in the rainbow-line style the remote session
/// progress row uses.
fn format_remote_progress(line: &crate::ui::tasks::remote_progress::RemoteProgressLine) -> String {
    use crate::ui::tasks::remote_progress::RemoteProgressLine as L;
    match line {
        L::ReviewReady => format!(
            "ultrareview ready · {} to view",
            rebon_design_system::format_shortcut_for_current_platform("shift+↓")
        ),
        L::ReviewFailed => "ultrareview · error".into(),
        L::ReviewRunning { tail } => format!("ultrareview · {tail}"),
        L::Done => "done".into(),
        L::Error => "error".into(),
        L::StatusEllipsis(status) => format!("{status}…"),
        L::TodoCounts { completed, total } => format!("{completed}/{total}"),
    }
}

/// Bucket of running counts used by
/// [`crate::ui::tasks::tasks_dialog::build_subtitle`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunningCounts {
    /// Running teammate / in-process-teammate tasks.
    pub running_teammates: u64,
    /// Running local-bash / local-shell tasks.
    pub running_bash: u64,
    /// Running remote / local-agent / workflow tasks.
    pub running_remote_or_agents: u64,
}

/// Count the running tasks in each of the three buckets the dialog
/// subtitle cares about.
pub fn count_running_by_bucket(snapshots: &[TaskSnapshot]) -> RunningCounts {
    let mut counts = RunningCounts::default();
    for snap in snapshots {
        if snap.status != CoordStatus::Running
            || !snap.is_backgrounded
            || crate::runtime::is_agent_snapshot_idle(snap)
        {
            continue;
        }
        match snap.kind {
            // "Active shells" bucket — local_bash + monitor_mcp are
            // grouped under shells.
            TaskKind::LocalShell | TaskKind::Monitor | TaskKind::MonitorMcp => {
                counts.running_bash += 1
            }
            // "Active agents" bucket — local_agent + remote_agent +
            // local_workflow.
            TaskKind::LocalAgent | TaskKind::RemoteAgent | TaskKind::LocalWorkflow => {
                counts.running_remote_or_agents += 1
            }
            // "Active teammates" bucket.
            TaskKind::InProcessTeammate => counts.running_teammates += 1,
            // Dream is never a "running" tally item — it's surfaced
            // separately via the "dreaming" pill. Ignore it.
            TaskKind::Dream => {}
        }
    }
    counts
}

/// Build the [`TasksDialogInput`] shape expected by
/// [`crate::ui::tasks::tasks_dialog::build_dialog_layout`].
///
/// * `snapshots` — the current snapshot of every task in the
///   [`crate::runtime::TaskRegistry`].
/// * `foregrounded_task_id` — the id of the currently-foregrounded
///   local_agent task (hidden from the dialog list so it only shows up
///   via the main transcript view). `None` when the user is not
///   viewing any task in the foreground.
///
/// Each snapshot becomes a [`DialogItemInput`] with:
/// * `id` — `TaskId.as_str()`
/// * `kind` — the runtime's own kind, which the widgets read directly
/// * `status` — the coordinator's own status, which the widgets read directly
/// * `start_time_ms` - copied from the coordinator snapshot so sorting
///   remains stable across dialog refreshes.
/// * `label` — produced by [`snapshot_label`]
pub fn build_dialog_input(
    snapshots: &[TaskSnapshot],
    foregrounded_task_id: Option<String>,
) -> TasksDialogInput {
    let tasks = snapshots
        .iter()
        .map(|snap| DialogItemInput {
            id: snap.id.as_str().to_owned(),
            kind: snap.kind,
            status: snap.status,
            start_time_ms: snap.start_time_ms,
            label: snapshot_label(snap),
        })
        .collect();
    TasksDialogInput {
        tasks,
        foregrounded_task_id,
        show_spinner_tree: false,
    }
}

/// Build the teammate pill-row input for
/// [`crate::ui::tasks::background_status::build_pill_row`].
///
/// Only live `InProcessTeammate` snapshots participate in the pill
/// row.
#[cfg(test)]
pub fn build_pill_row_input(snapshots: &[TaskSnapshot]) -> Vec<TeammatePillInput> {
    snapshots
        .iter()
        .filter_map(|snap| match &snap.data {
            TaskData::InProcessTeammate(data) if !snap.status.is_terminal() => {
                Some(TeammatePillInput {
                    task_id: snap.id.as_str().to_owned(),
                    agent_name: data.identity.agent_name.clone(),
                    agent_color: data.identity.color.clone(),
                    is_idle: data.is_idle,
                    kind: TaskKind::InProcessTeammate,
                })
            }
            _ => None,
        })
        .collect()
}

/// True when at least one live background shell should drive the footer pill.
pub fn has_background_tasks(snapshots: &[TaskSnapshot]) -> bool {
    snapshots.iter().any(|snap| {
        matches!(snap.kind, TaskKind::LocalShell | TaskKind::Monitor)
            && snap.is_backgrounded
            && !snap.status.is_terminal()
    })
}

pub fn background_tasks_footer_label(snapshots: &[TaskSnapshot]) -> Option<String> {
    let live_tasks = snapshots
        .iter()
        .filter(|snap| {
            matches!(snap.kind, TaskKind::LocalShell | TaskKind::Monitor)
                && snap.is_backgrounded
                && !snap.status.is_terminal()
        })
        .count();
    if live_tasks == 0 {
        return None;
    }

    let running_tasks = snapshots
        .iter()
        .filter(|snap| {
            matches!(snap.kind, TaskKind::LocalShell | TaskKind::Monitor)
                && snap.is_backgrounded
                && snap.status == CoordStatus::Running
        })
        .count();
    let includes_monitor = snapshots.iter().any(|snap| {
        snap.kind == TaskKind::Monitor && snap.is_backgrounded && !snap.status.is_terminal()
    });
    let noun = |count| {
        if includes_monitor {
            if count == 1 {
                "task"
            } else {
                "tasks"
            }
        } else if count == 1 {
            "shell"
        } else {
            "shells"
        }
    };
    if running_tasks > 0 {
        return Some(format!(
            "{running_tasks} background {} running",
            noun(running_tasks)
        ));
    }

    Some(format!("{live_tasks} background {}", noun(live_tasks)))
}

pub fn has_session_workflows(snapshots: &[TaskSnapshot]) -> bool {
    snapshots
        .iter()
        .any(|snap| snap.kind == TaskKind::LocalWorkflow)
}

pub fn workflows_footer_label(snapshots: &[TaskSnapshot]) -> Option<String> {
    let total = snapshots
        .iter()
        .filter(|snap| snap.kind == TaskKind::LocalWorkflow)
        .count();
    if total == 0 {
        return None;
    }

    let running = snapshots
        .iter()
        .filter(|snap| snap.kind == TaskKind::LocalWorkflow)
        .filter(|snap| snap.status == CoordStatus::Running)
        .count();
    let workflow_word = if total == 1 { "workflow" } else { "workflows" };
    if running > 0 {
        Some(format!("{running}/{total} session {workflow_word} running"))
    } else {
        Some(format!("{total} session {workflow_word}"))
    }
}

/// Format a duration in milliseconds as `mm:ss` or `h:mm:ss`, shared
/// by both detail dialogs and the footer pill row. Falls back to
/// `"--:--"` for zero-or-missing durations.
pub fn format_elapsed_ms(ms: u64) -> String {
    if ms == 0 {
        return "--:--".into();
    }
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// Extract stdout and the total byte count from a LocalShell
/// snapshot's `result` payload. The coordinator's shell spawn stores
/// the tool output as `{ "stdout": "...", "stderr": "...",
/// "exit_code": N }` (see
/// `crate::runtime::spawn_local_shell_task`), so this
/// function unpacks that shape for the shell detail dialog's
/// `extract_tail_lines` consumer.
///
/// Returns `(stdout, bytes_total)`. `bytes_total` is the length of
/// the raw stdout string — the coordinator doesn't truncate shell
/// output yet, so `content.len() == bytes_total` and the "Showing N
/// lines of M" summary will report `is_incomplete = false`.
pub fn extract_shell_stdout(result: &Option<serde_json::Value>) -> Option<(String, usize)> {
    let value = result.as_ref()?;
    let stdout = value.get("stdout")?.as_str()?.to_owned();
    let bytes_total = stdout.len();
    Some((stdout, bytes_total))
}

/// Extract the exit code from a LocalShell snapshot's `result`
/// payload. Returns `None` when
/// the result is absent
/// or does not contain an integer `exit_code` field.
pub fn extract_shell_exit_code(result: &Option<serde_json::Value>) -> Option<i32> {
    let value = result.as_ref()?;
    value
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .map(|n| n as i32)
}

/// Build a [`ShellDetailInput`] from a coordinator [`TaskSnapshot`].
/// Returns `None` when the snapshot is not a `LocalShell` task.
///
/// * `can_kill` — true when the dialog offers a kill hint (in practice
///   this is the `is_running && registry.cancel` check).
/// * `can_back` — true when the dialog is in "back to list" mode.
/// * `now_ms` — wall-clock milliseconds for the elapsed time
///   fallback when the task is still running.
pub fn build_shell_detail_input(
    snapshot: &TaskSnapshot,
    can_kill: bool,
    can_back: bool,
    now_ms: u64,
) -> Option<ShellDetailInput> {
    if !matches!(snapshot.kind, TaskKind::LocalShell) {
        return None;
    }
    Some(ShellDetailInput {
        is_monitor: false,
        command: snapshot.title.clone(),
        status: snapshot.status,
        start_time_ms: snapshot.start_time_ms,
        end_time_ms: snapshot.end_time_ms,
        now_ms,
        exit_code: extract_shell_exit_code(&snapshot.result),
        can_kill,
        can_back,
    })
}

/// Build the projected shell detail dialog from a coordinator
/// snapshot. Convenience wrapper around
/// [`build_shell_detail_input`] + [`build_shell_detail`].
pub fn project_shell_detail(snapshot: &TaskSnapshot, now_ms: u64) -> Option<ShellDetail> {
    let input = build_shell_detail_input(snapshot, false, true, now_ms)?;
    Some(build_shell_detail(&input))
}

/// Build an [`AsyncAgentDetailInput`] from a coordinator
/// [`TaskSnapshot`]. Returns `None` when the snapshot is not a
/// `LocalAgent` task.
///
/// * `can_kill` — true when the dialog offers a kill hint.
/// * `can_back` — true when the dialog is in "back to list" mode.
/// * `now_ms` — wall-clock milliseconds for the elapsed time
///   calculation.
pub fn build_async_agent_detail_input(
    snapshot: &TaskSnapshot,
    can_kill: bool,
    can_back: bool,
    now_ms: u64,
) -> Option<AsyncAgentDetailInput> {
    let agent_data = match &snapshot.data {
        TaskData::LocalAgent(data) => data,
        _ => return None,
    };
    let end = snapshot.end_time_ms.unwrap_or(now_ms);
    let elapsed_ms = end.saturating_sub(snapshot.start_time_ms);
    // Prefer explicit counters from the completed result payload, then
    // fall back to live counters mirrored into the LocalAgent snapshot.
    let token_count = snapshot
        .result
        .as_ref()
        .and_then(|r| {
            r.get("total_tokens").and_then(|v| v.as_u64()).or_else(|| {
                r.get("total_usage")
                    .or_else(|| r.get("usage"))
                    .and_then(|u| {
                        let input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        (input + output > 0).then_some(input + output)
                    })
            })
        })
        .or_else(|| (agent_data.token_count > 0).then_some(agent_data.token_count));
    let tool_use_count = snapshot
        .result
        .as_ref()
        .and_then(|r| {
            r.get("tool_call_count")
                .or_else(|| r.get("tool_use_count"))
                .and_then(|v| v.as_u64())
                .or_else(|| {
                    r.get("tool_calls")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len() as u64)
                })
        })
        .filter(|n| *n > 0)
        .or_else(|| (agent_data.tool_use_count > 0).then_some(agent_data.tool_use_count));
    Some(AsyncAgentDetailInput {
        agent_type: agent_data.agent_type.clone(),
        description: snapshot.title.clone(),
        status: snapshot.status,
        elapsed_time: format_elapsed_ms(elapsed_ms),
        token_count,
        tool_use_count,
        prompt: agent_data.prompt.clone(),
        error: snapshot.error.clone(),
        can_kill,
        can_back,
        // Live assistant text and tool activity are mirrored through
        // `last_progress`; the dialog uses it as the current activity.
        acp_last_assistant_text: snapshot.last_progress.clone().filter(|s| !s.is_empty()),
    })
}

/// Build the projected async-agent detail dialog from a coordinator
/// snapshot. Convenience wrapper.
pub fn project_async_agent_detail(
    snapshot: &TaskSnapshot,
    now_ms: u64,
) -> Option<AsyncAgentDetail> {
    let input = build_async_agent_detail_input(snapshot, false, true, now_ms)?;
    Some(build_async_agent_detail(&input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{TaskKind as CK, TaskStatus as CS};
    use crate::test_support::task_snapshot as snap;

    #[test]
    fn snapshot_label_local_shell_running_matches_the_shell_projector() {
        let s = snap("1", CK::LocalShell, CS::Running, "ls -la");
        let label = snapshot_label(&s);
        // LocalBash running goes through shell_progress → "running"
        assert!(label.contains("ls -la"));
        assert!(label.contains("running"));
    }

    #[test]
    fn snapshot_label_local_shell_done() {
        let s = snap("1", CK::LocalShell, CS::Completed, "cargo build");
        let label = snapshot_label(&s);
        assert!(label.contains("cargo build"));
        assert!(label.contains("done"));
    }

    #[test]
    fn snapshot_label_local_shell_error() {
        let s = snap("1", CK::LocalShell, CS::Failed, "bad cmd");
        let label = snapshot_label(&s);
        assert!(label.contains("bad cmd"));
        assert!(label.contains("error"));
    }

    #[test]
    fn snapshot_label_local_shell_killed_to_stopped() {
        let s = snap("1", CK::LocalShell, CS::Killed, "slow cmd");
        let label = snapshot_label(&s);
        assert!(label.contains("slow cmd"));
        assert!(label.contains("stopped"));
    }

    #[test]
    fn snapshot_label_local_agent_running() {
        let s = snap("1", CK::LocalAgent, CS::Running, "investigate bug");
        let label = snapshot_label(&s);
        assert!(label.contains("investigate bug"));
        assert!(label.contains("running"));
    }

    #[test]
    fn snapshot_label_local_agent_completed_notified_hides_unread_suffix() {
        let mut s = snap("1", CK::LocalAgent, CS::Completed, "investigate bug");
        s.notified = true;
        let label = snapshot_label(&s);
        assert!(label.contains("done"));
        assert!(!label.contains("unread"));
    }

    #[test]
    fn snapshot_label_local_agent_completed_unnotified_shows_unread_suffix() {
        // Default snapshot has `notified: false`; a freshly completed
        // agent task should carry the "(done, unread)" suffix until
        // the consumer acknowledges it.
        let s = snap("1", CK::LocalAgent, CS::Completed, "investigate bug");
        let label = snapshot_label(&s);
        assert!(label.contains("done"));
        assert!(label.contains("unread"));
    }

    #[test]
    fn count_running_by_bucket_empty() {
        let counts = count_running_by_bucket(&[]);
        assert_eq!(counts, RunningCounts::default());
    }

    #[test]
    fn count_running_by_bucket_mixed_kinds_and_statuses() {
        let mut snapshots = vec![
            snap("a", CK::LocalShell, CS::Running, "cmd-a"),
            snap("b", CK::LocalShell, CS::Completed, "cmd-b"),
            snap("c", CK::LocalAgent, CS::Running, "task-c"),
            snap("d", CK::LocalAgent, CS::Running, "task-d"),
            snap("e", CK::LocalAgent, CS::Failed, "task-e"),
            snap("f", CK::LocalAgent, CS::Running, "foreground-explorer"),
        ];
        snapshots[0].is_backgrounded = true;
        snapshots[2].is_backgrounded = true;
        snapshots[3].is_backgrounded = true;
        let counts = count_running_by_bucket(&snapshots);
        assert_eq!(counts.running_bash, 1);
        assert_eq!(counts.running_remote_or_agents, 2);
        assert_eq!(counts.running_teammates, 0);
    }

    #[test]
    fn build_dialog_input_maps_every_snapshot() {
        let snapshots = vec![
            snap("a", CK::LocalShell, CS::Running, "cmd-a"),
            snap("b", CK::LocalAgent, CS::Completed, "task-b"),
        ];
        let input = build_dialog_input(&snapshots, None);
        assert_eq!(input.tasks.len(), 2);
        assert_eq!(input.tasks[0].id, "a");
        assert_eq!(input.tasks[0].kind, CK::LocalShell);
        assert_eq!(input.tasks[0].status, RtStatus::Running);
        assert_eq!(input.tasks[1].id, "b");
        assert_eq!(input.tasks[1].kind, CK::LocalAgent);
        assert_eq!(input.tasks[1].status, RtStatus::Completed);
        assert!(input.foregrounded_task_id.is_none());
        assert!(!input.show_spinner_tree);
    }

    #[test]
    fn running_counts_exclude_idle_teammates() {
        let mut active = snap("alice", CK::InProcessTeammate, CS::Running, "research");
        let mut idle = snap("bob", CK::InProcessTeammate, CS::Running, "waiting");
        active.is_backgrounded = true;
        idle.is_backgrounded = true;
        if let TaskData::InProcessTeammate(data) = &mut idle.data {
            data.is_idle = true;
        }
        let counts = count_running_by_bucket(&[active.clone(), idle]);
        assert_eq!(counts.running_teammates, 1);
        if let TaskData::InProcessTeammate(data) = &mut active.data {
            data.is_idle = true;
        }
        assert_eq!(count_running_by_bucket(&[active]).running_teammates, 0);
    }

    #[test]
    fn build_dialog_input_forwards_foregrounded_task_id() {
        let snapshots = vec![snap("fg", CK::LocalAgent, CS::Running, "foreground-task")];
        let input = build_dialog_input(&snapshots, Some("fg".into()));
        assert_eq!(input.foregrounded_task_id.as_deref(), Some("fg"));
    }

    #[test]
    fn build_pill_row_input_collects_live_teammates() {
        let snapshots = vec![
            snap("alice", CK::InProcessTeammate, CS::Running, "research"),
            snap("bob", CK::LocalShell, CS::Running, "cmd"),
            snap("carol", CK::InProcessTeammate, CS::Killed, "done"),
        ];
        let input = build_pill_row_input(&snapshots);
        assert_eq!(input.len(), 1);
        assert_eq!(input[0].agent_name, "alice");
        assert_eq!(input[0].kind, CK::InProcessTeammate);
    }

    #[test]
    fn has_background_tasks_reflects_live_background_shells() {
        assert!(!has_background_tasks(&[]));
        for kind in [
            CK::LocalAgent,
            CK::RemoteAgent,
            CK::InProcessTeammate,
            CK::LocalWorkflow,
        ] {
            let mut snapshot = snap("not-shell", kind, CS::Running, "work");
            snapshot.is_backgrounded = true;
            assert!(!has_background_tasks(&[snapshot]), "kind {kind:?}");
        }
        let mut shell = snap("a", CK::LocalShell, CS::Running, "cmd");
        shell.is_backgrounded = true;
        assert!(has_background_tasks(&[shell]));
        let mut shell = snap("a", CK::LocalShell, CS::Completed, "cmd");
        shell.is_backgrounded = true;
        assert!(!has_background_tasks(&[shell]));
        let mut monitor = snap("monitor", CK::Monitor, CS::Running, "events");
        monitor.is_backgrounded = true;
        assert!(has_background_tasks(&[monitor]));
    }

    #[test]
    fn background_tasks_footer_label_counts_running_shells_only() {
        assert_eq!(background_tasks_footer_label(&[]), None);
        let mut shell = snap("a", CK::LocalShell, CS::Running, "cmd");
        shell.is_backgrounded = true;
        let snapshots = vec![shell];
        assert_eq!(
            background_tasks_footer_label(&snapshots).as_deref(),
            Some("1 background shell running")
        );

        let mut shell = snap("a", CK::LocalShell, CS::Running, "cmd-a");
        shell.is_backgrounded = true;
        let mut agent = snap("b", CK::LocalAgent, CS::Running, "agent-b");
        agent.is_backgrounded = true;
        let mut teammate = snap("c", CK::InProcessTeammate, CS::Running, "teammate-c");
        teammate.is_backgrounded = true;
        let snapshots = vec![shell, agent, teammate];
        assert_eq!(
            background_tasks_footer_label(&snapshots).as_deref(),
            Some("1 background shell running")
        );
    }

    #[test]
    fn background_tasks_footer_label_includes_monitors() {
        let mut monitor = snap("monitor", CK::Monitor, CS::Running, "events");
        monitor.is_backgrounded = true;
        assert_eq!(
            background_tasks_footer_label(&[monitor]).as_deref(),
            Some("1 background task running")
        );

        let mut shell = snap("shell", CK::LocalShell, CS::Running, "cmd");
        shell.is_backgrounded = true;
        let mut monitor = snap("monitor", CK::Monitor, CS::Running, "events");
        monitor.is_backgrounded = true;
        assert_eq!(
            background_tasks_footer_label(&[shell, monitor]).as_deref(),
            Some("2 background tasks running")
        );
    }

    #[test]
    fn idle_teammate_stays_in_teammate_pills_without_background_shell_label() {
        let mut teammate = snap("alice", CK::InProcessTeammate, CS::Running, "waiting");
        teammate.is_backgrounded = true;
        let TaskData::InProcessTeammate(data) = &mut teammate.data else {
            panic!("expected teammate data");
        };
        data.is_idle = true;

        assert_eq!(background_tasks_footer_label(&[teammate.clone()]), None);
        let pills = build_pill_row_input(&[teammate]);
        assert_eq!(pills.len(), 1);
        assert_eq!(pills[0].agent_name, "alice");
        assert!(pills[0].is_idle);
    }

    #[test]
    fn background_tasks_footer_label_excludes_foreground_agents() {
        let foreground = snap("explorer", CK::LocalAgent, CS::Running, "explore");
        assert_eq!(background_tasks_footer_label(&[foreground]), None);
    }

    #[test]
    fn background_tasks_footer_label_shows_pending_shells() {
        let mut pending = snap("a", CK::LocalShell, CS::Pending, "cmd-a");
        pending.is_backgrounded = true;
        let snapshots = vec![pending, snap("b", CK::LocalAgent, CS::Completed, "cmd-b")];
        assert_eq!(
            background_tasks_footer_label(&snapshots).as_deref(),
            Some("1 background shell")
        );
    }

    #[test]
    fn background_tasks_footer_label_hides_terminal_tasks() {
        let snapshots = vec![
            snap("a", CK::LocalShell, CS::Completed, "cmd-a"),
            snap("b", CK::LocalAgent, CS::Failed, "cmd-b"),
            snap("c", CK::LocalAgent, CS::Killed, "cmd-c"),
        ];
        assert_eq!(background_tasks_footer_label(&snapshots), None);
    }

    #[test]
    fn workflows_footer_label_counts_current_session_workflows() {
        assert_eq!(workflows_footer_label(&[]), None);
        let snapshots = vec![
            snap("a", CK::LocalWorkflow, CS::Completed, "workflow-a"),
            snap("b", CK::LocalWorkflow, CS::Running, "workflow-b"),
            snap("c", CK::LocalAgent, CS::Running, "agent"),
        ];
        assert!(has_session_workflows(&snapshots));
        assert_eq!(
            workflows_footer_label(&snapshots).as_deref(),
            Some("1/2 session workflows running")
        );
    }

    #[test]
    fn workflows_footer_label_shows_completed_session_workflows() {
        let snapshots = vec![snap("a", CK::LocalWorkflow, CS::Completed, "workflow-a")];
        assert!(has_session_workflows(&snapshots));
        assert_eq!(
            workflows_footer_label(&snapshots).as_deref(),
            Some("1 session workflow")
        );
    }

    #[test]
    fn render_background_line_local_bash_format() {
        let line = BackgroundTaskLine::LocalBash {
            display: "ls -la".into(),
            progress: crate::ui::tasks::shell_progress::shell_progress(RtStatus::Running),
        };
        let text = render_background_line(&line);
        assert_eq!(text, "ls -la (running)");
    }

    #[test]
    fn format_elapsed_zero_is_placeholder() {
        assert_eq!(format_elapsed_ms(0), "--:--");
    }

    #[test]
    fn format_elapsed_sub_minute() {
        assert_eq!(format_elapsed_ms(5_000), "00:05");
        assert_eq!(format_elapsed_ms(59_000), "00:59");
    }

    #[test]
    fn format_elapsed_minutes() {
        assert_eq!(format_elapsed_ms(60_000), "01:00");
        assert_eq!(format_elapsed_ms(10 * 60_000 + 5_000), "10:05");
    }

    #[test]
    fn format_elapsed_hours() {
        assert_eq!(format_elapsed_ms(3_600_000), "1:00:00");
        assert_eq!(format_elapsed_ms(3_665_000), "1:01:05");
    }

    #[test]
    fn extract_shell_stdout_reads_value_and_byte_count() {
        let result = Some(serde_json::json!({
            "stdout": "line1\nline2\n",
            "stderr": "",
            "exit_code": 0,
        }));
        let (stdout, bytes) = extract_shell_stdout(&result).unwrap();
        assert_eq!(stdout, "line1\nline2\n");
        assert_eq!(bytes, 12);
    }

    #[test]
    fn extract_shell_stdout_returns_none_for_empty_result() {
        assert!(extract_shell_stdout(&None).is_none());
        assert!(extract_shell_stdout(&Some(serde_json::json!({}))).is_none());
    }

    #[test]
    fn extract_shell_exit_code_picks_int() {
        let result = Some(serde_json::json!({ "exit_code": 0 }));
        assert_eq!(extract_shell_exit_code(&result), Some(0));
        let result = Some(serde_json::json!({ "exit_code": 42 }));
        assert_eq!(extract_shell_exit_code(&result), Some(42));
        let result = Some(serde_json::json!({}));
        assert_eq!(extract_shell_exit_code(&result), None);
    }

    #[test]
    fn build_shell_detail_input_returns_none_for_non_shell() {
        let s = snap("1", CK::LocalAgent, CS::Running, "agent-task");
        assert!(build_shell_detail_input(&s, true, false, 1000).is_none());
    }

    #[test]
    fn build_shell_detail_input_maps_snapshot_to_shell_input() {
        let mut s = snap("1", CK::LocalShell, CS::Running, "ls -la");
        s.start_time_ms = 1_000;
        let input = build_shell_detail_input(&s, true, true, 5_000).unwrap();
        assert!(!input.is_monitor);
        assert_eq!(input.command, "ls -la");
        assert_eq!(input.status, RtStatus::Running);
        assert_eq!(input.start_time_ms, 1_000);
        assert_eq!(input.now_ms, 5_000);
        assert!(input.can_kill);
        assert!(input.can_back);
    }

    #[test]
    fn project_shell_detail_runtime_ms_reflects_end_time() {
        let mut s = snap("1", CK::LocalShell, CS::Completed, "cargo build");
        s.start_time_ms = 1_000;
        s.end_time_ms = Some(4_000);
        s.result = Some(serde_json::json!({ "stdout": "ok\n", "exit_code": 0 }));
        let detail = project_shell_detail(&s, 999_999).unwrap();
        assert_eq!(detail.runtime_ms, 3_000);
        assert_eq!(detail.command_label, "Command:");
        assert_eq!(detail.status_row.status, "completed");
        assert_eq!(
            detail.status_row.exit_code_suffix.as_deref(),
            Some(" (exit code: 0)")
        );
        assert!(!detail.byline.show_stop); // completed — can't kill
        assert!(detail.byline.show_back);
    }

    #[test]
    fn build_async_agent_detail_input_returns_none_for_non_agent() {
        let s = snap("1", CK::LocalShell, CS::Running, "cmd");
        assert!(build_async_agent_detail_input(&s, true, false, 1000).is_none());
    }

    #[test]
    fn build_async_agent_detail_input_computes_elapsed_and_title() {
        let mut s = snap("1", CK::LocalAgent, CS::Running, "investigate auth bug");
        s.start_time_ms = 10_000;
        let input = build_async_agent_detail_input(&s, true, false, 40_000).unwrap();
        assert_eq!(input.description, "investigate auth bug");
        assert_eq!(input.status, RtStatus::Running);
        assert_eq!(input.elapsed_time, "00:30");
        assert!(input.can_kill);
        assert!(!input.can_back);
        assert!(input.token_count.is_none());
        assert!(input.tool_use_count.is_none());
    }

    #[test]
    fn build_async_agent_detail_input_extracts_token_and_tool_counts() {
        let mut s = snap("1", CK::LocalAgent, CS::Completed, "review diffs");
        s.start_time_ms = 0;
        s.end_time_ms = Some(5_000);
        s.result = Some(serde_json::json!({
            "final_text": "done",
            "total_usage": {
                "input_tokens": 100,
                "output_tokens": 50,
            },
            "tool_calls": [
                { "name": "Read", "ok": true },
                { "name": "Grep", "ok": true },
                { "name": "Edit", "ok": true },
            ],
        }));
        let input = build_async_agent_detail_input(&s, false, true, 999_999).unwrap();
        assert_eq!(input.token_count, Some(150));
        assert_eq!(input.tool_use_count, Some(3));
        assert_eq!(input.elapsed_time, "00:05");
    }

    #[test]
    fn project_async_agent_detail_completed_no_acp_tail() {
        let mut s = snap("1", CK::LocalAgent, CS::Completed, "task");
        s.start_time_ms = 0;
        s.end_time_ms = Some(1_000);
        s.last_progress = Some("streaming text".into());
        let detail = project_async_agent_detail(&s, 999_999).unwrap();
        // Running-only ACP output gate: completed → None
        assert!(detail.acp_output.is_none());
    }

    #[test]
    fn render_background_line_completed_with_suffix() {
        let line = BackgroundTaskLine::LocalAgent {
            description: "subagent".into(),
            progress: crate::ui::tasks::shell_progress::task_status_text(
                RtStatus::Completed,
                Some("done"),
                Some(", unread"),
            ),
        };
        let text = render_background_line(&line);
        assert_eq!(text, "subagent (done, unread)");
    }
}
