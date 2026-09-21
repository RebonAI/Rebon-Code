use std::time::{Duration, SystemTime};

pub(super) use rebon_types::wall_clock_ms;

use crate::session::commands::fmt_tokens;

const NEW_SESSION_HINT_TOKENS_THRESHOLD: u32 = 8_000;
const NEW_SESSION_HINT_IDLE_DELAY: Duration = Duration::from_secs(60 * 60);
const STALE_SESSION_WARNING_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AgentActivityInfo {
    pub(super) elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct GoalActivityInfo {
    pub(super) status: crate::goal::GoalStatus,
    pub(super) elapsed_ms: u64,
}

/// Status bar info passed to the render layer each frame.
pub(in crate::tui) struct StatusBarInfo<'a> {
    pub(super) provider: &'a str,
    pub(super) model: &'a str,
    pub(super) cwd: &'a str,
    /// Milliseconds elapsed since the current turn was spawned.
    /// 0 when idle.
    pub(super) elapsed_ms: u64,
    /// Current effort/thinking level display string (e.g. `"effort xhigh"`).
    /// Empty when effort is auto / not set.
    pub(super) effort_display: String,
    /// Current OpenAI OAuth fast-mode display marker. Empty when fast
    /// mode is off or unavailable for the current provider.
    pub(super) fast_mode_display: String,
    /// Percentage of context left until auto-compact triggers.
    /// `None` when no usage has been reported yet (budget unknown).
    pub(super) context_left_pct: Option<u32>,
    /// Optional right-aligned timer for live coordinator agent tasks.
    pub(super) agent_activity: Option<AgentActivityInfo>,
    /// Optional right-aligned timer for a visible goal run.
    pub(super) goal_activity: Option<GoalActivityInfo>,
    /// Optional left-side hint rendered immediately after the cwd.
    pub(super) footer_action_hint: Option<&'a str>,
    /// Optional right-aligned hint suggesting a fresh session.
    /// Only populated when the session is idle (no active prompt, no
    /// active coordinator-backed agents, no foreground coordinator tasks,
    /// no in-progress tool tasks) so a mid-flight `/new` does not discard
    /// work the user has not seen yet.
    pub(super) new_session_hint: Option<String>,
}

fn is_idle_agent(snapshot: &rebon_plugin_tasks::runtime::TaskSnapshot) -> bool {
    rebon_plugin_tasks::runtime::is_agent_snapshot_idle(snapshot)
}

pub(super) fn active_agent_footer_status(
    snapshots: &[rebon_plugin_tasks::runtime::TaskSnapshot],
    now_ms: u64,
    foregrounded_task_id: Option<&str>,
) -> Option<AgentActivityInfo> {
    use rebon_plugin_tasks::runtime::{TaskKind, TaskStatus};

    let mut earliest_start_ms: Option<u64> = None;

    for snapshot in snapshots {
        if !matches!(snapshot.status, TaskStatus::Pending | TaskStatus::Running)
            || is_idle_agent(snapshot)
        {
            continue;
        }
        if !matches!(
            snapshot.kind,
            TaskKind::LocalAgent
                | TaskKind::RemoteAgent
                | TaskKind::InProcessTeammate
                | TaskKind::LocalWorkflow
                | TaskKind::Dream
        ) {
            continue;
        }

        if snapshot.kind == TaskKind::LocalAgent
            && snapshot.is_backgrounded
            && foregrounded_task_id != Some(snapshot.id.as_str())
        {
            continue;
        }

        earliest_start_ms = Some(match earliest_start_ms {
            Some(current) => current.min(snapshot.start_time_ms),
            None => snapshot.start_time_ms,
        });
    }

    earliest_start_ms.map(|start_time_ms| AgentActivityInfo {
        elapsed_ms: now_ms.saturating_sub(start_time_ms),
    })
}

pub(super) fn has_active_background_agent(
    snapshots: &[rebon_plugin_tasks::runtime::TaskSnapshot],
) -> bool {
    use rebon_plugin_tasks::runtime::{TaskKind, TaskStatus};

    snapshots.iter().any(|snapshot| {
        matches!(snapshot.status, TaskStatus::Pending | TaskStatus::Running)
            && !is_idle_agent(snapshot)
            && snapshot.is_backgrounded
            && matches!(
                snapshot.kind,
                TaskKind::LocalAgent
                    | TaskKind::RemoteAgent
                    | TaskKind::InProcessTeammate
                    | TaskKind::LocalWorkflow
                    | TaskKind::Dream
            )
    })
}

pub(super) fn has_active_background_shell(
    snapshots: &[rebon_plugin_tasks::runtime::TaskSnapshot],
) -> bool {
    use rebon_plugin_tasks::runtime::{TaskKind, TaskStatus};

    snapshots.iter().any(|snapshot| {
        matches!(snapshot.status, TaskStatus::Pending | TaskStatus::Running)
            && snapshot.is_backgrounded
            && snapshot.kind == TaskKind::LocalShell
    })
}

pub(super) fn footer_new_session_hint(
    tokens: u32,
    is_loading: bool,
    has_active_agent: bool,
    has_active_background_agent: bool,
    has_foreground_coordinator_task: bool,
    has_in_progress_tool_task: bool,
    idle_for: Duration,
) -> Option<String> {
    if is_loading
        || has_active_agent
        || has_active_background_agent
        || has_foreground_coordinator_task
        || has_in_progress_tool_task
        || idle_for < NEW_SESSION_HINT_IDLE_DELAY
    {
        return None;
    }

    format_new_session_hint(tokens)
}

fn format_new_session_hint(tokens: u32) -> Option<String> {
    (tokens >= NEW_SESSION_HINT_TOKENS_THRESHOLD)
        .then(|| format!("/new to save ~{} tokens", fmt_tokens(tokens)))
}

pub(crate) fn stale_resume_warning(
    created_at: SystemTime,
    replayed_tokens: Option<u32>,
) -> Option<String> {
    let age = SystemTime::now().duration_since(created_at).ok()?;
    if age < STALE_SESSION_WARNING_AGE {
        return None;
    }

    let mut warning =
        String::from("This session is stale. Use /new to avoid resending old context");
    if let Some(tokens) =
        replayed_tokens.filter(|tokens| *tokens >= NEW_SESSION_HINT_TOKENS_THRESHOLD)
    {
        warning.push_str(&format!(" (~{} tokens)", fmt_tokens(tokens)));
    }
    warning.push('.');
    Some(warning)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tui::app::AppState;

    use crate::session::commands::effort::{execute_effort_command, EffortCommand};

    /// Build the effort_display string the same way the render loop does.
    fn build_effort_display(app: &AppState) -> String {
        app.effort_level
            .map(|level| {
                let label = app.effort_provider_kind.label();
                format!("{} {}", label, level.as_str())
            })
            .unwrap_or_default()
    }

    #[test]
    fn status_bar_empty_when_effort_is_auto() {
        let app = AppState::default();
        assert!(build_effort_display(&app).is_empty());
    }

    #[test]
    fn status_bar_updates_after_effort_set() {
        use rebon_types::ReasoningEffort;

        let mut app = AppState::default();
        app.effort_provider_kind = rebon_types::effort_indicator::EffortProviderKind::OpenAi;
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::XHigh),
        );
        let display = build_effort_display(&app);
        assert_eq!(display, "thinking xhigh");
    }

    #[test]
    fn status_bar_updates_on_every_switch() {
        use rebon_types::ReasoningEffort;

        let mut app = AppState::default();

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::Low),
        );
        assert!(build_effort_display(&app).contains("low"));

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::High),
        );
        let display = build_effort_display(&app);
        assert!(display.contains("high"));
        assert!(!display.contains("low"), "stale 'low' in: {display}");

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::XHigh),
        );
        let display = build_effort_display(&app);
        assert_eq!(display.split_whitespace().last(), Some("xhigh"));
    }

    #[test]
    fn status_bar_clears_after_auto() {
        use rebon_types::ReasoningEffort;

        let mut app = AppState::default();

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::XHigh),
        );
        assert!(!build_effort_display(&app).is_empty());

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Auto,
        );
        assert!(build_effort_display(&app).is_empty());
    }

    #[test]
    fn status_bar_reappears_after_auto_then_set() {
        use rebon_types::ReasoningEffort;

        let mut app = AppState::default();

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::High),
        );
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Auto,
        );
        assert!(build_effort_display(&app).is_empty());

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::Medium),
        );
        let display = build_effort_display(&app);
        assert!(display.contains("medium"));
    }

    #[test]
    fn status_bar_shows_provider_label_anthropic() {
        use rebon_types::ReasoningEffort;

        let mut app = AppState::default();
        app.effort_provider_kind = rebon_types::effort_indicator::EffortProviderKind::Anthropic;
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::High),
        );
        let display = build_effort_display(&app);
        assert!(
            display.contains("effort"),
            "expected 'effort' in: {display}"
        );
    }

    #[test]
    fn status_bar_shows_provider_label_openai() {
        use rebon_types::ReasoningEffort;

        let mut app = AppState::default();
        app.effort_provider_kind = rebon_types::effort_indicator::EffortProviderKind::OpenAi;
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::High),
        );
        let display = build_effort_display(&app);
        assert!(
            display.contains("thinking"),
            "expected 'thinking' in: {display}"
        );
    }

    #[test]
    fn active_agent_footer_status_uses_earliest_active_agent_start() {
        use rebon_plugin_tasks::runtime::{
            BashTaskKind, LocalAgentData, LocalShellData, TaskData, TaskId, TaskKind, TaskSnapshot,
            TaskStatus,
        };

        fn agent(id: &str, status: TaskStatus, start_time_ms: u64) -> TaskSnapshot {
            TaskSnapshot {
                id: TaskId::new(id),
                kind: TaskKind::LocalAgent,
                status,
                title: "agent".into(),
                last_progress: None,
                error: None,
                result: None,
                is_backgrounded: false,
                notified: false,
                start_time_ms,
                end_time_ms: None,
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
            }
        }

        let mut shell = TaskSnapshot::new_pending(
            TaskId::new("shell-1"),
            "shell".into(),
            TaskData::LocalShell(LocalShellData {
                command: "cargo test".into(),
                exit_code: None,
                interrupted: false,
                display_kind: BashTaskKind::Bash,
                agent_id: None,
            }),
        );
        shell.status = TaskStatus::Running;
        shell.start_time_ms = 1_000;

        let snapshots = vec![
            shell,
            agent("agent-1", TaskStatus::Running, 3_000),
            agent("agent-2", TaskStatus::Pending, 2_000),
            agent("agent-3", TaskStatus::Completed, 500),
        ];

        assert_eq!(
            active_agent_footer_status(&snapshots, 5_500, None),
            Some(AgentActivityInfo { elapsed_ms: 3_500 })
        );
    }

    #[test]
    fn active_background_shell_blocks_idle_state() {
        use rebon_plugin_tasks::runtime::{
            BashTaskKind, LocalShellData, TaskData, TaskId, TaskStatus,
        };

        let mut shell = rebon_plugin_tasks::runtime::TaskSnapshot::new_pending(
            TaskId::new("sh_test"),
            "cargo check".into(),
            TaskData::LocalShell(LocalShellData {
                command: "cargo check".into(),
                exit_code: None,
                interrupted: false,
                display_kind: BashTaskKind::Bash,
                agent_id: None,
            }),
        );
        shell.status = TaskStatus::Running;
        shell.is_backgrounded = true;
        assert!(has_active_background_shell(&[shell.clone()]));

        shell.status = TaskStatus::Completed;
        assert!(!has_active_background_shell(&[shell]));
    }

    #[test]
    fn active_agent_footer_status_is_none_without_active_agents() {
        use rebon_plugin_tasks::runtime::{
            LocalAgentData, TaskData, TaskId, TaskKind, TaskSnapshot, TaskStatus,
        };

        let snapshots = vec![TaskSnapshot {
            id: TaskId::new("agent-1"),
            kind: TaskKind::LocalAgent,
            status: TaskStatus::Completed,
            title: "agent".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: false,
            notified: false,
            start_time_ms: 1_000,
            end_time_ms: Some(2_000),
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
        }];

        assert_eq!(active_agent_footer_status(&snapshots, 5_500, None), None);
    }

    #[test]
    fn idle_teammate_does_not_keep_active_footer_or_background_gate() {
        use rebon_plugin_tasks::runtime::{
            InProcessTeammateData, TaskData, TaskId, TaskKind, TaskSnapshot, TaskStatus,
            TeammateIdentity,
        };
        let snapshot = TaskSnapshot {
            id: TaskId::new("alice"),
            kind: TaskKind::InProcessTeammate,
            status: TaskStatus::Running,
            title: "alice".into(),
            last_progress: Some("waiting".into()),
            error: None,
            result: None,
            is_backgrounded: true,
            notified: false,
            start_time_ms: 1_000,
            end_time_ms: None,
            metadata: serde_json::json!({}),
            data: TaskData::InProcessTeammate(Box::new(InProcessTeammateData {
                identity: TeammateIdentity {
                    agent_id: "alice@team".into(),
                    agent_name: "alice".into(),
                    team_name: "team".into(),
                    color: None,
                    plan_mode_required: false,
                    parent_session_id: "leader".into(),
                },
                prompt: "work".into(),
                model: None,
                model_profile: None,
                permission_mode: "default".into(),
                awaiting_plan_approval: false,
                is_idle: true,
                shutdown_requested: false,
                pending_user_messages: Vec::new(),
                tool_use_count: 0,
                token_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
            })),
        };

        assert_eq!(
            active_agent_footer_status(&[snapshot.clone()], 5_500, None),
            None
        );
        assert!(!has_active_background_agent(&[snapshot]));
    }

    #[test]
    fn has_active_background_agent_tracks_backgrounded_agents() {
        use rebon_plugin_tasks::runtime::{
            LocalAgentData, TaskData, TaskId, TaskKind, TaskSnapshot, TaskStatus,
        };

        let mut snapshot = TaskSnapshot {
            id: TaskId::new("agent-1"),
            kind: TaskKind::LocalAgent,
            status: TaskStatus::Running,
            title: "agent".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: true,
            notified: false,
            start_time_ms: 1_000,
            end_time_ms: None,
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
        };

        assert!(has_active_background_agent(&[snapshot.clone()]));

        snapshot.status = TaskStatus::Completed;
        assert!(!has_active_background_agent(&[snapshot]));
    }

    #[test]
    fn footer_new_session_hint_waits_for_one_hour_of_inactivity() {
        assert_eq!(
            footer_new_session_hint(
                NEW_SESSION_HINT_TOKENS_THRESHOLD,
                false,
                false,
                false,
                false,
                false,
                NEW_SESSION_HINT_IDLE_DELAY - Duration::from_millis(1),
            ),
            None
        );
        assert_eq!(
            footer_new_session_hint(
                NEW_SESSION_HINT_TOKENS_THRESHOLD,
                false,
                false,
                false,
                false,
                false,
                NEW_SESSION_HINT_IDLE_DELAY,
            ),
            Some("/new to save ~8.0k tokens".into())
        );
    }

    #[test]
    fn footer_new_session_hint_is_hidden_while_any_agent_is_active() {
        assert_eq!(
            footer_new_session_hint(
                NEW_SESSION_HINT_TOKENS_THRESHOLD,
                false,
                true,
                false,
                false,
                false,
                NEW_SESSION_HINT_IDLE_DELAY,
            ),
            None
        );
    }

    #[test]
    fn footer_new_session_hint_is_hidden_while_background_agent_is_active() {
        assert_eq!(
            footer_new_session_hint(
                NEW_SESSION_HINT_TOKENS_THRESHOLD,
                false,
                false,
                true,
                false,
                false,
                NEW_SESSION_HINT_IDLE_DELAY,
            ),
            None
        );
    }

    #[test]
    fn footer_new_session_hint_is_shown_only_when_idle() {
        assert_eq!(
            footer_new_session_hint(
                NEW_SESSION_HINT_TOKENS_THRESHOLD,
                false,
                false,
                false,
                false,
                false,
                NEW_SESSION_HINT_IDLE_DELAY,
            ),
            Some("/new to save ~8.0k tokens".into())
        );
        assert_eq!(
            footer_new_session_hint(
                NEW_SESSION_HINT_TOKENS_THRESHOLD,
                true,
                false,
                false,
                false,
                false,
                NEW_SESSION_HINT_IDLE_DELAY,
            ),
            None
        );
        assert_eq!(
            footer_new_session_hint(
                NEW_SESSION_HINT_TOKENS_THRESHOLD,
                false,
                false,
                false,
                true,
                false,
                NEW_SESSION_HINT_IDLE_DELAY,
            ),
            None
        );
        assert_eq!(
            footer_new_session_hint(
                NEW_SESSION_HINT_TOKENS_THRESHOLD,
                false,
                false,
                false,
                false,
                true,
                NEW_SESSION_HINT_IDLE_DELAY,
            ),
            None
        );
    }
}
