//! Slash command parsing, registration, and local execution.
//!
//! This module collects all `/command` structs, their parsers, and
//! the side-effect-free execution logic (provider management,
//! context/prune/effort display, session reset, etc.).

use crate::goal::GoalState;
use rebon_dialog::settings_tabs::TabId;
use rebon_types::wall_clock_ms;
use rebon_types::ReasoningEffort;

use crate::session::commands::effort::{EffortCommand, FastCommand};
use crate::session::commands::name_ends_here;
use crate::tui::app::AppState;
use crate::tui::runner::custom_status_line::StatusLineRunResult;
use crate::tui::wiring::TuiEngineSession;

// ---------------------------------------------------------------------------
// Command structs
// ---------------------------------------------------------------------------

/// Parsed payload of a "/tasks" slash command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TasksCommand {
    /// Optional task id to open directly in detail mode.
    pub initial_detail_task_id: Option<String>,
}

/// Parsed payload of a "/workflows" slash command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkflowsCommand {
    /// Optional workflow task id to open directly in detail mode.
    pub initial_detail_task_id: Option<String>,
}

/// Parsed payload of a "/teams" slash command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TeamsCommand {
    /// Optional explicit team name to open.
    pub initial_team_name: Option<String>,
}

/// Parsed payload of an `/agents` slash command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AgentsCommand {
    /// Optional explicit agent type to preselect in the dialog.
    pub initial_agent_type: Option<String>,
}

/// Parsed payload of a local settings-surface command invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SettingsDialogCommand {
    /// Which tab should be active when the dialog opens.
    pub default_tab: TabId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GoalCommand {
    Show,
    Clear,
    Off,
    Stop,
    Archive,
    Set {
        prompt: String,
        max_sessions: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GoalCommandOutcome {
    Feedback(String),
    ConfirmReplace {
        prompt: String,
        max_sessions: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReviewCommand {
    Current,
    Base { branch: String },
    Commit { sha: String, title: Option<String> },
    Custom { instructions: String },
    Invalid(String),
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

/// Strip a leading `/name` off a composer line — see
/// [`rebon_slash_commands::strip_command_prefix`], which owns it now that the
/// aliases it consults are the catalog's.
pub(crate) use rebon_slash_commands::{strip_command_prefix, Surface};

/// Whether `text` is exactly this command, with nothing after it.
pub(super) fn is_bare_command(text: &str, name: &str) -> bool {
    strip_command_prefix(text, name).is_some_and(|rest| rest.trim().is_empty())
}

pub(super) fn parse_goal_command(text: &str) -> Option<GoalCommand> {
    let rest = strip_command_prefix(text, "goal")?;
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with(':') {
        return None;
    }
    let args = rest
        .trim_start_matches(|c: char| c == ' ' || c == ':')
        .trim();
    if args.is_empty() || args == "status" {
        return Some(GoalCommand::Show);
    }
    if args == "clear" {
        return Some(GoalCommand::Clear);
    }
    if args == "off" {
        return Some(GoalCommand::Off);
    }
    if args == "stop" {
        return Some(GoalCommand::Stop);
    }
    if matches!(args, "archive" | "archived") {
        return Some(GoalCommand::Archive);
    }

    let mut max_sessions = None;
    let mut prompt_parts = Vec::new();
    let mut iter = args.split_whitespace().peekable();
    while let Some(part) = iter.next() {
        if let Some(value) = part.strip_prefix("--max-sessions=") {
            if let Ok(parsed) = value.parse::<u32>() {
                max_sessions = Some(parsed.max(1));
            }
            continue;
        }
        if part == "--max-sessions" {
            if let Some(value) = iter.next() {
                if let Ok(parsed) = value.parse::<u32>() {
                    max_sessions = Some(parsed.max(1));
                }
            }
            continue;
        }
        prompt_parts.push(part);
    }
    let prompt = prompt_parts.join(" ").trim().to_string();
    if prompt.is_empty() {
        Some(GoalCommand::Show)
    } else {
        Some(GoalCommand::Set {
            prompt,
            max_sessions,
        })
    }
}

pub(super) fn execute_goal_command(app: &mut AppState, command: GoalCommand) -> GoalCommandOutcome {
    match command {
        GoalCommand::Show => GoalCommandOutcome::Feedback(match &app.goal {
            Some(goal) if goal.is_archived() => format!(
                "goal archived after {} session(s): {}",
                goal.sessions_started, goal.prompt
            ),
            Some(goal) if goal.is_complete() => {
                let suffix = goal
                    .completed_reason
                    .as_ref()
                    .map(|reason| format!(" Reason: {reason}"))
                    .unwrap_or_default();
                format!(
                    "goal complete ({}): {}{}\nUse /goal archive to archive it.",
                    goal_session_count(goal),
                    goal.prompt,
                    suffix
                )
            }
            Some(goal) if goal.is_paused() => format!(
                "goal paused ({}): {}",
                goal_session_count(goal),
                goal.prompt
            ),
            Some(goal) => format!(
                "goal active ({}): {}",
                goal_session_count(goal),
                goal.prompt
            ),
            None if app.pending_goal_clarification.is_some() => {
                let pending = app
                    .pending_goal_clarification
                    .as_ref()
                    .expect("pending goal checked");
                format!(
                    "goal clarification pending: {}\nReply with concrete outcome, scope, and verification criteria.",
                    pending.prompt
                )
            }
            None => "no goal is active. Use /goal <objective> to start one.".to_string(),
        }),
        GoalCommand::Clear => {
            app.pending_goal_clarification = None;
            app.goal = None;
            app.deferred_goal_submit_payloads.clear();
            GoalCommandOutcome::Feedback("goal cleared".to_string())
        }
        GoalCommand::Off => {
            app.pending_goal_clarification = None;
            app.goal = None;
            app.deferred_goal_submit_payloads.clear();
            GoalCommandOutcome::Feedback("goal turned off".to_string())
        }
        GoalCommand::Stop => {
            app.pending_goal_clarification = None;
            app.deferred_goal_submit_payloads.clear();
            if let Some(goal) = app.goal.as_mut() {
                if goal.is_complete() {
                    GoalCommandOutcome::Feedback("goal is complete".to_string())
                } else if goal.is_archived() {
                    GoalCommandOutcome::Feedback("goal already archived".to_string())
                } else {
                    goal.mark_paused();
                    GoalCommandOutcome::Feedback("goal paused".to_string())
                }
            } else {
                GoalCommandOutcome::Feedback("no goal is active".to_string())
            }
        }
        GoalCommand::Archive => {
            if let Some(goal) = app.goal.as_mut() {
                if !goal.is_complete() && !goal.is_archived() {
                    return GoalCommandOutcome::Feedback(
                        "goal is not complete yet; use /goal stop to pause or /goal clear to remove it"
                            .to_string(),
                    );
                }
                app.deferred_goal_submit_payloads.clear();
                app.pending_goal_clarification = None;
                if goal.is_archived() {
                    return GoalCommandOutcome::Feedback("goal already archived".to_string());
                }
                goal.mark_archived_with_time(wall_clock_ms());
                GoalCommandOutcome::Feedback("goal archived".to_string())
            } else {
                GoalCommandOutcome::Feedback("no goal is active".to_string())
            }
        }
        GoalCommand::Set {
            prompt,
            max_sessions,
        } => {
            if app
                .goal
                .as_ref()
                .is_some_and(|goal| goal.is_complete() || goal.is_archived())
            {
                return GoalCommandOutcome::ConfirmReplace {
                    prompt,
                    max_sessions,
                };
            }
            GoalCommandOutcome::Feedback(set_goal(app, prompt, max_sessions))
        }
    }
}

pub(super) fn set_goal(app: &mut AppState, prompt: String, max_sessions: Option<u32>) -> String {
    let mut goal = crate::goal::GoalState::new_with_started_at(prompt, wall_clock_ms());
    if let Some(max_sessions) = max_sessions {
        goal = goal.with_max_sessions(max_sessions);
    }
    let prompt = goal.prompt.clone();
    app.pending_goal_clarification = None;
    app.goal = Some(goal);
    format!("goal set: {prompt}")
}

fn goal_session_count(goal: &GoalState) -> String {
    match goal.max_sessions {
        Some(max_sessions) => format!("{}/{max_sessions} sessions", goal.sessions_started),
        None if goal.sessions_started == 1 => "1 session".to_string(),
        None => format!("{} sessions", goal.sessions_started),
    }
}

/// Recognize `/tasks` — or `/bg` — with an optional task-id argument.
///
/// `/bg` used to background the session here and open this view on the desktop.
/// The desktop's meaning won: a user who types `/bg` expecting a list and gets
/// one notices at once, while the reverse silently moves their session.
/// `/background` still backgrounds — see [`parse_background_session_command`].
pub(super) fn parse_tasks_command(text: &str) -> Option<TasksCommand> {
    let initial_detail_task_id = parse_optional_local_command_arg(text, "tasks")?;
    Some(TasksCommand {
        initial_detail_task_id,
    })
}

pub(super) fn parse_workflows_command(text: &str) -> Option<WorkflowsCommand> {
    let initial_detail_task_id = parse_optional_local_command_arg(text, "workflows")?;
    Some(WorkflowsCommand {
        initial_detail_task_id,
    })
}

fn parse_optional_local_command_arg(text: &str, name: &str) -> Option<Option<String>> {
    let rest = strip_command_prefix(text, name)?;
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with(':') {
        return None;
    }
    let arg = rest
        .trim_start_matches(|c: char| c == ' ' || c == ':')
        .trim();
    Some(if arg.is_empty() {
        None
    } else {
        Some(arg.to_string())
    })
}

/// Recognize "/teams" (with an optional team-name argument) in the
/// current prompt input.
pub(super) fn parse_teams_command(text: &str) -> Option<TeamsCommand> {
    let initial_team_name = parse_optional_local_command_arg(text, "teams")?;
    Some(TeamsCommand { initial_team_name })
}

/// Recognize `/agents` (optionally with an agent type argument) in
/// the current prompt input.
pub(super) fn parse_agents_command(text: &str) -> Option<AgentsCommand> {
    let initial_agent_type = parse_optional_local_command_arg(text, "agents")?;
    Some(AgentsCommand { initial_agent_type })
}

/// Recognize `/background` and return an optional final instruction.
///
/// Deliberately not `/bg` any more: a spelling that means "show me a list" in
/// one front end must not move a session in the other. See
/// [`parse_tasks_command`].
pub(super) fn parse_background_session_command(text: &str) -> Option<Option<String>> {
    let rest = strip_command_prefix(text.trim_end(), "background")?;
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with(':') {
        return None;
    }
    let arg = rest
        .trim_start_matches(|c: char| c == ' ' || c == ':')
        .trim();
    Some((!arg.is_empty()).then(|| arg.to_string()))
}

/// Recognize `/background-agents` — and `/agent-view`, which is not in the
/// catalog and stays an undocumented spelling for the same overlay.
pub(super) fn parse_agent_view_command(text: &str) -> bool {
    ["background-agents", "agent-view"]
        .into_iter()
        .filter_map(|name| strip_command_prefix(text, name))
        .any(|rest| rest.is_empty() || rest.starts_with(' ') || rest.starts_with(':'))
}

/// Recognize `/config` and `/settings`, optionally with a tab name.
///
/// Supported tab arguments:
/// * `config`
/// * `status`
/// * `usage`
pub(super) fn parse_settings_dialog_command(text: &str) -> Option<SettingsDialogCommand> {
    // `/config` is an alias of `/settings` in the catalog, so one lookup covers
    // both spellings; they open the same tab either way.
    let rest = strip_command_prefix(text, "settings")?;
    let default_tab = TabId::Config;

    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with(':') {
        return None;
    }

    let arg = rest
        .trim_start_matches(|c: char| c == ' ' || c == ':')
        .trim()
        .to_ascii_lowercase();
    let default_tab = match arg.as_str() {
        "" | "config" | "settings" => default_tab,
        "status" => TabId::Status,
        "usage" => TabId::Usage,
        _ => return None,
    };

    Some(SettingsDialogCommand { default_tab })
}

/// Recognize `/vim` with only optional surrounding whitespace.
/// `/hosted` — move this session into a background worker and keep mirroring
/// it, so closing the terminal detaches instead of killing the session and
/// everything it spawned.
pub(super) fn parse_hosted_command(text: &str) -> bool {
    is_bare_command(text, "hosted")
}

pub(super) fn parse_vim_command(text: &str) -> bool {
    is_bare_command(text, "vim")
}

/// Recognize `/skills` with only optional surrounding whitespace.
pub(super) fn parse_skills_command(text: &str) -> bool {
    is_bare_command(text, "skills")
}

/// Recognize `/memory` with only optional surrounding whitespace.
pub(super) fn parse_memory_command(text: &str) -> bool {
    is_bare_command(text, "memory")
}

/// Recognize `/context` with only optional surrounding whitespace.
pub(super) fn parse_context_command(text: &str) -> bool {
    is_bare_command(text, "context")
}

/// Recognize `/status` with only optional surrounding whitespace.
pub(super) fn parse_status_command(text: &str) -> bool {
    is_bare_command(text, "status")
}

/// Recognize `/statusline` with only optional surrounding whitespace.
pub(super) fn parse_statusline_command(text: &str) -> bool {
    is_bare_command(text, "statusline")
}

pub(super) fn execute_statusline_command(app: &AppState) -> String {
    let Some(config) = app.custom_status_line.config.as_ref() else {
        return String::from(
            "No statusLine is configured. Add a top-level statusLine object to settings.json, for example {\"type\":\"command\",\"command\":\"...\",\"padding\":2,\"refreshInterval\":1}.",
        );
    };

    let (source_kind, source) = config.source_label();
    let mut lines = vec![
        String::from("statusLine is configured."),
        format!("{source_kind}: {source}"),
        format!("refreshInterval: {}s", config.refresh_interval_secs()),
        format!("padding: {}", config.padding()),
    ];

    if app.custom_status_line.in_flight {
        lines.push(String::from("state: command is currently running"));
    } else if let Some(result) = app.custom_status_line.last_result() {
        match result {
            StatusLineRunResult::Rendered => lines.push(format!(
                "state: rendered {} line(s)",
                app.custom_status_line.output.len()
            )),
            StatusLineRunResult::Failed(err) => {
                if app.custom_status_line.output.is_empty() {
                    lines.push(format!("state: hidden because {}", err.reason()))
                } else {
                    lines.push(format!(
                        "state: keeping previous output because latest run failed: {}",
                        err.reason()
                    ))
                }
            }
        }
    } else {
        lines.push(String::from("state: command has not completed yet"));
    }

    lines.join("\n")
}

/// Returns `Some(args)` when `text` is the `/kernel` command.
///
/// A first-class name on purpose: `/agent <x>` is shadowed by the
/// sub-agent spawner's identically-named command in the submit path
/// (only the `/agent:<id>` colon form reaches the backend switch), so
/// the kernel switch gets a name nothing else claims. Space and colon
/// forms both work (`/kernel dsh`, `/kernel:dsh`).
pub(super) fn parse_kernel_command(text: &str) -> Option<&str> {
    let rest = strip_command_prefix(text.trim_end(), "kernel")?;
    let args = rest.trim_start_matches([' ', ':']).trim();
    if rest.is_empty() || rest.starts_with(' ') || rest.starts_with(':') {
        Some(args)
    } else {
        None
    }
}

pub(super) fn parse_cost_command(text: &str) -> bool {
    is_bare_command(text, "cost")
}

pub(super) fn parse_doctor_command(text: &str) -> bool {
    is_bare_command(text, "doctor")
}

pub(super) fn parse_review_command(text: &str) -> Option<ReviewCommand> {
    let rest = strip_command_prefix(text.trim_end(), "review")?;
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with(':') {
        return None;
    }
    let args = rest
        .trim_start_matches(|c: char| c == ' ' || c == ':')
        .trim();
    if args.is_empty() {
        return Some(ReviewCommand::Current);
    }
    if let Some(after_flag) = args.strip_prefix("--base") {
        if !after_flag.is_empty() && !after_flag.starts_with(' ') {
            return Some(ReviewCommand::Custom {
                instructions: args.to_string(),
            });
        }
        let branch = after_flag.trim();
        if branch.is_empty() {
            return Some(ReviewCommand::Invalid(
                "Usage: /review --base <branch>".to_string(),
            ));
        }
        if branch.split_whitespace().count() != 1 {
            return Some(ReviewCommand::Invalid(
                "Branch names for /review --base must not contain spaces.".to_string(),
            ));
        }
        return Some(ReviewCommand::Base {
            branch: branch.to_string(),
        });
    }
    if let Some(after_flag) = args.strip_prefix("--commit") {
        if !after_flag.is_empty() && !after_flag.starts_with(' ') {
            return Some(ReviewCommand::Custom {
                instructions: args.to_string(),
            });
        }
        let mut parts = after_flag.trim().splitn(2, char::is_whitespace);
        let sha = parts.next().unwrap_or("").trim();
        if sha.is_empty() {
            return Some(ReviewCommand::Invalid(
                "Usage: /review --commit <sha> [title...]".to_string(),
            ));
        }
        let title = parts.next().map(str::trim).filter(|s| !s.is_empty());
        return Some(ReviewCommand::Commit {
            sha: sha.to_string(),
            title: title.map(str::to_string),
        });
    }
    Some(ReviewCommand::Custom {
        instructions: args.to_string(),
    })
}

/// Recognize `/help` with only optional surrounding whitespace.
pub(super) fn parse_help_command(text: &str) -> bool {
    is_bare_command(text, "help")
}

/// Recognize `/onboarding`.
pub(super) fn parse_onboarding_command(text: &str) -> bool {
    is_bare_command(text, "onboarding")
}

/// Recognize `/theme`. Opens a dedicated theme-picker dialog
/// (a one-step onboarding overlay) instead of advancing through
/// the full onboarding flow.
pub(super) fn parse_theme_command(text: &str) -> bool {
    is_bare_command(text, "theme")
}

/// Recognize `/migrate`. Opens the import step of the onboarding
/// overlay on its own, so Claude Code / Codex content installed after
/// setup can be imported without re-running the whole wizard.
pub(super) fn parse_migrate_command(text: &str) -> bool {
    is_bare_command(text, "migrate")
}

/// Recognize `/exit` or `/quit` in the current prompt input.
pub(super) fn parse_exit_command(text: &str) -> bool {
    is_bare_command(text, "exit")
}

/// Recognize `/stop` in the current prompt input.
pub(super) fn parse_stop_command(text: &str) -> bool {
    is_bare_command(text, "stop")
}

/// Recognize "/new" or "/clear" in the current prompt input.
pub(super) fn parse_new_or_clear_command(text: &str) -> bool {
    is_bare_command(text, "new") || is_bare_command(text, "clear")
}

/// Returns `Some(())` when the text names `/profile`.
pub(super) fn parse_profile_command(text: &str) -> Option<()> {
    name_ends_here(strip_command_prefix(text, "profile")?).then_some(())
}

/// Recognize "/run <command>" in the current prompt input. Returns
/// `Some(command)` when the line starts with "/run" followed by a
/// space, `None` otherwise. The trailing command is returned
/// unmodified (not trimmed) so callers can forward the exact byte
/// sequence to the shell.
///
/// Strict prefix match: "/runfoo" is not a match (prevents
/// accidental capture of unrelated slash commands).
pub(super) fn parse_run_command(text: &str) -> Option<String> {
    let rest = strip_command_prefix(text, "run")?;
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None;
    }
    Some(rest.trim_start().to_owned())
}

/// Recognize "/agent <prompt>" in the current prompt input. Returns
/// `Some(prompt)` when the line starts with "/agent" followed by a
/// space, `None` otherwise. Same strict-prefix rule as
/// [`parse_run_command`].
pub(super) fn parse_agent_command(text: &str) -> Option<String> {
    let rest = strip_command_prefix(text, "agent")?;
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None;
    }
    Some(rest.trim_start().to_owned())
}

pub(super) fn parse_effort_command(text: &str) -> Option<EffortCommand> {
    let rest = strip_command_prefix(text, "effort")?;
    if rest.is_empty() {
        return Some(EffortCommand::Show);
    }
    if !rest.starts_with(' ') {
        return None; // "/effortfoo" is not a match
    }
    let arg = rest.trim();
    if arg.is_empty() {
        return Some(EffortCommand::Show);
    }
    let lowered = arg.to_ascii_lowercase();
    if lowered == "auto" {
        return Some(EffortCommand::Auto);
    }
    // `/effort med` has always worked. The alias belongs to the command, not
    // to the type — `ReasoningEffort::from_str` takes none — so it is folded
    // to canonical here.
    let canonical = if lowered == "med" { "medium" } else { &lowered };
    Some(match canonical.parse::<ReasoningEffort>() {
        Ok(level) => EffortCommand::Set(level),
        Err(_) => EffortCommand::Invalid(lowered.clone()),
    })
}

pub(super) fn parse_fast_command(text: &str) -> Option<FastCommand> {
    let rest = strip_command_prefix(text, "fast")?;
    if rest.is_empty() {
        return Some(FastCommand::Status);
    }
    if !rest.starts_with(' ') {
        return None;
    }
    let arg = rest.trim();
    if arg.is_empty() {
        return Some(FastCommand::Status);
    }
    Some(match arg.to_ascii_lowercase().as_str() {
        "status" => FastCommand::Status,
        "on" => FastCommand::On,
        "off" => FastCommand::Off,
        other => FastCommand::Invalid(other.to_string()),
    })
}

// ---------------------------------------------------------------------------
// Handlers / executors
// ---------------------------------------------------------------------------

/// Reset the session state for a "/new" or "/clear" command.
/// Clears the conversation: wipe transcript, reset
/// scroll/overlay/pickers, preserve background tasks and history.
pub(super) fn apply_new_session(app: &mut AppState, session: &mut TuiEngineSession) {
    apply_new_session_with_inline_banner(app, session, false);
}

pub(super) fn apply_user_new_session(app: &mut AppState, session: &mut TuiEngineSession) {
    apply_new_session_with_inline_banner(app, session, true);
}

fn apply_new_session_with_inline_banner(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    show_inline_banner: bool,
) {
    if app.is_loading {
        super::inject_system_message(
            app,
            "error",
            "Cannot replace the session while a prompt is running.",
        );
        app.follow_transcript_tail = true;
        return;
    }
    // 0. Create a new server-side session so subsequent messages are
    //    persisted to a fresh JSONL file instead of corrupting the old
    //    session's transcript. Without this, `session_id` stays the
    //    same and `push_transcript_entries` keeps appending to the
    //    old record's `loaded_transcript`.
    let state = session.server_state.clone();
    let default_permission_mode = crate::rebon_config::saved_default_permission_mode()
        .unwrap_or(rebon_permissions::PermissionMode::Default);
    let new_record = state.create_session_with_permission_mode(
        session.cwd.clone(),
        Vec::new(),
        default_permission_mode.as_wire(),
    );
    tracing::info!(
        old_session = %session.session_id,
        new_session = %new_record.id,
        "rebon: /new created fresh server-side session"
    );
    let active_lock = match rebon_session::try_acquire_session_active_lock(
        &session.projects_root,
        &session.cwd,
        &new_record.id,
    ) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            super::inject_system_message(
                app,
                "error",
                "Cannot start a new session because the new session id is already active.",
            );
            app.follow_transcript_tail = true;
            return;
        }
        Err(err) => {
            super::inject_system_message(
                app,
                "error",
                &format!("Cannot start a new session: could not acquire active lock: {err}"),
            );
            app.follow_transcript_tail = true;
            return;
        }
    };
    let previous_session_id = session.session_id.clone();
    let previous_scratchpad = session.owned_scratchpad();
    if let Err(err) = session.swap_runtime(&new_record.id, &new_record.cwd, app.is_loading) {
        super::inject_system_message(
            app,
            "error",
            &format!("Cannot start a new session: could not build session runtime: {err}"),
        );
        app.follow_transcript_tail = true;
        return;
    }
    session
        .engine_half
        .tasks
        .close_owner_session(&previous_session_id);
    if let Some(scratchpad) = previous_scratchpad {
        scratchpad.remove();
    }
    app.mid_turn_queued_submit_poller = Some(session.engine_half.runtime.mid_turn_queue.clone());
    std::env::set_var("REBON_SESSION_ID", &new_record.id);
    rebon_tool::clear_current_team_name();
    session.session_active_lock = active_lock;
    session.attached_background_job_id = None;
    // The user is done with the old session; its worker's lease releases
    // deliberately so it shuts down on policy instead of lingering for a
    // return that is not coming.
    if let Some(previous) = session.remote_background_attachment.take() {
        previous.mark_exit_deliberate();
    }
    finish_new_session(app, session, show_inline_banner, default_permission_mode);
}

/// `/new` in a hosted terminal: the fresh session starts in a worker and
/// this terminal is its mirror from birth,
/// rather than being built here, locked here, and handed over afterwards
/// with a "Moving this session…" line that never described it. Silent: the
/// status bar's dot is the whole signal, as at startup. Returns whether the
/// session was replaced; a worker that could not be started leaves the
/// caller to open a local session instead, and says why.
pub(super) fn apply_new_hosted_session(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    show_inline_banner: bool,
) -> bool {
    apply_new_hosted_session_with(
        app,
        session,
        show_inline_banner,
        crate::session_shell::hosted_startup::start_hosted_session_with_runtime,
    )
}

/// [`apply_new_hosted_session`] with the step that names the session and
/// starts its worker passed in, so a test can do it against its own store.
pub(super) fn apply_new_hosted_session_with(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    show_inline_banner: bool,
    host: impl FnOnce(
        crate::background::BackgroundRuntimeFields,
        &str,
    ) -> anyhow::Result<super::HostedStartup>,
) -> bool {
    if app.is_loading {
        super::inject_system_message(
            app,
            "error",
            "Cannot replace the session while a prompt is running.",
        );
        app.follow_transcript_tail = true;
        return false;
    }
    let default_permission_mode = crate::rebon_config::saved_default_permission_mode()
        .unwrap_or(rebon_permissions::PermissionMode::Default);
    // The worker starts under the mode the new session opens with, and
    // the model, effort and channels this terminal has — the same runtime a
    // handover would record.
    let mode_before = app.permission_mode;
    app.set_permission_mode(default_permission_mode);
    let runtime = crate::session_shell::handover::background_runtime_from_session(
        session,
        session.ui_mode,
        app.effort_level,
        app.permission_mode,
    );
    app.set_permission_mode(mode_before);
    let cwd = session.cwd.clone();
    let hosted = match host(runtime, &cwd) {
        Ok(hosted) => hosted,
        Err(err) => {
            super::inject_system_message(
                app,
                "error",
                &format!(
                    "Could not start a background worker for a new session ({err:#}); opening one in this process instead, as with --local."
                ),
            );
            app.follow_transcript_tail = true;
            return false;
        }
    };
    tracing::info!(
        old_session = %session.session_id,
        new_session = %hosted.session_id,
        job_id = %hosted.job_id,
        "rebon: /new started a hosted session"
    );
    // The record under the minted id: what the mirror's runtime is keyed
    // on until the worker's transcript takes over. No lock — the worker
    // takes it when it resumes.
    let state = session.server_state.clone();
    let new_record = state.restore_empty_session(
        hosted.session_id.clone(),
        cwd.clone(),
        Vec::new(),
        default_permission_mode.as_wire(),
    );
    let previous_session_id = session.session_id.clone();
    let previous_scratchpad = session.owned_scratchpad();
    if let Err(err) = session.swap_runtime(&new_record.id, &new_record.cwd, app.is_loading) {
        super::inject_system_message(
            app,
            "error",
            &format!("Cannot start a new session: could not build session runtime: {err}"),
        );
        app.follow_transcript_tail = true;
        return true;
    }
    session
        .engine_half
        .tasks
        .close_owner_session(&previous_session_id);
    if let Some(scratchpad) = previous_scratchpad {
        scratchpad.remove();
    }
    app.mid_turn_queued_submit_poller = Some(session.engine_half.runtime.mid_turn_queue.clone());
    std::env::set_var("REBON_SESSION_ID", &new_record.id);
    rebon_tool::clear_current_team_name();
    // A mirror holds no lock, hosts no MCP servers and runs no cron
    // scheduler: the worker's are the session's (the previous session, if it
    // was a mirror, had none).
    session.session_active_lock = None;
    session.release_mcp();
    session.stop_cron_scheduler();
    session.attached_background_job_id = Some(hosted.job_id.clone());
    // As in the local route: the old session's worker is deliberately let
    // go, not left to linger behind the fresh one.
    if let Some(previous) = session.remote_background_attachment.take() {
        previous.mark_exit_deliberate();
    }
    session.pending_hosted_session = Some(crate::background::PendingHostedSession::startup(
        hosted.job_id,
    ));
    finish_new_session(app, session, show_inline_banner, default_permission_mode);
    true
}

/// What every new session does to the app once its runtime is in, whichever
/// process hosts it.
fn finish_new_session(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    show_inline_banner: bool,
    default_permission_mode: rebon_permissions::PermissionMode,
) {
    app.set_permission_mode(default_permission_mode);
    session
        .engine_half
        .handler
        .seed_config_option_value("permissions", default_permission_mode.as_wire());

    // 0b. The fresh immutable runtime owns an independent model continuation
    //     handle, MCP cache, hooks, skills and per-session pollers. Do not reset
    //     the shared provider handle here: detached old turns still own it.
    session.model.prune_level.clear_usage();
    app.usage_mut().reset(wall_clock_ms());
    app.streaming_token_count = 0;
    app.session_title = None;
    app.prompt_completion_status = None;
    app.custom_status_line.force_refresh();

    // 1. Wipe every transcript view and its measurement cache.
    app.reset_transcript_views();
    if app.ui_mode == crate::ui_config::UiMode::Inline {
        app.pending_inline_viewport_reset = true;
        app.pending_inline_startup_banner |= show_inline_banner;
    }
    // 2. Reset scroll and follow state.
    app.scroll_offset = 0;
    app.follow_transcript_tail = true;
    app.total_content_lines = 0;
    // 3. Clear queued prompts and undo stack.
    app.queued_commands.clear();
    app.queued_submit_payloads.clear();
    app.deferred_goal_submit_payloads.clear();
    app.deferred_internal_submit_payloads.clear();
    session.engine_half.runtime.mid_turn_queue.clear_pending();
    if let Some(poller) = app.mid_turn_queued_submit_poller.as_ref() {
        poller.clear_pending();
    }
    app.queued_auto_drain_paused_after_withdrawal = false;
    app.undo_stack.clear();
    // 4. Reset paste state.
    app.pasted_contents.clear();
    app.next_paste_id = 1;
    // 5. Dismiss any open pickers/overlays.
    app.slash_picker = None;
    app.at_mention_picker = None;
    app.pending_permission_view = None;
    // 6. Reset the unseen divider.
    app.unseen_divider = rebon_tui::layout::unseen_divider::UnseenDividerState::new(0);
    // 7. Clear hidden tool call ids from previous turns.
    app.hidden_tool_call_ids.clear();
    app.background_agent_tool_tasks.clear();
    app.remote_background_tasks.clear();
    app.live_agent_tool_activity.clear();
    app.live_agent_tool_activity_revision = app.live_agent_tool_activity_revision.wrapping_add(1);
    app.task_completion_timestamps.clear();
    app.prev_task_snapshot.clear();
    app.task_hide_deadline_ms = None;
    app.task_list_collapsed = false;
    app.task_list_prev_count = 0;
    // 8. Clear session-local goal and ultraplan presentation state.
    app.goal = None;
    app.ultraplan_status = None;
}

fn slash_command_exists(commands: &[rebon_types::SlashCommand], name: &str) -> bool {
    commands
        .iter()
        .any(|command| command.matches_name_or_alias(name))
}

/// Add the catalog's commands to a picker list, skipping names it already
/// holds.
///
/// The picker is seeded straight from the catalog, but that happens before the
/// session exists and the session is what boots the kernel, so the seed can
/// only see the built-in fallback table. This runs again once the seat is
/// live. Built-ins go in *before* skills, so a skill named after a built-in is
/// the one dropped.
pub(super) fn register_local_slash_commands(commands: &mut Vec<rebon_types::SlashCommand>) {
    for spec in rebon_slash_commands::all() {
        if !spec.available_on(Surface::Tui) {
            continue;
        }
        if slash_command_exists(commands, spec.name.as_ref()) {
            continue;
        }
        commands.push(spec.to_wire());
    }
}

pub(super) fn register_registered_skill_slash_commands(
    commands: &mut Vec<rebon_types::SlashCommand>,
    registry: &rebon_plugin_skill::SkillRegistry,
) {
    use rebon_types::SlashCommandCategory;

    for skill in registry.entries() {
        if registry.is_disabled(&skill.id) || !skill.user_invocable {
            continue;
        }
        if slash_command_exists(commands, &skill.id) {
            continue;
        }
        let input = skill
            .argument_hint
            .as_ref()
            .map(|hint| rebon_types::SlashCommandInput {
                hint: Some(hint.clone()),
            });
        commands.push(rebon_types::SlashCommand {
            name: skill.id,
            description: skill.description,
            input,
            category: Some(SlashCommandCategory::Skill),
            aliases: Vec::new(),
        });
    }
}

/// Replace the entire dynamic Skill category from the current enabled
/// registry snapshot. Rebuilding rather than comparing counts handles an
/// equal-size swap (one skill disabled while another is enabled) and removes
/// commands that were registered before a skill became disabled.
pub(super) fn refresh_registered_skill_slash_commands(
    commands: &mut Vec<rebon_types::SlashCommand>,
    registry: &rebon_plugin_skill::SkillRegistry,
) {
    commands.retain(|command| command.category != Some(rebon_types::SlashCommandCategory::Skill));
    register_registered_skill_slash_commands(commands, registry);
}

// ---------------------------------------------------------------------------
// /cost, /doctor, and /review commands
// ---------------------------------------------------------------------------

pub(super) fn synthesize_review_prompt(command: &ReviewCommand) -> Result<String, String> {
    let target = match command {
        ReviewCommand::Current => "current uncommitted changes".to_string(),
        ReviewCommand::Base { branch } if !branch.trim().is_empty() => {
            format!("changes compared with base branch `{}`", branch.trim())
        }
        ReviewCommand::Commit { sha, title } if !sha.trim().is_empty() => match title {
            Some(title) => format!("commit `{}` ({})", sha.trim(), title.trim()),
            None => format!("commit `{}`", sha.trim()),
        },
        ReviewCommand::Custom { instructions } if !instructions.trim().is_empty() => {
            format!(
                "current changes with these additional instructions: {}",
                instructions.trim()
            )
        }
        ReviewCommand::Invalid(message) => return Err(message.clone()),
        _ => return Err("Invalid /review target.".to_string()),
    };

    Ok(format!(
        "You are performing a strict code review. Inspect the {target}. Focus only on bugs, regressions, security issues, data loss risks, concurrency hazards, and missing or broken tests. Avoid praise, summaries, style-only nits, and other noise. If you find issues, report them as concise findings with file paths and line numbers whenever possible, explain the impact, and suggest a concrete fix. If no high-confidence issues are found, say so briefly."
    ))
}

// ---------------------------------------------------------------------------
// /context command
// ---------------------------------------------------------------------------

/// Look up the tool name for a tool_result by scanning earlier assistant
/// messages for the matching tool_use id.
// ---------------------------------------------------------------------------
// /compact command — deferred compaction
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// /effort command — set or display effort / thinking level
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::commands::command_args;
    use crate::session::commands::context::{parse_compact_command, parse_prune_command};
    use crate::session::commands::effort::{
        execute_effort_command, execute_persisted_effort_command, load_persisted_effort_level,
    };
    use crate::session::commands::model::parse_model_command;
    use crate::session::commands::plugin::parse_plugin_command;
    use crate::session::commands::EnvRestore;

    use super::super::test_support::make_test_tui_session;
    use tempfile::TempDir;

    #[test]
    fn handler_args_follow_the_recognizer_not_a_literal_prefix() {
        // The recognizer accepts case variants, aliases, and the colon form;
        // a handler that re-strips a lowercase literal read `/Provider add …`
        // as no arguments at all and silently ran `list` instead.
        assert_eq!(
            command_args("/Provider add deepseek sk-x", "provider"),
            "add deepseek sk-x"
        );
        assert_eq!(
            command_args("/plugins install foo", "plugin"),
            "install foo"
        );
        assert_eq!(command_args("/PLUGIN enable x", "plugin"), "enable x");
        assert_eq!(command_args("/Model: gpt-5", "model"), "gpt-5");
        assert_eq!(command_args("/model", "model"), "");
        // A longer word is a different command, not this one with arguments.
        assert_eq!(command_args("/modelling x", "model"), "");
    }

    #[test]
    fn statusline_command_reports_missing_configuration() {
        let app = AppState::default();

        let output = execute_statusline_command(&app);

        assert!(output.contains("No statusLine is configured"));
        assert!(output.contains("settings.json"));
    }

    #[test]
    fn statusline_command_reports_configured_runtime_state() {
        let mut app = AppState::default();
        app.custom_status_line.config = Some(crate::ui_config::StatusLineConfig {
            kind: crate::ui_config::StatusLineKind::Command,
            command: String::from("echo ok"),
            script: None,
            padding: Some(2),
            refresh_interval: Some(3),
            hide_vim_mode_indicator: Some(false),
        });

        let output = execute_statusline_command(&app);

        assert!(output.contains("statusLine is configured"));
        assert!(output.contains("command: echo ok"));
        assert!(output.contains("refreshInterval: 3s"));
        assert!(output.contains("padding: 2"));
        assert!(output.contains("state: command has not completed yet"));
    }

    #[test]
    fn parse_review_command_variants() {
        assert_eq!(
            parse_review_command("/review"),
            Some(ReviewCommand::Current)
        );
        assert_eq!(
            parse_review_command("/review --base main"),
            Some(ReviewCommand::Base {
                branch: "main".into()
            })
        );
        assert_eq!(
            parse_review_command("/review --commit abc123 Fix thing"),
            Some(ReviewCommand::Commit {
                sha: "abc123".into(),
                title: Some("Fix thing".into())
            })
        );
        assert_eq!(
            parse_review_command("/review focus on security"),
            Some(ReviewCommand::Custom {
                instructions: "focus on security".into()
            })
        );
        assert!(matches!(
            parse_review_command("/review --base"),
            Some(ReviewCommand::Invalid(_))
        ));
        assert!(parse_review_command("/reviewer").is_none());
    }

    #[test]
    fn synthesize_review_prompt_is_strict_and_targets_base() {
        let prompt = synthesize_review_prompt(&ReviewCommand::Base {
            branch: "main".into(),
        })
        .unwrap();
        assert!(prompt.contains("strict code review"));
        assert!(prompt.contains("base branch `main`"));
        assert!(prompt.contains("bugs, regressions, security issues"));
        assert!(!prompt.contains("/review --base main"));
    }

    #[test]
    fn cost_and_doctor_commands_parse_locally() {
        assert!(parse_cost_command("/cost"));
        assert!(!parse_cost_command("/cost now"));
        assert!(parse_doctor_command("/doctor"));
        assert!(!parse_doctor_command("/doctor please"));
    }

    #[test]
    fn background_session_command_parses_its_prompt() {
        assert_eq!(parse_background_session_command("/background"), Some(None));
        assert_eq!(
            parse_background_session_command("/background finish this turn"),
            Some(Some("finish this turn".to_string()))
        );
        assert_eq!(
            parse_background_session_command("/background:run tests"),
            Some(Some("run tests".to_string()))
        );
        assert_eq!(parse_background_session_command("/bground"), None);
        assert_eq!(parse_background_session_command("/backgrounded"), None);
        // `/bg` belongs to `/tasks` now — see
        // `bg_shows_the_task_view_and_background_still_moves_the_session`.
        assert_eq!(parse_background_session_command("/bg"), None);
    }

    #[test]
    fn stop_command_matches_exact_command_only() {
        assert!(parse_stop_command("/stop"));
        assert!(!parse_stop_command("/stop now"));
        assert!(!parse_stop_command("/stopped"));
    }

    #[test]
    fn register_registered_skill_slash_commands_includes_user_skills() {
        let mut commands = Vec::new();
        register_local_slash_commands(&mut commands);
        let registry = rebon_plugin_skill::SkillRegistry::new();
        registry.register(rebon_plugin_skill::Skill {
            id: "project-skill".into(),
            title: "Project Skill".into(),
            description: "Loaded from .rebon/skills.".into(),
            prompt_template: "Do project-specific work".into(),
            suggested_tools: Vec::new(),
            source: rebon_plugin_skill::SkillSource::Project,
            argument_hint: Some("<target>".into()),
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: true,
            disable_model_invocation: false,
            required_tools: Vec::new(),
        });
        registry.register(rebon_plugin_skill::Skill {
            id: "model-only".into(),
            title: "Model Only".into(),
            description: "Hidden from users.".into(),
            prompt_template: "Only the model may invoke this".into(),
            suggested_tools: Vec::new(),
            source: rebon_plugin_skill::SkillSource::Project,
            argument_hint: None,
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: false,
            disable_model_invocation: false,
            required_tools: Vec::new(),
        });

        registry.register(rebon_plugin_skill::Skill {
            id: "grill".into(),
            title: "Conflicting Grill Skill".into(),
            description: "Must not override the built-in command.".into(),
            prompt_template: "external grill prompt".into(),
            suggested_tools: Vec::new(),
            source: rebon_plugin_skill::SkillSource::Project,
            argument_hint: Some("<external>".into()),
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: true,
            disable_model_invocation: true,
            required_tools: Vec::new(),
        });

        register_registered_skill_slash_commands(&mut commands, &registry);

        let project_skill = commands
            .iter()
            .find(|command| command.name == "project-skill")
            .expect("registered user skill should appear in slash menu");
        assert_eq!(project_skill.description, "Loaded from .rebon/skills.");
        assert_eq!(
            project_skill
                .input
                .as_ref()
                .and_then(|input| input.hint.as_deref()),
            Some("<target>")
        );
        assert_eq!(
            project_skill.category,
            Some(rebon_types::SlashCommandCategory::Skill)
        );
        assert!(!commands.iter().any(|command| command.name == "model-only"));
        let grill_commands = commands
            .iter()
            .filter(|command| command.name == "grill")
            .collect::<Vec<_>>();
        assert_eq!(grill_commands.len(), 1);
        assert_eq!(
            grill_commands[0].category,
            Some(rebon_types::SlashCommandCategory::Command)
        );
        assert_ne!(
            grill_commands[0].description,
            "Must not override the built-in command."
        );
    }

    #[test]
    fn refreshing_skill_slash_commands_replaces_equal_count_swap() {
        let registry = rebon_plugin_skill::SkillRegistry::new();
        for id in ["skill-a", "skill-b"] {
            registry.register(rebon_plugin_skill::Skill {
                id: id.into(),
                title: id.into(),
                description: format!("Description for {id}"),
                prompt_template: String::new(),
                suggested_tools: Vec::new(),
                source: rebon_plugin_skill::SkillSource::Project,
                argument_hint: None,
                argument_names: Vec::new(),
                skill_root: None,
                user_invocable: true,
                disable_model_invocation: false,
                required_tools: Vec::new(),
            });
        }
        let mut commands = Vec::new();
        register_local_slash_commands(&mut commands);

        registry.set_disabled_skills(["skill-b"]);
        refresh_registered_skill_slash_commands(&mut commands, &registry);
        assert!(commands.iter().any(|command| command.name == "skill-a"));
        assert!(!commands.iter().any(|command| command.name == "skill-b"));

        // Enabled count stays one, but the identity changes. Refresh must not
        // leave the previously-enabled Skill category command behind.
        registry.set_disabled_skills(["skill-a"]);
        refresh_registered_skill_slash_commands(&mut commands, &registry);
        assert!(!commands.iter().any(|command| command.name == "skill-a"));
        assert!(commands.iter().any(|command| command.name == "skill-b"));
        assert_eq!(
            commands
                .iter()
                .filter(|command| {
                    command.category == Some(rebon_types::SlashCommandCategory::Skill)
                })
                .count(),
            1
        );
    }

    #[test]
    fn local_skills_command_uses_manage_description() {
        let mut commands = Vec::new();
        register_local_slash_commands(&mut commands);
        let skills = commands
            .iter()
            .find(|command| command.name == "skills")
            .expect("/skills command");
        assert_eq!(skills.description, "Manage available skills");
    }

    /// `/update` belongs to `plugins/updater` now, so it reaches the picker
    /// through the seat rather than through the built-in fallback table. The
    /// kernel boot is what installs that catalog; without it this asserts on
    /// whichever earlier test happened to boot one.
    #[test]
    fn register_local_slash_commands_includes_update() {
        rebon_harness::kernel_bootstrap::process_kernel();
        let mut commands = Vec::new();
        register_local_slash_commands(&mut commands);
        let update = commands
            .iter()
            .find(|command| command.name == "update")
            .expect("/update command registered");
        assert!(update.description.contains("update"));
        assert_eq!(
            update
                .input
                .as_ref()
                .and_then(|input| input.hint.as_deref()),
            Some("status|check|skip|channel <latest|stable>|auto <on|off|status>")
        );
    }

    /// `/workflows` is the tasks plugin's, so the same kernel boot applies:
    /// without it this test read the built-in fallback table and passed only
    /// when a sibling test had booted the kernel first.
    #[test]
    fn register_local_slash_commands_includes_context_and_compact() {
        rebon_harness::kernel_bootstrap::process_kernel();
        let mut commands = Vec::new();
        register_local_slash_commands(&mut commands);
        assert!(
            commands.iter().any(|c| c.name == "context"),
            "expected /context in registered slash commands"
        );
        assert!(
            commands.iter().any(|c| c.name == "workflows"),
            "expected /workflows in registered slash commands"
        );
        let compact = commands
            .iter()
            .find(|c| c.name == "compact")
            .expect("compact command should be registered");
        assert_eq!(
            compact.input.as_ref().and_then(|i| i.hint.as_deref()),
            Some("[instructions]")
        );
        let ultraplan = commands
            .iter()
            .find(|c| c.name == "ultraplan")
            .expect("ultraplan command should be registered");
        assert_eq!(
            ultraplan.input.as_ref().and_then(|i| i.hint.as_deref()),
            Some("[--grill] [--file <plan.md>] <prompt>")
        );
        assert!(
            ultraplan.description.contains("Multi-agent plan mode"),
            "description: {}",
            ultraplan.description
        );
        let grill = commands
            .iter()
            .find(|c| c.name == "grill")
            .expect("grill command should be registered");
        assert_eq!(
            grill.input.as_ref().and_then(|i| i.hint.as_deref()),
            Some("[--file <plan.md>] <prompt>")
        );
        assert_eq!(
            grill.category,
            Some(rebon_types::SlashCommandCategory::Command)
        );
        let ceo = commands
            .iter()
            .find(|c| c.name == "ceo")
            .expect("ceo command should be registered");
        assert_eq!(
            ceo.input.as_ref().and_then(|i| i.hint.as_deref()),
            Some("[on|off|task]")
        );
        assert!(ceo.description.contains("CEO/coordinator"));
        let ultrawork = commands
            .iter()
            .find(|c| c.name == "ultrawork")
            .expect("ultrawork command should be registered");
        assert_eq!(ultrawork.aliases, vec!["ulw".to_string()]);
        assert_eq!(
            ultrawork.input.as_ref().and_then(|i| i.hint.as_deref()),
            Some("<request>")
        );
        assert!(ultrawork.description.contains("workflow orchestration"));
    }

    #[test]
    fn register_local_slash_commands_includes_status() {
        let mut commands = Vec::new();
        register_local_slash_commands(&mut commands);

        let status = commands
            .iter()
            .find(|command| command.matches_name_or_alias("status"))
            .expect("/status command registered");
        assert_eq!(
            status.description,
            "Show session, model, and project status"
        );
    }

    fn restore_env_var(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn register_local_slash_commands_describes_protected_provider_setup() {
        let mut commands = Vec::new();
        register_local_slash_commands(&mut commands);
        let provider = commands
            .iter()
            .find(|command| command.name == "provider")
            .expect("/provider command registered");
        assert!(provider.description.contains("protected credential form"));
        assert_eq!(
            provider
                .input
                .as_ref()
                .and_then(|input| input.hint.as_deref()),
            Some("add <preset> <apiKey>|add|remove|list|use|add-model")
        );
    }

    #[test]
    fn register_local_slash_commands_includes_plugin_hint() {
        let mut commands = Vec::new();
        register_local_slash_commands(&mut commands);
        let plugin = commands
            .iter()
            .find(|command| command.name == "plugin")
            .expect("/plugin command registered");
        assert!(plugin.description.contains("plugins"));
        assert_eq!(
            plugin
                .input
                .as_ref()
                .and_then(|input| input.hint.as_deref()),
            Some("install <path|name|rust-lsp> [--scope user|project]")
        );
        assert!(parse_plugin_command("/plugin install rust-lsp").is_some());
        assert!(parse_plugin_command("/plugin:install rust-lsp").is_some());
        assert!(parse_plugin_command("/pluginx").is_none());
    }

    // ── parse_help_command ──────────────────────────────────────

    #[test]
    fn parse_help_command_matches_exact_with_trailing_whitespace() {
        assert!(parse_help_command("/help"));
        assert!(parse_help_command("/help  "));
    }

    #[test]
    fn parse_help_command_rejects_arguments_and_prefix_collisions() {
        assert!(!parse_help_command("/help me"));
        assert!(!parse_help_command("/help:me"));
        assert!(!parse_help_command("/helpfoo"));
        assert!(!parse_help_command("help"));
    }

    /// One rule for every parser in this file, because they used to each have
    /// their own. `/Compact` was prompt text while `/statusLine` was a command,
    /// since case was only handled where someone had thought to spell the
    /// variant out.
    #[test]
    fn kernel_command_parses_its_own_name_only() {
        assert_eq!(parse_kernel_command("/kernel"), Some(""));
        assert_eq!(parse_kernel_command("/kernel dsh"), Some("dsh"));
        assert_eq!(parse_kernel_command("/kernel:dsh"), Some("dsh"));
        assert_eq!(parse_kernel_command("/kernel  list "), Some("list"));
        // Not this command:
        assert_eq!(parse_kernel_command("/kernels"), None);
        assert_eq!(parse_kernel_command("/kern"), None);
        assert_eq!(parse_kernel_command("kernel dsh"), None);
        assert_eq!(parse_kernel_command("/agent kernel:dsh"), None);
    }

    #[test]
    fn a_held_shift_key_does_not_change_what_a_command_is() {
        assert!(parse_compact_command("/Compact").is_some());
        assert!(parse_compact_command("/COMPACT keep the test output").is_some());
        assert!(super::super::rewind::parse_rewind_command("/Rewind"));
        assert!(super::super::rewind::parse_rewind_command("/CheckPoint"));
        assert!(parse_prune_command("/Prune stats").is_some());
        assert!(parse_model_command("/Model gpt-5").is_some());
        assert!(parse_tasks_command("/Tasks").is_some());
        assert!(parse_review_command("/Review").is_some());
        assert!(parse_kernel_command("/Kernel dsh").is_some());
    }

    /// Trailing space is a keystroke on the way to pressing enter. A *leading*
    /// space is how you talk about a command instead of running one, and every
    /// parser here has always relied on that escape hatch — including the ones
    /// that had briefly stopped, when they were routed through a parser that
    /// trimmed both ends.
    #[test]
    fn a_leading_space_is_prompt_text_and_a_trailing_one_is_not() {
        assert!(parse_compact_command("/compact ").is_some());
        assert!(parse_help_command("/help "));
        assert!(parse_status_command("/status  "));

        assert!(parse_compact_command(" /compact").is_none());
        assert!(!parse_help_command(" /help"));
        assert!(!parse_status_command("\t/status"));
        assert!(parse_tasks_command(" /tasks").is_none());
    }

    /// The catalog lists other spellings for a handful of commands, and the
    /// picker shows them. Each one has to reach the same parser, or it is a
    /// name the menu offers and the dispatcher declines.
    #[test]
    fn catalog_aliases_reach_the_same_parser() {
        assert!(parse_exit_command("/quit"));
        assert!(parse_hosted_command("/host"));
        assert!(parse_plugin_command("/plugins").is_some());
        assert!(super::super::rewind::parse_rewind_command("/checkpoint"));
        assert!(parse_settings_dialog_command("/config").is_some());
        // The longest spelling wins, so `/hosted` is not `/host` plus "ed".
        assert!(parse_hosted_command("/hosted"));
    }

    /// `/bg` opens the task view, the way it always has on the desktop.
    /// `/background` still moves the session. The two used to be one command
    /// here and a different one there, and the picker described the desktop's.
    #[test]
    fn bg_shows_the_task_view_and_background_still_moves_the_session() {
        assert!(parse_tasks_command("/bg").is_some());
        assert_eq!(
            parse_tasks_command("/bg shell-123")
                .expect("expected match")
                .initial_detail_task_id
                .as_deref(),
            Some("shell-123")
        );
        assert!(
            parse_background_session_command("/bg").is_none(),
            "/bg must not silently move the session any more"
        );

        assert_eq!(parse_background_session_command("/background"), Some(None));
        assert_eq!(
            parse_background_session_command("/background finish this turn"),
            Some(Some("finish this turn".to_string()))
        );
        assert!(parse_tasks_command("/background").is_none());
        // `/background-agents` opens the Agent View, which is a third overlay.
        assert!(parse_background_session_command("/background-agents").is_none());
        assert!(parse_agent_view_command("/background-agents"));
        assert!(parse_tasks_command("/background-agents").is_none());
    }

    #[test]
    fn user_new_session_marks_inline_startup_banner_pending_only_for_inline_mode() {
        let mut inline_app = AppState::default();
        inline_app.ui_mode = crate::ui_config::UiMode::Inline;
        let mut inline_session = make_test_tui_session();

        apply_user_new_session(&mut inline_app, &mut inline_session);

        assert!(inline_app.pending_inline_viewport_reset);
        assert!(inline_app.pending_inline_startup_banner);

        let mut screen_app = AppState::default();
        let mut screen_session = make_test_tui_session();

        apply_user_new_session(&mut screen_app, &mut screen_session);

        assert!(!screen_app.pending_inline_viewport_reset);
        assert!(!screen_app.pending_inline_startup_banner);
    }

    #[test]
    fn internal_new_session_resets_inline_viewport_without_startup_banner() {
        let mut app = AppState::default();
        app.ui_mode = crate::ui_config::UiMode::Inline;
        let mut session = make_test_tui_session();

        apply_new_session(&mut app, &mut session);

        assert!(app.pending_inline_viewport_reset);
        assert!(!app.pending_inline_startup_banner);
    }

    #[test]
    fn apply_new_session_clears_previous_session_title() {
        let mut app = AppState::default();
        app.session_title = Some("Previous session".into());
        let mut session = make_test_tui_session();

        apply_new_session(&mut app, &mut session);

        assert!(app.session_title.is_none());
    }

    /// `/new` in a hosted terminal takes the startup route: the session is
    /// named and given a worker first, and this terminal becomes its
    /// mirror from birth — no lock, no handover, no "Moving this session"
    /// line, the dot the only signal. The view is reset as any `/new`
    /// resets it.
    #[test]
    fn a_new_hosted_session_is_a_mirror_from_birth() {
        let _guard = crate::test_env::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path().join("jobs"));
        let mut app = AppState::default();
        app.session_title = Some("Previous session".into());
        super::super::inject_system_message(&mut app, "info", "the old conversation");
        let mut session = make_test_tui_session();
        let previous_session_id = session.session_id.clone();
        let store_for_host = store.clone();
        let cwd_seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let cwd_seen_for_host = cwd_seen.clone();

        let replaced =
            apply_new_hosted_session_with(&mut app, &mut session, true, move |runtime, cwd| {
                *cwd_seen_for_host.lock().unwrap() = Some(cwd.to_string());
                let session_id = rebon_types::new_session_id();
                let job = rebon_session_host::queue_session_for_worker(
                    &store_for_host,
                    &session_id,
                    std::path::PathBuf::from(cwd),
                    runtime,
                    None,
                    rebon_session_host::JobPlacement::Foreground,
                )?;
                Ok(super::super::HostedStartup {
                    session_id,
                    job_id: job.identity.job_id,
                })
            });

        assert!(replaced);
        assert_eq!(cwd_seen.lock().unwrap().as_deref(), Some("."));
        let job_id = session
            .attached_background_job_id
            .clone()
            .expect("the new session is the job's");
        let job = store.read_state(&job_id).unwrap();
        assert_ne!(session.session_id, previous_session_id);
        assert_eq!(
            job.identity.session_id.as_deref(),
            Some(session.session_id.as_str())
        );
        assert!(job.identity.resume_only);
        assert!(matches!(
            session
                .pending_hosted_session
                .as_ref()
                .map(|pending| pending.kind),
            Some(crate::background::PendingHostedKind::Startup)
        ));
        assert!(session.session_active_lock.is_none(), "the worker takes it");
        assert!(
            session.engine_half.mcp.is_none(),
            "a mirror hosts no MCP servers"
        );
        assert!(app.session_title.is_none());
        assert!(
            app.rebon_tui.transcript.is_empty(),
            "reset like any /new; nothing said"
        );
        assert!(session
            .engine_half
            .handler
            .state()
            .get_session(&session.session_id)
            .is_some_and(|record| record.messages.is_empty()));
    }

    /// A worker that cannot be started leaves the session as it was and
    /// says so; the caller opens a local session instead.
    #[test]
    fn a_new_hosted_session_that_cannot_start_leaves_the_session_alone() {
        let mut app = AppState::default();
        let mut session = make_test_tui_session();
        let previous_session_id = session.session_id.clone();

        let replaced = apply_new_hosted_session_with(&mut app, &mut session, true, |_, _| {
            Err(anyhow::anyhow!("no exe"))
        });

        assert!(!replaced);
        assert_eq!(session.session_id, previous_session_id);
        assert!(session.attached_background_job_id.is_none());
        assert!(session.pending_hosted_session.is_none());
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| {
            matches!(row, rebon_tui::Message::System(message)
                if message.content.as_deref().is_some_and(|c| c.contains("Could not start a background worker")))
        }));
    }

    #[test]
    fn apply_new_session_detaches_previous_task_list() {
        let _guard = crate::test_env::lock_env();
        let _env = EnvRestore::new(&[
            "REBON_CONFIG_DIR",
            "REBON_TASK_LIST_ID",
            "REBON_TEAM_NAME",
            "REBON_SESSION_ID",
        ]);
        let temp = TempDir::new().unwrap();
        std::env::set_var("REBON_CONFIG_DIR", temp.path());
        std::env::remove_var("REBON_TASK_LIST_ID");
        rebon_tool::set_current_team_name("previous-session-team");
        rebon_tool::tasks::create_task(
            "previous-session-team",
            rebon_tool::tasks::NewTask {
                subject: "previous task".into(),
                status: rebon_tool::tasks::TaskListStatus::InProgress,
                ..Default::default()
            },
        )
        .unwrap();

        let mut app = AppState::default();
        app.task_completion_timestamps
            .push(crate::tui::app::TaskCompletionEntry {
                id: "1".into(),
                completed_at_ms: 1,
            });
        app.prev_task_snapshot.push((
            "1".into(),
            rebon_tui::promptinput::TaskListStatus::InProgress,
        ));
        app.task_hide_deadline_ms = Some(1);
        app.task_list_collapsed = true;
        app.task_list_prev_count = 1;
        let mut session = make_test_tui_session();

        apply_new_session(&mut app, &mut session);

        assert!(rebon_tool::current_team_name().is_none());
        assert_eq!(
            rebon_tool::tasks::current_task_list_id(),
            session.session_id
        );
        assert!(rebon_tool::tasks::list_tasks(&session.session_id)
            .unwrap()
            .is_empty());
        assert_eq!(
            rebon_tool::tasks::list_tasks("previous-session-team")
                .unwrap()
                .len(),
            1
        );
        assert!(app.task_completion_timestamps.is_empty());
        assert!(app.prev_task_snapshot.is_empty());
        assert!(app.task_hide_deadline_ms.is_none());
        assert!(!app.task_list_collapsed);
        assert_eq!(app.task_list_prev_count, 0);
    }

    #[test]
    fn apply_new_session_forces_custom_status_line_refresh() {
        let mut app = AppState::default();
        app.custom_status_line =
            crate::tui::runner::custom_status_line::CustomStatusLineState::configured(Some(
                crate::ui_config::StatusLineConfig {
                    kind: crate::ui_config::StatusLineKind::Command,
                    command: "status".into(),
                    script: None,
                    padding: None,
                    refresh_interval: Some(30),
                    hide_vim_mode_indicator: None,
                },
            ));
        app.custom_status_line.output = vec!["old session".into()];
        app.custom_status_line.sequence = 7;
        app.custom_status_line.in_flight = true;
        app.custom_status_line.last_started_at = Some(std::time::Instant::now());
        app.custom_status_line.last_completed_at = Some(std::time::Instant::now());
        let mut session = make_test_tui_session();

        apply_new_session(&mut app, &mut session);

        assert!(app.custom_status_line.config.is_some());
        assert_eq!(app.custom_status_line.output, vec!["old session"]);
        assert_eq!(app.custom_status_line.sequence, 8);
        assert!(!app.custom_status_line.in_flight);
        assert!(app.custom_status_line.last_started_at.is_none());
        assert!(app.custom_status_line.last_completed_at.is_none());
        assert!(app.custom_status_line.last_result().is_none());
    }

    #[test]
    fn apply_new_session_resets_status_line_usage_inputs() {
        let mut app = AppState::default();
        app.usage_mut()
            .reset(wall_clock_ms().saturating_sub(30_000));
        app.streaming_token_count = 123;
        app.prompt_completion_status = Some(crate::tui::app::PromptCompletionStatus::Succeeded);
        app.usage_mut().add_turn(
            "provider",
            "model",
            rebon_types::Usage {
                input_tokens: 50_000,
                output_tokens: 1_000,
                prompt_cache_hit_tokens: 40_000,
                prompt_cache_miss_tokens: 10_000,
                ..Default::default()
            },
        );
        let mut session = make_test_tui_session();
        session.model.prune_level.report_usage(126_000);

        apply_new_session(&mut app, &mut session);

        assert_eq!(app.streaming_token_count, 0);
        assert_eq!(app.prompt_completion_status, None);
        let usage = app.usage();
        assert_eq!(usage.last_turn, rebon_types::Usage::default());
        assert_eq!(usage.total, rebon_types::Usage::default());
        assert!(usage.by_model.is_empty());
        assert!(wall_clock_ms().saturating_sub(usage.started_at_ms) < 5_000);
        let usage = session.model.prune_level.budget.usage_snapshot();
        assert_eq!(usage.tokens, 0);
        assert_eq!(usage.source, rebon_api::ContextUsageSource::Unknown);
    }

    #[test]
    fn apply_new_session_clears_active_goal() {
        let mut app = AppState::default();
        app.goal = Some(crate::goal::GoalState::new_now("ship the feature"));
        app.deferred_goal_submit_payloads
            .push(crate::session::submit_payload::SubmitPayload {
                text: "goal queued".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            });
        app.deferred_internal_submit_payloads
            .push(crate::session::submit_payload::SubmitPayload {
                text: "internal queued".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            });
        let mut session = make_test_tui_session();

        apply_new_session(&mut app, &mut session);

        assert!(app.goal.is_none());
        assert!(app.deferred_goal_submit_payloads.is_empty());
        assert!(app.deferred_internal_submit_payloads.is_empty());
    }

    #[test]
    fn parse_goal_command_matches_show_clear_and_set_forms() {
        assert!(matches!(
            parse_goal_command("/goal"),
            Some(GoalCommand::Show)
        ));
        assert!(matches!(
            parse_goal_command("/goal status"),
            Some(GoalCommand::Show)
        ));
        assert!(matches!(
            parse_goal_command("/goal clear"),
            Some(GoalCommand::Clear)
        ));
        assert!(matches!(
            parse_goal_command("/goal off"),
            Some(GoalCommand::Off)
        ));
        assert!(matches!(
            parse_goal_command("/goal stop"),
            Some(GoalCommand::Stop)
        ));
        assert!(matches!(
            parse_goal_command("/goal archive"),
            Some(GoalCommand::Archive)
        ));
        assert_eq!(
            parse_goal_command("/goal --max-sessions 3 finish the migration"),
            Some(GoalCommand::Set {
                prompt: "finish the migration".into(),
                max_sessions: Some(3),
            })
        );
        assert_eq!(
            parse_goal_command("/goal:ship it"),
            Some(GoalCommand::Set {
                prompt: "ship it".into(),
                max_sessions: None,
            })
        );
    }

    #[test]
    fn execute_goal_command_sets_simple_feedback() {
        let mut app = AppState::default();
        let outcome = execute_goal_command(
            &mut app,
            GoalCommand::Set {
                prompt: "ship it".into(),
                max_sessions: Some(3),
            },
        );

        assert_eq!(
            outcome,
            GoalCommandOutcome::Feedback("goal set: ship it".into())
        );
        let goal = app.goal.expect("goal set");
        assert_eq!(goal.prompt, "ship it");
        assert_eq!(goal.max_sessions, Some(3));
        assert!(goal.started_at_ms > 0);
    }

    #[test]
    fn execute_goal_command_complete_goal_requires_replace_confirmation() {
        let mut app = AppState::default();
        let mut old_goal = crate::goal::GoalState::new_with_started_at("old", 10);
        old_goal.mark_complete(Some("done".into()));
        app.goal = Some(old_goal);

        let outcome = execute_goal_command(
            &mut app,
            GoalCommand::Set {
                prompt: "new".into(),
                max_sessions: Some(2),
            },
        );

        assert_eq!(
            outcome,
            GoalCommandOutcome::ConfirmReplace {
                prompt: "new".into(),
                max_sessions: Some(2)
            }
        );
        assert_eq!(app.goal.as_ref().unwrap().prompt, "old");
    }

    #[test]
    fn execute_goal_command_controls_goal_lifecycle() {
        let mut app = AppState::default();
        app.goal = Some(crate::goal::GoalState::new_with_started_at("ship it", 10));
        app.deferred_goal_submit_payloads
            .push(crate::session::submit_payload::SubmitPayload {
                text: "continue".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            });
        app.deferred_internal_submit_payloads
            .push(crate::session::submit_payload::SubmitPayload {
                text: "internal".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            });

        let outcome = execute_goal_command(&mut app, GoalCommand::Stop);
        assert_eq!(outcome, GoalCommandOutcome::Feedback("goal paused".into()));
        assert!(app.goal.as_ref().is_some_and(|goal| goal.is_paused()));

        let outcome = execute_goal_command(&mut app, GoalCommand::Show);
        assert_eq!(
            outcome,
            GoalCommandOutcome::Feedback("goal paused (1 session): ship it".into())
        );

        let outcome = execute_goal_command(&mut app, GoalCommand::Clear);
        assert_eq!(outcome, GoalCommandOutcome::Feedback("goal cleared".into()));
        assert!(app.goal.is_none());
        assert!(app.deferred_goal_submit_payloads.is_empty());
        assert_eq!(app.deferred_internal_submit_payloads.len(), 1);

        app.goal = Some(crate::goal::GoalState::new_with_started_at("ship it", 10));
        let outcome = execute_goal_command(&mut app, GoalCommand::Off);
        assert_eq!(
            outcome,
            GoalCommandOutcome::Feedback("goal turned off".into())
        );
        assert!(app.goal.is_none());
        assert!(app.deferred_goal_submit_payloads.is_empty());
        assert_eq!(app.deferred_internal_submit_payloads.len(), 1);

        app.goal = Some(crate::goal::GoalState::new_with_started_at("ship it", 10));
        app.deferred_goal_submit_payloads
            .push(crate::session::submit_payload::SubmitPayload {
                text: "continue".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            });
        let outcome = execute_goal_command(&mut app, GoalCommand::Archive);
        assert_eq!(
            outcome,
            GoalCommandOutcome::Feedback(
                "goal is not complete yet; use /goal stop to pause or /goal clear to remove it"
                    .into()
            )
        );
        assert!(app.goal.as_ref().is_some_and(|goal| goal.is_active()));
        assert_eq!(app.deferred_goal_submit_payloads.len(), 1);
        assert_eq!(app.deferred_internal_submit_payloads.len(), 1);

        let goal = app.goal.as_mut().expect("goal preserved");
        goal.mark_complete(Some("done".into()));
        let outcome = execute_goal_command(&mut app, GoalCommand::Archive);
        assert_eq!(
            outcome,
            GoalCommandOutcome::Feedback("goal archived".into())
        );
        assert!(app.goal.as_ref().is_some_and(|goal| goal.is_archived()));
        assert!(app.goal.as_ref().unwrap().archived_at_ms.is_some());
        assert!(app.deferred_goal_submit_payloads.is_empty());
        assert_eq!(app.deferred_internal_submit_payloads.len(), 1);
    }

    #[test]
    fn parse_goal_command_rejects_prefix_collision() {
        assert!(parse_goal_command("/goalpost").is_none());
        assert!(parse_goal_command("goal").is_none());
    }

    // ── parse_tasks_command ─────────────────────────────────────

    #[test]
    fn parse_tasks_command_matches_bare_slash_tasks() {
        let cmd = parse_tasks_command("/tasks").expect("expected match");
        assert!(cmd.initial_detail_task_id.is_none());
    }

    #[test]
    fn parse_tasks_command_captures_explicit_id_space_separated() {
        let cmd = parse_tasks_command("/tasks shell-123").expect("expected match");
        assert_eq!(cmd.initial_detail_task_id.as_deref(), Some("shell-123"));
    }

    #[test]
    fn parse_tasks_command_captures_explicit_id_colon_separated() {
        let cmd = parse_tasks_command("/tasks:shell-123").expect("expected match");
        assert_eq!(cmd.initial_detail_task_id.as_deref(), Some("shell-123"));
    }

    #[test]
    fn parse_tasks_command_rejects_non_matches() {
        assert!(parse_tasks_command("/tasksfoo").is_none());
        assert!(parse_tasks_command("tasks").is_none());
        assert!(parse_tasks_command("prompt text").is_none());
    }

    #[test]
    fn parse_workflows_command_matches_bare_and_explicit_id_forms() {
        let bare = parse_workflows_command("/workflows").expect("expected bare /workflows");
        assert!(bare.initial_detail_task_id.is_none());

        let spaced =
            parse_workflows_command("/workflows local_workflow-1").expect("expected workflow id");
        assert_eq!(
            spaced.initial_detail_task_id.as_deref(),
            Some("local_workflow-1")
        );

        let colon = parse_workflows_command("/workflows:local_workflow-1")
            .expect("expected colon workflow id");
        assert_eq!(
            colon.initial_detail_task_id.as_deref(),
            Some("local_workflow-1")
        );
    }

    #[test]
    fn parse_workflows_command_rejects_non_matches() {
        assert!(parse_workflows_command("/workflowsfoo").is_none());
        assert!(parse_workflows_command("workflows").is_none());
    }

    #[test]
    fn parse_teams_command_matches_bare_and_named_forms() {
        let bare = parse_teams_command("/teams").expect("expected bare /teams");
        assert!(bare.initial_team_name.is_none());

        let named = parse_teams_command("/teams research").expect("expected named /teams");
        assert_eq!(named.initial_team_name.as_deref(), Some("research"));

        let colon = parse_teams_command("/teams:research").expect("expected colon /teams");
        assert_eq!(colon.initial_team_name.as_deref(), Some("research"));
    }

    #[test]
    fn parse_teams_command_rejects_prefix_collision() {
        assert!(parse_teams_command("/teamsfoo").is_none());
        assert!(parse_teams_command("teams").is_none());
    }

    #[test]
    fn parse_agents_command_matches_bare_and_named_forms() {
        let bare = parse_agents_command("/agents").expect("expected bare /agents");
        assert!(bare.initial_agent_type.is_none());

        let named = parse_agents_command("/agents reviewer").expect("expected named /agents");
        assert_eq!(named.initial_agent_type.as_deref(), Some("reviewer"));

        let colon = parse_agents_command("/agents:reviewer").expect("expected colon /agents");
        assert_eq!(colon.initial_agent_type.as_deref(), Some("reviewer"));
    }

    #[test]
    fn parse_agents_command_rejects_prefix_collision() {
        assert!(parse_agents_command("/agentsfoo").is_none());
        assert!(parse_agents_command("agents").is_none());
    }

    #[test]
    fn parse_settings_dialog_command_matches_config_and_settings_alias() {
        assert_eq!(
            parse_settings_dialog_command("/config"),
            Some(SettingsDialogCommand {
                default_tab: TabId::Config,
            })
        );
        assert_eq!(
            parse_settings_dialog_command("/settings"),
            Some(SettingsDialogCommand {
                default_tab: TabId::Config,
            })
        );
    }

    #[test]
    fn parse_settings_dialog_command_accepts_explicit_tab_name() {
        assert_eq!(
            parse_settings_dialog_command("/settings status"),
            Some(SettingsDialogCommand {
                default_tab: TabId::Status,
            })
        );
        assert_eq!(
            parse_settings_dialog_command("/config:usage"),
            Some(SettingsDialogCommand {
                default_tab: TabId::Usage,
            })
        );
    }

    #[test]
    fn parse_settings_dialog_command_rejects_unknown_suffixes() {
        assert!(parse_settings_dialog_command("/settingsfoo").is_none());
        assert!(parse_settings_dialog_command("/settings advanced").is_none());
        assert!(parse_settings_dialog_command("settings").is_none());
    }

    // ── parse_run_command ───────────────────────────────────────

    #[test]
    fn parse_run_command_captures_command() {
        assert_eq!(parse_run_command("/run ls -la").as_deref(), Some("ls -la"));
        assert_eq!(
            parse_run_command("/run cargo build --release").as_deref(),
            Some("cargo build --release")
        );
    }

    #[test]
    fn parse_run_command_bare_slash_run_returns_empty() {
        // "/run" alone matches and yields an empty command so the
        // caller can decide whether to treat as no-op.
        assert_eq!(parse_run_command("/run").as_deref(), Some(""));
    }

    #[test]
    fn parse_run_command_rejects_prefix_collision() {
        assert!(parse_run_command("/runfoo").is_none());
        assert!(parse_run_command("/runner").is_none());
        assert!(parse_run_command("run ls").is_none());
    }

    #[test]
    fn parse_run_command_preserves_trailing_whitespace_in_arg() {
        // After the required leading space, internal whitespace is
        // left alone so the shell sees the exact intended string.
        assert_eq!(
            parse_run_command("/run  foo bar  ").as_deref(),
            Some("foo bar  ")
        );
    }

    // ── parse_agent_command ─────────────────────────────────────

    #[test]
    fn parse_agent_command_captures_prompt() {
        assert_eq!(
            parse_agent_command("/agent investigate the auth bug").as_deref(),
            Some("investigate the auth bug"),
        );
    }

    #[test]
    fn parse_agent_command_rejects_prefix_collision() {
        assert!(parse_agent_command("/agentfoo").is_none());
        assert!(parse_agent_command("/agents list").is_none());
    }

    #[test]
    fn parse_agent_command_bare_slash_agent_returns_empty() {
        assert_eq!(parse_agent_command("/agent").as_deref(), Some(""));
    }

    // ── parse_effort_command ──────────────────────────────────────

    #[test]
    fn parse_effort_bare_returns_show() {
        assert!(matches!(
            parse_effort_command("/effort"),
            Some(EffortCommand::Show)
        ));
    }

    #[test]
    fn parse_effort_trailing_space_returns_show() {
        assert!(matches!(
            parse_effort_command("/effort "),
            Some(EffortCommand::Show)
        ));
    }

    #[test]
    fn parse_effort_rejects_prefix_collision() {
        assert!(parse_effort_command("/effortxhigh").is_none());
        assert!(parse_effort_command("/efforting").is_none());
    }

    #[test]
    fn parse_effort_low() {
        let Some(EffortCommand::Set(level)) = parse_effort_command("/effort low") else {
            panic!("expected Set(Low)");
        };
        assert_eq!(level, rebon_types::ReasoningEffort::Low);
    }

    #[test]
    fn parse_effort_medium() {
        let Some(EffortCommand::Set(level)) = parse_effort_command("/effort medium") else {
            panic!("expected Set(Medium)");
        };
        assert_eq!(level, rebon_types::ReasoningEffort::Medium);
    }

    #[test]
    fn parse_effort_med_alias() {
        let Some(EffortCommand::Set(level)) = parse_effort_command("/effort med") else {
            panic!("expected Set(Medium)");
        };
        assert_eq!(level, rebon_types::ReasoningEffort::Medium);
    }

    #[test]
    fn parse_effort_high() {
        let Some(EffortCommand::Set(level)) = parse_effort_command("/effort high") else {
            panic!("expected Set(High)");
        };
        assert_eq!(level, rebon_types::ReasoningEffort::High);
    }

    #[test]
    fn parse_effort_xhigh() {
        let Some(EffortCommand::Set(level)) = parse_effort_command("/effort xhigh") else {
            panic!("expected Set(Max)");
        };
        assert_eq!(level, rebon_types::ReasoningEffort::XHigh);
    }

    #[test]
    fn parse_effort_auto() {
        assert!(matches!(
            parse_effort_command("/effort auto"),
            Some(EffortCommand::Auto)
        ));
    }

    #[test]
    fn parse_effort_case_insensitive() {
        let Some(EffortCommand::Set(level)) = parse_effort_command("/effort XHIGH") else {
            panic!("expected Set(Max)");
        };
        assert_eq!(level, rebon_types::ReasoningEffort::XHigh);
    }

    #[test]
    fn parse_effort_invalid_arg() {
        assert!(matches!(
            parse_effort_command("/effort foo"),
            Some(EffortCommand::Invalid(_))
        ));
    }

    #[test]
    fn parse_fast_commands() {
        assert!(matches!(
            parse_fast_command("/fast"),
            Some(FastCommand::Status)
        ));
        assert!(matches!(
            parse_fast_command("/fast status"),
            Some(FastCommand::Status)
        ));
        assert!(matches!(
            parse_fast_command("/fast on"),
            Some(FastCommand::On)
        ));
        assert!(matches!(
            parse_fast_command("/fast off"),
            Some(FastCommand::Off)
        ));
        assert!(parse_fast_command("/fastest").is_none());
    }

    // ── execute_effort_command — live switching ───────────────────

    #[test]
    fn effort_starts_at_auto() {
        let app = AppState::default();
        assert_eq!(app.effort_level, None);
    }

    #[test]
    fn effort_set_xhigh_takes_effect_immediately() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::XHigh),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::XHigh));
    }

    #[test]
    fn effort_set_low_takes_effect_immediately() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::Low),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::Low));
    }

    #[test]
    fn effort_switch_between_levels_in_same_session() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();

        // low → high → xhigh → medium — each switch is immediate
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::Low),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::Low));

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::High),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::High));

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::XHigh),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::XHigh));

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::Medium),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::Medium));
    }

    #[test]
    fn effort_reset_to_auto_clears_level() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::XHigh),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::XHigh));

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Auto,
        );
        assert_eq!(app.effort_level, None);
    }

    #[test]
    fn effort_auto_then_set_again() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();

        // set → auto → set again
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::High),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::High));

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Auto,
        );
        assert_eq!(app.effort_level, None);

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::Low),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::Low));
    }

    #[test]
    fn effort_show_does_not_mutate_state() {
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
            EffortCommand::Show,
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::High));
    }

    #[test]
    fn effort_invalid_does_not_mutate_state() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::XHigh),
        );
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Invalid("nope".into()),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::XHigh));
    }

    #[test]
    fn effort_set_same_level_twice_is_idempotent() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();

        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::Medium),
        );
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::Medium),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::Medium));
    }

    #[test]
    fn effort_full_cycle_all_levels_and_back() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();

        for level in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
        ] {
            let _ = execute_effort_command(
                &mut app.effort_level,
                app.effort_provider_kind,
                EffortCommand::Set(level),
            );
            assert_eq!(app.effort_level, Some(level), "failed to set {:?}", level);
        }
        // Back to auto
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Auto,
        );
        assert_eq!(app.effort_level, None);
        // And back to low
        let _ = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::Low),
        );
        assert_eq!(app.effort_level, Some(ReasoningEffort::Low));
    }

    #[test]
    fn effort_loads_persisted_user_setting_on_startup() {
        use rebon_types::ReasoningEffort;
        let tempdir = TempDir::new().expect("tempdir");
        let _env_lock = crate::test_env::lock_env();
        crate::rebon_config::save_effort_level_in_dir(tempdir.path(), Some("medium"))
            .expect("persist effort");
        let old_config_dir = std::env::var_os("REBON_CONFIG_DIR");
        std::env::set_var("REBON_CONFIG_DIR", tempdir.path());

        let mut app = AppState::default();
        load_persisted_effort_level(&mut app.effort_level);

        restore_env_var("REBON_CONFIG_DIR", old_config_dir);
        assert_eq!(app.effort_level, Some(ReasoningEffort::Medium));
    }

    #[test]
    fn effort_set_persists_to_user_settings() {
        use rebon_types::ReasoningEffort;
        let tempdir = TempDir::new().expect("tempdir");
        let _env_lock = crate::test_env::lock_env();
        let old_config_dir = std::env::var_os("REBON_CONFIG_DIR");
        std::env::set_var("REBON_CONFIG_DIR", tempdir.path());
        let mut app = AppState::default();

        let _ = execute_persisted_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::XHigh),
        );

        restore_env_var("REBON_CONFIG_DIR", old_config_dir);
        assert_eq!(app.effort_level, Some(ReasoningEffort::XHigh));
        assert_eq!(
            crate::rebon_config::saved_effort_level_in_dir(tempdir.path()).as_deref(),
            Some("xhigh")
        );
    }

    #[test]
    fn effort_auto_clears_persisted_user_setting() {
        use rebon_types::ReasoningEffort;
        let tempdir = TempDir::new().expect("tempdir");
        let _env_lock = crate::test_env::lock_env();
        crate::rebon_config::save_effort_level_in_dir(tempdir.path(), Some("high"))
            .expect("seed effort");
        let old_config_dir = std::env::var_os("REBON_CONFIG_DIR");
        std::env::set_var("REBON_CONFIG_DIR", tempdir.path());
        let mut app = AppState::default();
        app.effort_level = Some(ReasoningEffort::High);

        let _ = execute_persisted_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Auto,
        );

        restore_env_var("REBON_CONFIG_DIR", old_config_dir);
        assert_eq!(app.effort_level, None);
        assert_eq!(
            crate::rebon_config::saved_effort_level_in_dir(tempdir.path()),
            None
        );
    }

    // ── effort message text — provider-aware labels ──────────────

    #[test]
    fn effort_message_anthropic_says_effort() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();
        app.effort_provider_kind = rebon_types::effort_indicator::EffortProviderKind::Anthropic;
        let msg = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::High),
        );
        assert!(msg.contains("effort"), "expected 'effort' in: {msg}");
        assert!(!msg.contains("thinking"), "unexpected 'thinking' in: {msg}");
    }

    #[test]
    fn effort_message_openai_says_thinking() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();
        app.effort_provider_kind = rebon_types::effort_indicator::EffortProviderKind::OpenAi;
        let msg = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Set(ReasoningEffort::High),
        );
        assert!(msg.contains("thinking"), "expected 'thinking' in: {msg}");
        assert!(!msg.contains("effort"), "unexpected 'effort' in: {msg}");
    }

    #[test]
    fn effort_show_message_anthropic() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();
        app.effort_provider_kind = rebon_types::effort_indicator::EffortProviderKind::Anthropic;
        app.effort_level = Some(ReasoningEffort::XHigh);
        let msg = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Show,
        );
        assert!(msg.contains("effort"), "expected 'effort' in: {msg}");
        assert!(msg.contains("xhigh"), "expected 'xhigh' in: {msg}");
    }

    #[test]
    fn effort_show_message_openai() {
        use rebon_types::ReasoningEffort;
        let mut app = AppState::default();
        app.effort_provider_kind = rebon_types::effort_indicator::EffortProviderKind::OpenAi;
        app.effort_level = Some(ReasoningEffort::XHigh);
        let msg = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Show,
        );
        assert!(msg.contains("thinking"), "expected 'thinking' in: {msg}");
        assert!(msg.contains("xhigh"), "expected 'xhigh' in: {msg}");
    }

    #[test]
    fn effort_auto_message_openai() {
        let mut app = AppState::default();
        app.effort_provider_kind = rebon_types::effort_indicator::EffortProviderKind::OpenAi;
        let msg = execute_effort_command(
            &mut app.effort_level,
            app.effort_provider_kind,
            EffortCommand::Auto,
        );
        assert!(msg.contains("thinking"), "expected 'thinking' in: {msg}");
    }

    // ── provider_kind_from_format ─────────────────────────────────
}

//   /permissions approve <id>   → mark as Approved
//   /permissions retry <id>     → mark as Retried (replay request)
//   /permissions clear          → drop every resolved record
//   /permissions clear all      → drop every record

#[cfg(test)]
mod new_session_command_tests {
    use super::super::test_support::make_test_tui_session;
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// The agent layer stays while the session changes, and every piece of
    /// it refuses work for a session it does not recognise. Left behind, an
    /// agent keeps running and quietly stops being recorded.
    #[test]
    fn a_new_session_moves_the_agent_layer_with_it() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects = TempDir::new().unwrap();
        session.projects_root = projects.path().to_path_buf();
        let before = session.engine_half.runtime.session_agents.session();

        apply_new_session(&mut app, &mut session);

        assert_ne!(before, session.engine_half.runtime.session_agents.session());
        assert_eq!(
            session.engine_half.runtime.session_agents.session(),
            session.session_id
        );
    }

    /// A detached prompt owns the runtime Arc it started with. `/new` installs
    /// a coherent replacement without retargeting any of the old turn's
    /// session metadata or open file-history boundary.
    #[test]
    fn a_new_session_preserves_the_runtime_captured_by_an_old_turn() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects = TempDir::new().unwrap();
        session.projects_root = projects.path().to_path_buf();
        let old_session_id = session.session_id.clone();
        let old_cwd = projects.path().join("old").display().to_string();
        session
            .swap_runtime(&old_session_id, &old_cwd, false)
            .unwrap();
        let old = session.engine_half.runtime.clone();
        old.file_history_tracker.set_current_message_id("old-turn");
        let old_hook = old.policy.context().clone();
        let old_binding = {
            let binding = old.skill_state.lock().unwrap();
            let (session_id, cwd) = binding.session_binding();
            (session_id.to_string(), cwd.to_string())
        };

        apply_new_session(&mut app, &mut session);

        let new = session.engine_half.runtime.clone();
        assert!(!Arc::ptr_eq(&old, &new));
        assert_ne!(old.session_id, new.session_id);
        assert_eq!(old.session_id, old_hook.session_id);
        assert_eq!(old.cwd, old_hook.cwd);
        assert_eq!(old_binding.0, old.session_id);
        assert_eq!(old_binding.1, old.cwd);
        assert_eq!(
            rebon_agent_core::file_history::FileHistoryTracker::is_armed(&old.file_history_tracker),
            Some(true),
            "the old turn boundary must remain open until that turn closes it"
        );
        assert_eq!(
            rebon_agent_core::file_history::FileHistoryTracker::is_armed(&new.file_history_tracker),
            Some(false),
            "the new session must not inherit the old turn boundary"
        );
        assert!(!Arc::ptr_eq(&old.session_agents, &new.session_agents));
        assert_ne!(
            old.policy.context().session_id,
            new.policy.context().session_id,
            "the new session's policy handle names the new session"
        );
        assert!(!Arc::ptr_eq(&old.skill_state, &new.skill_state));
        assert!(!Arc::ptr_eq(&old.mid_turn_queue, &new.mid_turn_queue));
        assert!(!Arc::ptr_eq(&old.permission_broker, &new.permission_broker));
        let new_hook = new.policy.context().clone();
        assert_eq!(new_hook.session_id, new.session_id);
        assert_eq!(new_hook.cwd, new.cwd);
        let new_binding = new.skill_state.lock().unwrap();
        assert_eq!(
            new_binding.session_binding(),
            (new.session_id.as_str(), new.cwd.as_str())
        );
    }

    /// `/new` is the way off a session parked on a stopped worker, so it
    /// has to actually produce a session that accepts input again.
    #[test]
    fn a_new_session_leaves_a_parked_session_behind() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects = TempDir::new().unwrap();
        session.projects_root = projects.path().to_path_buf();
        session.attached_background_job_id = Some("bg-gone".into());
        session.remote_background_attachment = Some(
            crate::background::RemoteBackgroundAttachment::without_worker(
                "bg-gone".into(),
                session.session_id.clone(),
                session.cwd.clone(),
                crate::background::BackgroundJobStatus::Stopped,
                0,
            ),
        );
        session.pending_hosted_session = Some(crate::background::PendingHostedSession::handover(
            "bg-gone".into(),
        ));

        apply_new_session(&mut app, &mut session);

        assert!(session.attached_background_job_id.is_none());
        assert!(session.remote_background_attachment.is_none());
        assert!(session.pending_hosted_session.is_none());
        assert!(session.session_active_lock.is_some());
    }
}
