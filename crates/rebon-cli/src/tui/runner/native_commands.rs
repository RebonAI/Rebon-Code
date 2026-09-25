//! The terminal's implementations of the built-in slash commands.
//!
//! Every
//! built-in the terminal can run is registered on the `command-registry` seat
//! as [`CommandHandler::Native(name)`], and this module is where that name is
//! turned back into code: [`native_dispatch`] matches the id the seat carried
//! and runs the one branch that belongs to it.
//!
//! It replaces a fifty-branch `if parse_x_command(text) { … }` chain that ran
//! at the top of `submit_or_queue_with_images_and_uuid`. The chain decided
//! *which* command a line was by asking every parser in turn; the seat has
//! already decided that by the time we get here, so each branch only parses
//! its own arguments. A branch that cannot parse them returns `None`, which
//! means exactly what falling off the end of the chain used to mean: the line
//! goes on as prompt text.
//!
//! `/exit` and `/stop` are not here. They are `Native` on the seat like the
//! rest, but the submit path has to run them before the stale-projection gate
//! and before session-control forwarding, so they stay at the top of
//! `submit.rs`. [`NATIVE_COMMAND_IDS`] and the interlock test below say so.

use rebon_plugin_onboarding::OnboardingDialogState;
use rebon_tui::input::VimMode;
use tokio::runtime::Handle;

use crate::session::submit_payload::SubmitPayload;
use crate::tui::app::AppState;
use crate::tui::dispatch::{apply_submit, clear_input, enqueue_submit_payload};
use crate::tui::mcp_dialog::McpDialogState;
use crate::tui::ui_registry::open as open_dialog;
use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::UiMode;
use rebon_dialog::settings_tabs::TabId;
use rebon_ui_seat::{ids, input as dialog_input, DialogArgs};

use super::commands::{
    apply_user_new_session, execute_goal_command, execute_statusline_command,
    parse_account_command, parse_agent_command, parse_agent_view_command, parse_agents_command,
    parse_background_session_command, parse_context_command, parse_cost_command,
    parse_doctor_command, parse_effort_command, parse_fast_command, parse_goal_command,
    parse_help_command, parse_memory_command, parse_migrate_command, parse_new_or_clear_command,
    parse_onboarding_command, parse_profile_command, parse_review_command, parse_run_command,
    parse_settings_dialog_command, parse_skills_command, parse_status_command,
    parse_statusline_command, parse_tasks_command, parse_teams_command, parse_theme_command,
    parse_vim_command, parse_workflows_command, synthesize_review_prompt, GoalCommand,
    GoalCommandOutcome,
};
use super::compact_runtime::{start_manual_compact, CompactStart};
use super::layout_and_scroll::repin_transcript_to_bottom;
use super::onboarding_hooks::fire_onboarding_opened;
use super::profile_command;
use super::prompt_history::save_to_history_for_session_if_needed;
use super::prompt_lifecycle::{
    admit_active_prompt, admit_permission_replay_turn, live_input_withdrawable_submit_from_payload,
    prepare_for_new_prompt_after_withdrawal,
};
use super::rewind::{open_rewind_dialog, parse_resume_command, parse_rewind_command};
use super::runtime_refresh::refresh_runtime_model;
use super::session_detach_attach::background_current_session;
use super::submit::{
    current_session_is_blank, execute_ultraplan_manual_review_command, format_ultraplan_run_list,
    parse_ultrawork_workflow_command, prepare_goal_clarification_if_needed,
    prepare_ultraplan_resume_submit, prepare_ultrawork_workflow_submit,
    refuse_local_turn_if_session_is_elsewhere, session_engine_lives_elsewhere, set_goal_and_start,
    submit_model_text,
};
use super::task_runtime::{spawn_agent_task, spawn_shell_task};
use super::transcript_messages::{
    inject_local_command_feedback, inject_local_command_feedback_with_command,
    inject_system_message,
};
use super::ultraplan::{enter_coordinator_mode, exit_coordinator_mode, prepare_ultraplan_submit};
use super::updater_ui::handle_update_command;
use super::{ActivePrompt, LocalTurnSource};
use crate::session::commands::ceo::{parse_ceo_command, CeoCommand};
use crate::session::commands::context::{
    execute_compact_command, execute_context_command, execute_memory_command,
    execute_prune_command, parse_compact_command, parse_prune_command,
};
use crate::session::commands::effort::{
    execute_fast_command, execute_persisted_effort_command, EffortCommand,
};
use crate::session::commands::hooks::{execute_hooks_command, parse_hooks_command};
use crate::session::commands::mcp::{execute_mcp_command, parse_mcp_command, McpCommand};
use crate::session::commands::model::{handle_model_command, parse_model_command};
use crate::session::commands::permissions::{
    execute_permissions_command, parse_permissions_command,
};
use crate::session::commands::plugin::{handle_plugin_command, parse_plugin_command};
use crate::session::commands::provider::{
    execute_provider_reconnect, handle_provider_command, parse_provider_command,
    ProviderCommandResult,
};
use crate::session::commands::status::execute_status_command;
use crate::session::commands::ultraplan_prompt::{
    parse_ultraplan_command, ultraplan_usage, UltraplanCommand,
};
use crate::session_shell::session_command_inputs_from_app;

/// Every catalog name [`native_dispatch`] has an arm for, plus the two the
/// submit path runs ahead of it. The interlock test below compares this
/// against the seat's `Native` commands on the `Tui` surface, in both
/// directions: a name here the seat does not offer is a branch nobody can
/// reach, and a name the seat offers without a branch reaches the model as
/// prompt text.
#[cfg(test)]
const NATIVE_COMMAND_IDS: &[&str] = &[
    "agent",
    "agents",
    "backend",
    "background",
    "background-agents",
    "ceo",
    "clear",
    "codemode",
    "compact",
    "context",
    "cost",
    "doctor",
    "effort",
    "exit",
    "fast",
    "goal",
    "grill",
    "help",
    "hooks",
    "hosted",
    "kernel",
    "login",
    "logout",
    "mcp",
    "memory",
    "migrate",
    "model",
    "new",
    "onboarding",
    "permissions",
    "plugin",
    "profile",
    "provider",
    "prune",
    "resume",
    "review",
    "rewind",
    "run",
    "sandbox",
    "settings",
    "skills",
    "status",
    "statusline",
    "stop",
    "tasks",
    "teams",
    "theme",
    "ultraplan",
    "ultrawork",
    "update",
    "vim",
    "workflows",
];

/// The two ids the submit path runs before this dispatcher, and why.
#[cfg(test)]
const HANDLED_BEFORE_DISPATCH: &[&str] = &["exit", "stop"];

/// A spelling the terminal has always answered to that the seat does not
/// carry.
///
/// `/agent-view` predates the catalog and `background-agents` never listed it
/// as an alias, so dispatching on the seat's token alone would have turned it
/// into prompt text. Recognized here rather than added to the catalog: making
/// it an alias would also put it in the `/` picker, which is a product
/// decision and not this refactor's to make.
pub(super) fn unlisted_command_alias(token: &str) -> Option<&'static str> {
    token
        .eq_ignore_ascii_case("agent-view")
        .then_some("background-agents")
}

/// Everything a branch may reach for, gathered once so each one can be a
/// function with a one-line signature.
///
/// Moved rather than borrowed: exactly one branch runs, so the branch that
/// runs takes the whole thing.
pub(super) struct Cx<'a> {
    /// The composer text. `/review` rewrites it and lets the line go on as a
    /// prompt; nothing else touches it.
    text: &'a mut String,
    app: &'a mut AppState,
    session: &'a mut TuiEngineSession,
    handle: &'a Handle,
    active_prompt: &'a mut Option<ActivePrompt>,
    ui_mode: UiMode,
    cursor_offset: usize,
    transcript_len: usize,
}

impl<'a> Cx<'a> {
    /// Hand every borrow to the branch that took it, ordered by how often a
    /// branch wants it, so each one opens with a single `let (…) = cx.split()`
    /// naming the prefix it uses and dropping the rest with `..`.
    #[allow(clippy::type_complexity)]
    fn split(
        self,
    ) -> (
        &'a mut AppState,
        &'a mut TuiEngineSession,
        &'a Handle,
        &'a mut Option<ActivePrompt>,
        UiMode,
        usize,
        usize,
        &'a mut String,
    ) {
        (
            self.app,
            self.session,
            self.handle,
            self.active_prompt,
            self.ui_mode,
            self.cursor_offset,
            self.transcript_len,
            self.text,
        )
    }
}

/// Run the built-in the `command-registry` seat resolved this line to.
///
/// `None` means the id has no branch here, or the branch could not parse its
/// arguments — either way the line goes on as prompt text, which is what
/// falling off the end of the old chain did. `Some(quit)` is the submit
/// path's return value.
#[allow(clippy::too_many_arguments)]
pub(super) fn native_dispatch(
    id: &str,
    text: &mut String,
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    ui_mode: UiMode,
    cursor_offset: usize,
    transcript_len: usize,
) -> Option<bool> {
    let command = text.trim_end().to_string();
    let cx = Cx {
        text,
        app,
        session,
        handle,
        active_prompt,
        ui_mode,
        cursor_offset,
        transcript_len,
    };
    let command = command.as_str();
    match id {
        "help" => run_help(cx, command),
        "new" | "clear" => run_new_or_clear(cx, command),
        "hosted" => run_hosted(cx, command),
        "vim" => run_vim(cx, command),
        "skills" => run_skills(cx, command),
        "memory" => run_memory(cx, command),
        "goal" => run_goal(cx, command),
        "mcp" => run_mcp(cx, command),
        "hooks" => run_hooks(cx, command),
        "status" => run_status(cx, command),
        "statusline" => run_statusline(cx, command),
        "codemode" => run_codemode(cx, command),
        "cost" => run_cost(cx, command),
        "doctor" => run_doctor(cx, command),
        "review" => run_review(cx, command),
        "background" => run_background(cx, command),
        "background-agents" => run_agent_view(cx, command),
        "tasks" => run_tasks(cx, command),
        "workflows" => run_workflows(cx, command),
        "teams" => run_teams(cx, command),
        "agents" => run_agents(cx, command),
        "sandbox" => run_sandbox(cx, command),
        "settings" => run_settings(cx, command),
        "onboarding" => run_onboarding(cx, command),
        "theme" => run_theme(cx, command),
        "migrate" => run_migrate(cx, command),
        "login" => run_login(cx, command),
        "logout" => run_logout(cx, command),
        "resume" => run_resume(cx, command),
        "rewind" => run_rewind(cx, command),
        "run" => run_run(cx, command),
        "agent" => run_agent(cx, command),
        "context" => run_context(cx, command),
        "permissions" => run_permissions(cx, command),
        "prune" => run_prune(cx, command),
        "compact" => run_compact(cx, command),
        "effort" => run_effort(cx, command),
        "fast" => run_fast(cx, command),
        "update" => run_update(cx, command),
        "ultraplan" | "grill" => run_ultraplan(cx, command),
        "ultrawork" => run_ultrawork(cx, command),
        "ceo" => run_ceo(cx, command),
        "kernel" => run_kernel(cx, command),
        "backend" => run_backend_switch(cx, command, "/backend"),
        "profile" => run_profile(cx, command),
        "provider" => run_provider(cx, command),
        "model" => run_model(cx, command),
        "plugin" => run_plugin(cx, command),
        _ => None,
    }
}

// ------------------------------------------------------------------ shared

/// What a local command does with the line it took: remember it in the prompt
/// history, then clear the composer. Every branch below does both, in this
/// order.
fn accept(app: &mut AppState, session: &TuiEngineSession, command: &str) {
    save_to_history_for_session_if_needed(app, session, command);
    clear_input(app);
}

/// A local command's output, in the transcript, with the view following it.
fn feedback(app: &mut AppState, name: &str, output: &str) {
    inject_local_command_feedback(app, name, output);
    app.follow_transcript_tail = true;
}

/// A `*CommandResult`'s text, as an error or as a local-command receipt.
fn command_result(app: &mut AppState, is_err: bool, text: &str) {
    inject_system_message(app, if is_err { "error" } else { "local_command" }, text);
    app.follow_transcript_tail = true;
}

/// Start — or, mid-turn, queue — a payload a slash command built.
fn start_command_turn(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    submit: SubmitPayload,
    cursor_offset: usize,
    transcript_len: usize,
) {
    repin_transcript_to_bottom(app);
    if active_prompt.is_some() {
        app.is_loading = true;
        enqueue_submit_payload(app, submit);
        return;
    }
    let withdrawable = live_input_withdrawable_submit_from_payload(
        &submit,
        cursor_offset,
        transcript_len,
        app.rebon_tui.transcript.len(),
    );
    if let Some(admitted) = admit_active_prompt(
        app,
        session,
        handle,
        active_prompt.as_ref(),
        LocalTurnSource::Command,
        submit,
    ) {
        *active_prompt = Some(admitted.with_withdrawable(withdrawable));
        app.is_loading = true;
    }
}

// ------------------------------------------------------------- local UI

/// Intercept `/help` locally so slash-picker completion and manual
/// submission open the help overlay instead of spawning an engine turn.
fn run_help(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_help_command(command).then_some(())?;
    let (app, session, ..) = cx.split();
    app.help_open = true;
    app.help_tab_index = 0;
    accept(app, session, command);
    Some(false)
}

/// Intercept `/new` and `/clear` locally: wipe transcript, reset session
/// state, regenerate session ID unless the current session is still blank.
/// Background tasks are preserved.
fn run_new_or_clear(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_new_or_clear_command(command).then_some(())?;
    let (app, session, _, active_prompt, ..) = cx.split();
    if active_prompt.is_some() {
        accept(app, session, command);
        inject_system_message(
            app,
            "error",
            "Cannot start a new session while a prompt is running.",
        );
        app.follow_transcript_tail = true;
        return Some(false);
    }
    accept(app, session, command);
    if !current_session_is_blank(app, session) {
        // The terminal keeps its mode across `/new`: a session that ran
        // in a worker is followed by one that does — started in a worker
        // and mirrored from birth, the way startup does it (RFC-0004
        // §9), not built here and handed over — and a `--local` session
        // by another local one. A worker that could not be started
        // falls back to a local session, and says so.
        let was_hosted = session.remote_background_attachment.is_some()
            || session.pending_hosted_session.is_some();
        let hosted_now = was_hosted
            && !session.terminal_startup.local
            && super::commands::apply_new_hosted_session(app, session, true);
        if !hosted_now {
            apply_user_new_session(app, session);
        }
    }
    Some(false)
}

/// `/hosted` moves the engine that owns this session into a worker process.
/// It is a local control action, not a session-control command, so the
/// forwarding gate in `submit.rs` leaves it alone; once a session is already
/// hosted the handler is what says so.
fn run_hosted(cx: Cx<'_>, command: &str) -> Option<bool> {
    super::commands::parse_hosted_command(command).then_some(())?;
    let (app, session, _, active_prompt, ..) = cx.split();
    accept(app, session, command);
    super::session_detach_attach::host_current_session_in_worker(app, session, active_prompt);
    Some(false)
}

/// Intercept `/vim` locally so toggling editor mode never starts a model turn.
fn run_vim(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_vim_command(command).then_some(())?;
    let (app, session, ..) = cx.split();
    accept(app, session, command);
    let output = if app.vim_mode.is_some() {
        app.vim_mode = None;
        "Vim mode disabled.".to_string()
    } else {
        let enabled_mode = app.initial_vim_mode.unwrap_or(VimMode::Insert);
        app.vim_mode = Some(enabled_mode);
        format!("Vim mode enabled ({}).", enabled_mode.as_label())
    };
    feedback(app, "vim", &output);
    Some(false)
}

/// A bare `/skills` is a local UI action: clear the prompt and open the
/// multi-selector without committing a transcript row or starting/queuing a
/// model turn. An actually empty registry gets a small local notice.
fn run_skills(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_skills_command(command).then_some(())?;
    let (app, session, ..) = cx.split();
    accept(app, session, command);
    app.slash_picker = None;
    app.at_mention_picker = None;
    if let Some(dialog) = open_dialog(
        ids::dialog::SKILLS,
        DialogArgs::values([skills_input(session.engine_half.skill_registry.as_ref())]),
    ) {
        app.dialogs.push_boxed(dialog);
    } else {
        feedback(
            app,
            "skills",
            "No skills are currently loaded for this session.",
        );
    }
    Some(false)
}

fn run_memory(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_memory_command(command).then_some(())?;
    let (app, session, _, _, ui_mode, ..) = cx.split();
    accept(app, session, command);
    if ui_mode == UiMode::Inline {
        let output = execute_memory_command(session);
        feedback(app, "memory", &output);
        app.dialogs.close_if(ids::dialog::MEMORY);
    } else {
        push_dialog(
            app,
            ids::dialog::MEMORY,
            DialogArgs::values([session.cwd.as_str()]),
        );
    }
    Some(false)
}

fn run_goal(cx: Cx<'_>, command: &str) -> Option<bool> {
    let cmd = parse_goal_command(command)?;
    let (app, session, handle, active_prompt, ..) = cx.split();
    save_to_history_for_session_if_needed(app, session, command);
    match cmd {
        GoalCommand::Set {
            prompt,
            max_sessions,
        } if !app
            .goal
            .as_ref()
            .is_some_and(|goal| goal.is_complete() || goal.is_archived()) =>
        {
            if refuse_local_turn_if_session_is_elsewhere(app, session, "/goal") {
                clear_input(app);
                return Some(false);
            }
            if prepare_goal_clarification_if_needed(app, command, &prompt, max_sessions) {
                clear_input(app);
            } else {
                set_goal_and_start(
                    app,
                    command,
                    prompt,
                    max_sessions,
                    session,
                    handle,
                    active_prompt,
                );
            }
        }
        cmd => match execute_goal_command(app, cmd) {
            GoalCommandOutcome::Feedback(output) => {
                inject_local_command_feedback_with_command(app, "goal", command, &output);
                clear_input(app);
                app.follow_transcript_tail = true;
            }
            GoalCommandOutcome::ConfirmReplace {
                prompt,
                max_sessions,
            } => {
                clear_input(app);
                app.goal_confirm_dialog = Some(
                    crate::tui::goal_confirm_dialog::GoalConfirmDialogState::new(
                        prompt,
                        max_sessions,
                    ),
                );
            }
        },
    }
    Some(false)
}

fn run_mcp(cx: Cx<'_>, command: &str) -> Option<bool> {
    let parsed = parse_mcp_command(command)?;
    let (app, session, _, _, ui_mode, ..) = cx.split();
    accept(app, session, command);
    match parsed {
        // A malformed subcommand is still an `/mcp`: it gets the usage line
        // rather than being sent to the model as a prompt.
        Err(usage) => feedback(app, "mcp", &usage),
        // The dialog is the status view. Asking to reconnect or disconnect
        // is an action, and it answers in the transcript either way.
        Ok(McpCommand::Status) if ui_mode != UiMode::Inline => {
            app.mcp_dialog = Some(McpDialogState::open(session));
        }
        Ok(subcommand) => {
            let output = execute_mcp_command(session, subcommand);
            feedback(app, "mcp", &output);
        }
    }
    Some(false)
}

fn run_hooks(cx: Cx<'_>, command: &str) -> Option<bool> {
    let cmd = parse_hooks_command(command)?;
    let (app, session, _, _, ui_mode, ..) = cx.split();
    accept(app, session, command);
    let unknown_event = cmd
        .event_name
        .as_deref()
        .is_some_and(|name| rebon_hooks::parse_hook_event(name).is_none());
    if ui_mode == UiMode::Inline || unknown_event {
        let output = execute_hooks_command(session, cmd);
        feedback(app, "hooks", &output);
    } else {
        push_dialog(
            app,
            ids::dialog::HOOKS,
            DialogArgs::payload(crate::session::commands::hooks::collect_hooks_dialog_input(
                session,
                cmd.event_name.as_deref(),
            )),
        );
    }
    Some(false)
}

fn run_status(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_status_command(command).then_some(())?;
    let (app, session, _, _, ui_mode, ..) = cx.split();
    accept(app, session, command);
    if ui_mode == UiMode::Inline {
        let inputs = session_command_inputs_from_app(app, session.ui_mode);
        let output = execute_status_command(&inputs, session);
        feedback(app, "status", &output);
        app.dialogs.close_if(ids::dialog::SETTINGS);
    } else {
        let initial = parse_settings_dialog_command("/config status")
            .expect("the built-in status settings route should parse");
        open_settings_dialog(app, session, initial.default_tab);
    }
    Some(false)
}

fn run_statusline(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_statusline_command(command).then_some(())?;
    let (app, session, ..) = cx.split();
    accept(app, session, command);
    let output = execute_statusline_command(app);
    feedback(app, "statusline", &output);
    Some(false)
}

fn run_codemode(cx: Cx<'_>, command: &str) -> Option<bool> {
    use crate::session::commands::control::{
        execute_session_control_command, parse_session_control_command,
    };
    let (name, args) = parse_session_control_command(command)?;
    let (app, session, ..) = cx.split();
    accept(app, session, command);
    let inputs = session_command_inputs_from_app(app, session.ui_mode);
    match execute_session_control_command(&inputs, session, &name, &args) {
        Ok(result) => feedback(app, "codemode", &result.output.text),
        Err(error) => command_result(app, true, &error),
    }
    Some(false)
}

fn run_cost(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_cost_command(command).then_some(())?;
    let (app, session, ..) = cx.split();
    accept(app, session, command);
    let inputs = session_command_inputs_from_app(app, session.ui_mode);
    let output = crate::session::commands::cost::execute_cost_command(session, &inputs);
    feedback(app, "cost", &output);
    Some(false)
}

fn run_doctor(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_doctor_command(command).then_some(())?;
    let (app, session, _, _, ui_mode, ..) = cx.split();
    accept(app, session, command);
    if ui_mode == UiMode::Inline {
        let inputs = session_command_inputs_from_app(app, session.ui_mode);
        let output = crate::session::commands::doctor::execute_doctor_command(&inputs, session);
        feedback(app, "doctor", &output);
        app.dialogs.close_if(ids::dialog::DOCTOR);
    } else {
        let inputs = session_command_inputs_from_app(app, session.ui_mode);
        let report = crate::session::commands::doctor::collect_doctor_report(&inputs, session);
        push_dialog(app, ids::dialog::DOCTOR, DialogArgs::payload(report));
    }
    Some(false)
}

fn run_review(cx: Cx<'_>, command: &str) -> Option<bool> {
    let review_cmd = parse_review_command(command)?;
    let (app, session, handle, active_prompt, _, cursor_offset, transcript_len, text) = cx.split();
    save_to_history_for_session_if_needed(app, session, command);
    match synthesize_review_prompt(&review_cmd) {
        Ok(prompt) => {
            tracing::info!("rebon-cli: transforming /review into reviewer prompt");
            // `/review` is a model turn wearing a slash command, and a
            // session whose engine lives in a worker does not own its
            // transcript here. Running it locally would put a second
            // writer on the file that worker is writing — so once the
            // prompt is synthesized it takes the ordinary route and the
            // ownership gates in `submit.rs` decide where the turn runs.
            if session_engine_lives_elsewhere(session) {
                *text = prompt;
                None
            } else {
                submit_model_text(
                    app,
                    &prompt,
                    None,
                    session,
                    handle,
                    active_prompt,
                    cursor_offset,
                    transcript_len,
                );
                Some(false)
            }
        }
        Err(err) => {
            clear_input(app);
            inject_system_message(app, "error", &err);
            app.follow_transcript_tail = true;
            Some(false)
        }
    }
}

// -------------------------------------------------------------- overlays

fn run_background(cx: Cx<'_>, command: &str) -> Option<bool> {
    let final_prompt = parse_background_session_command(command)?;
    let (app, session, _, active_prompt, ..) = cx.split();
    accept(app, session, command);
    background_current_session(app, session, final_prompt, active_prompt);
    Some(false)
}

fn run_agent_view(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_agent_view_command(command).then_some(())?;
    let (app, session, ..) = cx.split();
    super::agent_view::open_agent_view(app, session.attached_background_job_id.as_deref());
    accept(app, session, command);
    Some(false)
}

/// Intercept the `/tasks` slash command locally: open the tasks overlay
/// without spawning an engine turn.
fn run_tasks(cx: Cx<'_>, command: &str) -> Option<bool> {
    let initial = parse_tasks_command(command)?;
    let (app, session, ..) = cx.split();
    let snapshots = app.task_snapshots();
    app.background_tasks_dialog = crate::tui::ui_registry::open_background_tasks(
        rebon_plugin_tasks::ui::background_tasks_dialog::BackgroundTasksDialogOpen {
            snapshots,
            initial_detail_task_id: initial.initial_detail_task_id,
            foregrounded_task_id: app.foregrounded_task_id.clone(),
            kind_filter: None,
        },
    );
    accept(app, session, command);
    Some(false)
}

fn run_workflows(cx: Cx<'_>, command: &str) -> Option<bool> {
    let initial = parse_workflows_command(command)?;
    let (app, session, ..) = cx.split();
    let snapshots = app.task_snapshots();
    app.background_tasks_dialog = crate::tui::ui_registry::open_background_tasks(
        rebon_plugin_tasks::ui::background_tasks_dialog::BackgroundTasksDialogOpen {
            snapshots,
            initial_detail_task_id: initial.initial_detail_task_id,
            foregrounded_task_id: None,
            kind_filter: Some(rebon_plugin_tasks::runtime::TaskKind::LocalWorkflow),
        },
    );
    accept(app, session, command);
    Some(false)
}

fn run_teams(cx: Cx<'_>, command: &str) -> Option<bool> {
    let initial = parse_teams_command(command)?;
    let (app, session, ..) = cx.split();
    let snapshots = app.task_snapshots();
    app.teams_dialog = rebon_plugin_tasks::ui::teams_dialog::TeamsDialogState::open(
        &snapshots,
        initial.initial_team_name.as_deref(),
    );
    accept(app, session, command);
    Some(false)
}

fn run_agents(cx: Cx<'_>, command: &str) -> Option<bool> {
    let initial = parse_agents_command(command)?;
    let (app, session, ..) = cx.split();
    // The Model step offers every declared ACP agent as an
    // `<id>:` spec next to the built-in tiers.
    let external_model_options = session
        .engine_half
        .runtime
        .session_agents
        .choices()
        .into_iter()
        .filter(|choice| choice.origin.is_some())
        .map(
            |choice| rebon_plugin_agents::surface::model_selector::ModelOption {
                value: format!("{}:", choice.id),
                label: format!("{} (ACP)", choice.label),
                description: format!(
                    "Runs the whole task on the external `{}` agent; append a model after \
                 ':' as a best-effort hint",
                    choice.id
                ),
            },
        )
        .collect();
    // Through the seat, so that turning the `agents` plugin off takes
    // the panel with the tool instead of leaving a menu that edits
    // definitions nothing will run.
    push_dialog(
        app,
        ids::dialog::AGENTS,
        DialogArgs::payload(rebon_plugin_agents::dialog::AgentsDialogInput {
            cwd: session.cwd.clone(),
            tool_names: session.engine_half.engine.tool_names(),
            initial_agent_type: initial.initial_agent_type.clone(),
            external_model_options,
        }),
    );
    accept(app, session, command);
    Some(false)
}

/// Intercept `/sandbox`: the sandbox plugin's panel, opened for this
/// session's workspace because the settings chain it reads is that
/// workspace's. Nothing to parse — the seat already decided the line is this
/// command, and the panel takes no arguments.
fn run_sandbox(cx: Cx<'_>, command: &str) -> Option<bool> {
    let (app, session, ..) = cx.split();
    let args = DialogArgs::values([session.cwd.as_str()]);
    accept(app, session, command);
    push_dialog(app, ids::dialog::SANDBOX, args);
    Some(false)
}

/// Intercept `/config` and `/settings` locally so the user gets the settings
/// surface immediately instead of sending the slash command through the model.
fn run_settings(cx: Cx<'_>, command: &str) -> Option<bool> {
    let initial = parse_settings_dialog_command(command)?;
    let (app, session, ..) = cx.split();
    open_settings_dialog(app, session, initial.default_tab);
    accept(app, session, command);
    Some(false)
}

/// Intercept `/onboarding`: re-run the onboarding wizard as a full-screen
/// dialog overlay.
fn run_onboarding(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_onboarding_command(command).then_some(())?;
    let (app, session, handle, ..) = cx.split();
    app.onboarding_dialog = Some(OnboardingDialogState::open_for_command_with_ui_mode(
        session.ui_mode,
    ));
    open_onboarding_dialog(app, session, handle, "slash_onboarding");
    accept(app, session, command);
    Some(false)
}

/// Intercept `/theme`: open a single-step theme picker as a full screen
/// overlay.
fn run_theme(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_theme_command(command).then_some(())?;
    let (app, session, handle, ..) = cx.split();
    app.onboarding_dialog = Some(OnboardingDialogState::open_for_theme_command());
    open_onboarding_dialog(app, session, handle, "slash_theme");
    accept(app, session, command);
    Some(false)
}

/// Intercept `/migrate`: open the import step on its own, without walking (or
/// completing) the whole setup wizard.
fn run_migrate(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_migrate_command(command).then_some(())?;
    let (app, session, handle, ..) = cx.split();
    app.onboarding_dialog = Some(OnboardingDialogState::open_for_migrate_command());
    open_onboarding_dialog(app, session, handle, "slash_migrate");
    accept(app, session, command);
    Some(false)
}

/// Intercept `/login [account]`: open the login pane, which lists every
/// account login. Naming one opens the pane with that account selected, one
/// Enter away — the sign-in itself needs the pane's key loop to drive it.
fn run_login(cx: Cx<'_>, command: &str) -> Option<bool> {
    let account = parse_account_command(command, "login")?;
    let (app, session, handle, ..) = cx.split();
    let mut dialog = OnboardingDialogState::open_for_login_pane();
    if let Some(name) = account.as_deref() {
        match rebon_plugin_onboarding::accounts::find_login(name) {
            Ok(spec) => {
                dialog.focus_login(spec.id);
            }
            Err(message) => {
                accept(app, session, command);
                command_result(app, true, &message);
                return Some(false);
            }
        }
    }
    app.onboarding_dialog = Some(dialog);
    open_onboarding_dialog(app, session, handle, "slash_login");
    accept(app, session, command);
    Some(false)
}

/// Intercept `/logout [account]`: sign out of the account named, or of the
/// only one signed in. The session running now keeps the credential it
/// already resolved, so when that is the login just removed, say so rather
/// than let the next turn look like the sign-out did nothing.
fn run_logout(cx: Cx<'_>, command: &str) -> Option<bool> {
    let account = parse_account_command(command, "logout")?;
    let (app, session, ..) = cx.split();
    accept(app, session, command);
    let config_dir = crate::rebon_config::config_home_dir();
    match rebon_plugin_onboarding::accounts::logout(&config_dir, account.as_deref()) {
        Ok(report) => {
            let still_held = report.signed_out.is_some_and(|spec| {
                session
                    .model
                    .provider_name
                    .eq_ignore_ascii_case(spec.provider.name)
            });
            let text = if still_held {
                format!(
                    "{} This session keeps the credential it already holds until it ends.",
                    report.message
                )
            } else {
                report.message
            };
            feedback(app, "logout", &text);
        }
        Err(message) => command_result(app, true, &message),
    }
    Some(false)
}

/// Tell the hooks an onboarding overlay opened, if one did.
fn open_onboarding_dialog(
    app: &AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    reason: &str,
) {
    if let Some(dialog) = app.onboarding_dialog.as_ref() {
        fire_onboarding_opened(session, handle, dialog, reason);
    }
}

/// Intercept the `/resume` slash command. Opens the resume dialog which lists
/// on-disk sessions for the current cwd.
fn run_resume(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_resume_command(command).then_some(())?;
    let (app, session, _, active_prompt, ..) = cx.split();
    if active_prompt.is_some() {
        accept(app, session, command);
        inject_system_message(
            app,
            "error",
            "Cannot resume another session while a prompt is running.",
        );
        app.follow_transcript_tail = true;
        return Some(false);
    }
    app.resume_dialog = Some(crate::tui::resume_dialog::ResumeDialogState::open());
    accept(app, session, command);
    Some(false)
}

fn run_rewind(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_rewind_command(command).then_some(())?;
    let (app, session, _, active_prompt, ..) = cx.split();
    if active_prompt.is_some() {
        accept(app, session, command);
        inject_system_message(
            app,
            "error",
            "Cannot rewind or reload this session while a prompt is running.",
        );
        app.follow_transcript_tail = true;
        return Some(false);
    }
    // An agent that writes to disk itself leaves no pre-write
    // snapshot, so rewind can only cover what it routed back
    // through Rebon. Say so before the picker implies otherwise.
    if !session
        .engine_half
        .runtime
        .session_agents
        .capabilities()
        .supports_rewind()
    {
        inject_system_message(
            app,
            "local_command",
            "This session is running on an external agent. Rebon only snapshotted the \
             writes that agent routed through it, so rewind restores the conversation \
             but may not restore every code change. Use /agent local to go back to \
             rebon's engine.",
        );
    }
    app.rewind_dialog = Some(open_rewind_dialog(app, session));
    accept(app, session, command);
    Some(false)
}

// ------------------------------------------------------------ local work

/// Intercept the `/run <cmd>` slash command. Spawn a shell task directly into
/// the coordinator registry and let the tasks dialog surface it.
fn run_run(cx: Cx<'_>, command: &str) -> Option<bool> {
    let cmd = parse_run_command(command)?;
    let (app, session, ..) = cx.split();
    if refuse_local_turn_if_session_is_elsewhere(app, session, "/run") {
        accept(app, session, command);
        return Some(false);
    }
    if !cmd.is_empty() {
        match spawn_shell_task(app, session, &cmd) {
            Ok(task_id) => {
                tracing::info!(task_id = %task_id, command = %cmd, "rebon-cli: /run spawned shell task");
            }
            Err(err) => {
                tracing::warn!(error = %err, "rebon-cli: /run spawn failed");
            }
        }
    }
    accept(app, session, command);
    Some(false)
}

/// Intercept `/agent <prompt>`: hand the prompt to the coordinator's
/// sub-agent spawner with the session's default model.
///
/// `/agent` carries two meanings — spawn a sub-agent, and switch which agent
/// runs this session. The spawn side used to take every input and return
/// unconditionally, so the switch was unreachable: `/agent list` spawned a
/// sub-agent whose prompt was the word "list". Anything the switch
/// understands falls through to it; everything else is a prompt. The test for
/// that lives beside the switch, because a line taken here never reaches it: a
/// subcommand it fails to recognise is one nobody can run.
fn run_agent(cx: Cx<'_>, command: &str) -> Option<bool> {
    let spawn = parse_agent_command(command).filter(|args| {
        !rebon_agent_core::routing::agent_args_name_the_switch(
            &cx.session.engine_half.runtime.session_agents,
            args,
        )
    });
    let Some(agent_prompt) = spawn else {
        return run_backend_switch(cx, command, "/agent");
    };
    let (app, session, handle, ..) = cx.split();
    if refuse_local_turn_if_session_is_elsewhere(app, session, "/agent") {
        accept(app, session, command);
        return Some(false);
    }
    match spawn_agent_task(app, session, handle, &agent_prompt) {
        Ok(task_id) => {
            tracing::info!(task_id = %task_id, "rebon-cli: /agent spawned sub-agent task");
        }
        Err(err) => {
            tracing::warn!(error = %err, "rebon-cli: /agent spawn failed");
        }
    }
    accept(app, session, command);
    Some(false)
}

// --------------------------------------------------------- session state

/// Intercept `/context`: show context window usage and message breakdown.
/// Open the settings panel with its first projection already in hand,
/// so the very first frame paints live values rather than an empty
/// shell. Every later frame refreshes it from the render entry.
fn open_settings_dialog(app: &mut AppState, session: &TuiEngineSession, default_tab: TabId) {
    use crate::session::settings_rows;

    let view = super::render::build_settings_dialog_view(app, session);
    let active_tab = settings_rows::tab_index(default_tab);
    let args = DialogArgs::payload(rebon_dialog::settings_dialog::SettingsDialogOpen {
        active_tab,
        projection: settings_rows::projection(&view, active_tab),
    });
    push_dialog(app, ids::dialog::SETTINGS, args);
}

/// Bucket the transcript into the context browser's five categories.
///
/// The browser's reducer lives in `rebon-dialog`, which cannot see a
/// transcript, so this projection stays here and travels as data.
fn context_input(content: &str, messages: &[rebon_tui::Message]) -> String {
    let mut buckets: Vec<Vec<String>> = vec![Vec::new(); 5];
    for (index, message) in messages.iter().enumerate() {
        let number = index + 1;
        match message {
            rebon_tui::Message::User(user) => {
                for block in &user.message.content {
                    match block {
                        rebon_tui::UserContentBlock::Text(text) => {
                            buckets[0].push(format!("#{number} User: {}", preview(&text.text)))
                        }
                        rebon_tui::UserContentBlock::ToolResult(result) => buckets[2].push(
                            format!("#{number} {}", preview(&result.content.as_display_string())),
                        ),
                        rebon_tui::UserContentBlock::Image(_) => {
                            buckets[0].push(format!("#{number} User image"))
                        }
                    }
                }
            }
            rebon_tui::Message::Assistant(assistant) => {
                for block in &assistant.message.content {
                    match block {
                        rebon_tui::AssistantContentBlock::Text(text) => {
                            buckets[0].push(format!("#{number} Assistant: {}", preview(&text.text)))
                        }
                        rebon_tui::AssistantContentBlock::ToolUse(tool) => {
                            buckets[1].push(format!("#{number} {}", tool.name))
                        }
                        rebon_tui::AssistantContentBlock::Thinking(thinking) => {
                            buckets[3].push(format!("#{number} {}", preview(&thinking.thinking)))
                        }
                        rebon_tui::AssistantContentBlock::RedactedThinking(_) => {
                            buckets[3].push(format!("#{number} Redacted thinking"))
                        }
                        rebon_tui::AssistantContentBlock::GeneratedImage(_) => {
                            buckets[0].push(format!("#{number} Assistant image"))
                        }
                        rebon_tui::AssistantContentBlock::Other => {}
                    }
                }
            }
            rebon_tui::Message::System(system) => buckets[0].push(format!(
                "#{number} System: {}",
                preview(system.content.as_deref().unwrap_or(&system.subtype))
            )),
            rebon_tui::Message::Attachment(_) => buckets[0].push(format!("#{number} Attachment")),
            rebon_tui::Message::Unknown => {}
        }
    }
    dialog_input::encode(&dialog_input::ContextDialogInput {
        content: content.to_string(),
        category_items: buckets,
    })
}

fn preview(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = compact.chars();
    let preview: String = chars.by_ref().take(72).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else if preview.is_empty() {
        "(empty)".into()
    } else {
        preview
    }
}

/// Project the loaded skill index into the selector's input. The index
/// is the front end's; the panel that edits it is the skill plugin's.
fn skills_input(registry: &rebon_plugin_skill::SkillRegistry) -> String {
    let disabled = registry.disabled_skills();
    dialog_input::encode(&dialog_input::SkillsDialogInput {
        skills: registry
            .all_entries()
            .into_iter()
            .map(|skill| dialog_input::SkillRowInput {
                id: skill.id,
                source: skill_source_label(skill.source).to_string(),
                description: skill.description,
            })
            .collect(),
        disabled: disabled.into_iter().collect(),
    })
}

fn skill_source_label(source: rebon_plugin_skill::SkillSource) -> &'static str {
    match source {
        rebon_plugin_skill::SkillSource::BuiltIn => "built-in",
        rebon_plugin_skill::SkillSource::User => "user",
        rebon_plugin_skill::SkillSource::Project => "project",
        rebon_plugin_skill::SkillSource::Managed => "managed",
        rebon_plugin_skill::SkillSource::Local => "local",
        rebon_plugin_skill::SkillSource::Flag => "flag",
        rebon_plugin_skill::SkillSource::Plugin => "plugin",
        rebon_plugin_skill::SkillSource::Mcp => "mcp",
    }
}

/// Open a registered panel and put it on the dialog stack. Silent when
/// the panel declines — every caller of that shape has its own fallback.
fn push_dialog(app: &mut AppState, id: &'static str, args: DialogArgs) {
    if let Some(dialog) = open_dialog(id, args) {
        app.dialogs.push_boxed(dialog);
    }
}

fn run_context(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_context_command(command).then_some(())?;
    let (app, session, _, _, ui_mode, ..) = cx.split();
    accept(app, session, command);
    let inputs = session_command_inputs_from_app(app, session.ui_mode);
    let output = execute_context_command(&inputs, session);
    if ui_mode == UiMode::Inline {
        feedback(app, "context", &output);
        app.dialogs.close_if(ids::dialog::CONTEXT);
    } else {
        let args = DialogArgs::values([context_input(&output, app.rebon_tui.transcript.rows())]);
        push_dialog(app, ids::dialog::CONTEXT, args);
    }
    Some(false)
}

/// Intercept `/permissions`: list / approve / retry auto-mode denials.
fn run_permissions(cx: Cx<'_>, command: &str) -> Option<bool> {
    let cmd = parse_permissions_command(command)?;
    let (app, session, handle, active_prompt, ..) = cx.split();
    accept(app, session, command);
    // Listing and approving are local bookkeeping; a retry is a model
    // turn, and a turn belongs to whoever owns the session. Checked
    // *before* the command runs: executing it marks the denials as
    // retried, so a refusal afterwards would leave them consumed by a
    // retry that never happened.
    if matches!(
        cmd,
        crate::session::commands::permissions::PermissionsCommand::Retry(_)
    ) {
        if let Some(reason) =
            super::prompt_lifecycle::local_turn_rejection(app, session, active_prompt.as_ref())
        {
            inject_system_message(app, "permissions", &reason);
            app.follow_transcript_tail = true;
            return Some(false);
        }
    }
    let inputs = session_command_inputs_from_app(app, session.ui_mode);
    let output = execute_permissions_command(&inputs, cmd);
    inject_system_message(app, "permissions", &output.text);
    if !output.replay_requests.is_empty() {
        prepare_for_new_prompt_after_withdrawal(app, &mut session.engine_half.update_rx);
        *active_prompt = admit_permission_replay_turn(
            app,
            session,
            handle,
            active_prompt.as_ref(),
            LocalTurnSource::PermissionRetry,
            output.replay_requests,
        );
    }
    app.follow_transcript_tail = true;
    Some(false)
}

/// Intercept `/prune`: context pruning commands.
fn run_prune(cx: Cx<'_>, command: &str) -> Option<bool> {
    let cmd = parse_prune_command(command)?;
    let (app, session, ..) = cx.split();
    accept(app, session, command);
    let output = execute_prune_command(cmd, session);
    inject_system_message(app, "prune", &output);
    app.follow_transcript_tail = true;
    Some(false)
}

/// Intercept `/compact [instructions]`: compact now, not "next request".
fn run_compact(cx: Cx<'_>, command: &str) -> Option<bool> {
    let cmd = parse_compact_command(command)?;
    let (app, session, handle, active_prompt, ..) = cx.split();
    accept(app, session, command);
    // Compaction rewrites the session's history with a model turn. On a
    // session a worker owns, that is the second writer again — and the
    // worker has its own `/compact`, reachable over the mirror.
    if refuse_local_turn_if_session_is_elsewhere(app, session, "/compact") {
        return Some(false);
    }
    // A running turn owns the live ContextManager, and the engine's
    // in-turn manual path already compacts inside it — starting a second
    // compaction here would race that one for the same history. Defer to
    // it, and say so, rather than refusing the command outright.
    if active_prompt.is_some() {
        let output = execute_compact_command(session, cmd.instructions.as_deref());
        inject_system_message(app, "local_command", &output);
        app.follow_transcript_tail = true;
        return Some(false);
    }
    match start_manual_compact(app, session, handle, cmd.instructions, None) {
        CompactStart::Started => {}
        CompactStart::Rejected(reason) => {
            inject_system_message(app, "local_command", &reason);
        }
    }
    app.follow_transcript_tail = true;
    Some(false)
}

/// Intercept `/effort`: bare opens the picker; arguments retain direct-set
/// behavior.
fn run_effort(cx: Cx<'_>, command: &str) -> Option<bool> {
    let result = parse_effort_command(command)?;
    let (app, session, ..) = cx.split();
    accept(app, session, command);
    if matches!(&result, EffortCommand::Show) {
        let level = app.effort_level.map(|level| level.as_str()).unwrap_or("");
        push_dialog(
            app,
            ids::dialog::EFFORT,
            DialogArgs::values([session.model.name.as_str(), level]),
        );
        return Some(false);
    }
    // A mirrored session runs on the owner's effort, not this terminal's
    // configuration. Persisting it here would change what the *next*
    // local session does and nothing about the one on screen.
    if let EffortCommand::Set(level) = &result {
        if super::remote_session_option::route_session_option_to_owner(
            app,
            session,
            "effort",
            level.as_str(),
        ) {
            return Some(false);
        }
    }
    let changed = matches!(&result, EffortCommand::Set(_));
    let output =
        execute_persisted_effort_command(&mut app.effort_level, app.effort_provider_kind, result);
    if changed {
        if let Err(error) = rebon_session::model_selection::save_manual_effort(
            &session.engine_half.runtime.projects_root,
            &session.cwd,
            &session.session_id,
            app.effort_level,
        ) {
            inject_system_message(
                app,
                "warning",
                &format!("Cannot persist session effort: {error}"),
            );
        }
    }
    tracing::debug!("rebon-cli: /effort - {output}");
    inject_local_command_feedback_with_command(app, "effort", command, &output);
    app.follow_transcript_tail = true;
    Some(false)
}

fn run_fast(cx: Cx<'_>, command: &str) -> Option<bool> {
    let cmd = parse_fast_command(command)?;
    let (app, session, ..) = cx.split();
    accept(app, session, command);
    let is_update = matches!(
        cmd,
        crate::session::commands::effort::FastCommand::On
            | crate::session::commands::effort::FastCommand::Off
    );
    let output = execute_fast_command(session, cmd);
    super::transcript_messages::inject_fast_command_result(app, output, is_update);
    Some(false)
}

fn run_update(cx: Cx<'_>, command: &str) -> Option<bool> {
    let cmd = rebon_plugin_updater::parse_update_command(command)?;
    let (app, session, handle, ..) = cx.split();
    accept(app, session, command);
    handle_update_command(app, handle, cmd);
    app.follow_transcript_tail = true;
    Some(false)
}

// ----------------------------------------------------- command-made turns

/// Intercept `/ultraplan` (and `/grill`): local-only deep multi-agent plan
/// workflow.
fn run_ultraplan(cx: Cx<'_>, command: &str) -> Option<bool> {
    let cmd = parse_ultraplan_command(command)?;
    let (app, session, handle, active_prompt, _, cursor_offset, transcript_len, ..) = cx.split();
    accept(app, session, command);
    match cmd {
        UltraplanCommand::Help => {
            inject_system_message(app, "local_command", ultraplan_usage());
            app.follow_transcript_tail = true;
        }
        UltraplanCommand::List => {
            let output = format_ultraplan_run_list(session);
            feedback(app, "ultraplan", &output);
        }
        UltraplanCommand::Review => {
            let output = execute_ultraplan_manual_review_command(app, session);
            feedback(app, "ultraplan", &output);
        }
        UltraplanCommand::Error(err) => {
            inject_system_message(
                app,
                "local_command",
                &format!("Error: {err}\n\n{}", ultraplan_usage()),
            );
            app.follow_transcript_tail = true;
        }
        UltraplanCommand::Resume { run_id } => {
            if refuse_local_turn_if_session_is_elsewhere(app, session, "/ultraplan") {
                return Some(false);
            }
            prepare_for_new_prompt_after_withdrawal(app, &mut session.engine_half.update_rx);
            if let Some(submit) = prepare_ultraplan_resume_submit(app, session, &run_id) {
                start_command_turn(
                    app,
                    session,
                    handle,
                    active_prompt,
                    submit,
                    cursor_offset,
                    transcript_len,
                );
            }
        }
        UltraplanCommand::Task(spec) => {
            if refuse_local_turn_if_session_is_elsewhere(app, session, "/ultraplan") {
                return Some(false);
            }
            prepare_for_new_prompt_after_withdrawal(app, &mut session.engine_half.update_rx);
            if let Some(submit) = prepare_ultraplan_submit(app, command, &spec, session) {
                start_command_turn(
                    app,
                    session,
                    handle,
                    active_prompt,
                    submit,
                    cursor_offset,
                    transcript_len,
                );
            }
        }
    }
    Some(false)
}

/// Intercept `/ultrawork` and `/ulw`: submit a workflow orchestration request
/// and add an explicit model reminder to use the Workflow tool.
fn run_ultrawork(cx: Cx<'_>, command: &str) -> Option<bool> {
    let (name, request) = parse_ultrawork_workflow_command(command)?;
    let (app, session, handle, active_prompt, _, cursor_offset, transcript_len, ..) = cx.split();
    accept(app, session, command);
    if refuse_local_turn_if_session_is_elsewhere(app, session, name) {
        return Some(false);
    }
    let submit_text = if request.is_empty() {
        name.to_string()
    } else {
        request
    };
    prepare_for_new_prompt_after_withdrawal(app, &mut session.engine_half.update_rx);
    if let Some(submit) = prepare_ultrawork_workflow_submit(app, &submit_text, &session.session_id)
    {
        start_command_turn(
            app,
            session,
            handle,
            active_prompt,
            submit,
            cursor_offset,
            transcript_len,
        );
    }
    Some(false)
}

/// Intercept `/ceo`:
/// - `/ceo` or `/ceo on` enters coordinator mode
/// - `/ceo off` exits coordinator mode
/// - `/ceo <task>` enters coordinator mode + submits task
fn run_ceo(cx: Cx<'_>, command: &str) -> Option<bool> {
    let cmd = parse_ceo_command(command)?;
    let (app, session, handle, active_prompt, _, cursor_offset, transcript_len, ..) = cx.split();
    accept(app, session, command);
    match cmd {
        CeoCommand::On => {
            if !app.coordinator_mode {
                enter_coordinator_mode(app, session);
            }
        }
        CeoCommand::Off => {
            if app.coordinator_mode {
                exit_coordinator_mode(app, session);
            }
        }
        CeoCommand::Task(task) => {
            if refuse_local_turn_if_session_is_elsewhere(app, session, "/ceo") {
                return Some(false);
            }
            if !app.coordinator_mode {
                enter_coordinator_mode(app, session);
            }
            prepare_for_new_prompt_after_withdrawal(app, &mut session.engine_half.update_rx);
            if let Some(submit) = apply_submit(app, &task, &session.session_id) {
                start_command_turn(
                    app,
                    session,
                    handle,
                    active_prompt,
                    submit,
                    cursor_offset,
                    transcript_len,
                );
            }
        }
    }
    Some(false)
}

// ------------------------------------------------------------- switches

/// Intercept `/kernel`: which kernel runs this session — rebon's native
/// engine or an embedded loop vendor (dsh|pi|opencode). A first-class name
/// because the sub-agent spawner's `/agent` command shadows the space form of
/// the agent switch.
fn run_kernel(cx: Cx<'_>, command: &str) -> Option<bool> {
    let args = super::commands::parse_kernel_command(command)?.to_string();
    let (app, session, _, active_prompt, ..) = cx.split();
    accept(app, session, command);
    if active_prompt.is_some() && rebon_agent_core::routing::args_change_the_backend(&args) {
        inject_system_message(
            app,
            "error",
            "Cannot switch kernels while a prompt is running.",
        );
        app.follow_transcript_tail = true;
        return Some(false);
    }
    let result = crate::session::commands::kernel::handle_kernel_command(
        &session.engine_half.runtime.session_agents,
        &args,
        session.runtime_handle().as_ref(),
    );
    command_result(app, result.is_err, &result.text);
    Some(false)
}

/// Intercept `/backend` — and `/agent`, which reaches the same switch: which
/// agent runs this session's turns, rebon's own engine or one of the ACP agent
/// CLIs configured under `acpAgents`. Distinct from `/agents`, which browses
/// agent definitions.
///
/// The switch flips state in *this* process, so a session whose engine lives
/// elsewhere is refused rather than quietly switched here: the receipt would
/// say "Switched" while the next turn ran on the old agent, and `set_current`
/// would overwrite that worker's sidecar on the way out. An attached
/// `/backend` never reaches this point — the session-control gate in
/// `submit.rs` forwarded it — but `/agent` cannot be forwarded (its other
/// meaning is a prompt bound to this terminal) and a session mid-handover is
/// not attached yet, so both arrive here and both have to be turned away.
fn run_backend_switch(cx: Cx<'_>, command: &str, name: &'static str) -> Option<bool> {
    let args = if name == "/backend" {
        rebon_agent_core::routing::parse_backend_command(command)?
    } else {
        rebon_agent_core::routing::parse_agent_command(command)?
    }
    .to_string();
    let (app, session, _, active_prompt, ..) = cx.split();
    accept(app, session, command);
    if refuse_local_turn_if_session_is_elsewhere(app, session, name) {
        return Some(false);
    }
    if active_prompt.is_some() && rebon_agent_core::routing::args_change_the_backend(&args) {
        inject_system_message(
            app,
            "error",
            "Cannot hand this session to another agent while a prompt is running.",
        );
        app.follow_transcript_tail = true;
        return Some(false);
    }
    let result = rebon_agent_core::routing::handle_agent_command(
        &session.engine_half.runtime.session_agents,
        &args,
        session.runtime_handle().as_ref(),
    );
    command_result(app, result.is_err, &result.text);
    Some(false)
}

/// Intercept `/profile`: provider, model, agent backend, permission mode and
/// tool surface applied as one named bundle. Every one of those is a switch
/// some other command already owns, so the interesting part is the guards — a
/// profile must not become a way around them.
fn run_profile(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_profile_command(command)?;
    let (app, session, handle, active_prompt, ..) = cx.split();
    accept(app, session, command);
    // Only when the profile actually hands over the agent. A profile that
    // just moves the model has no reason to be refused mid-turn.
    if profile_command::switches_agent(command) {
        if refuse_local_turn_if_session_is_elsewhere(app, session, "/profile") {
            return Some(false);
        }
        if active_prompt.is_some() {
            inject_system_message(
                app,
                "error",
                "That profile hands this session to another agent, which cannot happen while a prompt is running.",
            );
            app.follow_transcript_tail = true;
            return Some(false);
        }
    }
    let result = profile_command::handle_profile_command(app, session, handle, command);
    command_result(app, result.is_err, &result.text);
    Some(false)
}

/// Intercept the `/provider` slash command locally. A bare command opens the
/// provider panel in screen mode; subcommands keep using the text CRUD surface
/// for custom API providers stored in `config.json`.
fn run_provider(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_provider_command(command)?;
    let (app, session, handle, ..) = cx.split();
    accept(app, session, command);
    // A bare `/provider` opens the lightweight switcher: pick a
    // provider and Enter to activate it (add/edit/remove escalate from
    // there). `/provider add` opens the full add form directly. Both
    // render fullscreen in screen mode and in the prompt input area in
    // inline mode. Every other subcommand (`/provider list|use|…`)
    // keeps the text CRUD surface below.
    // Matched with the recognizer's rules, not literal spellings: the
    // recognizer accepts `/Provider`, so these routes have to as well or
    // the capitalised spelling silently falls through to the text CRUD.
    let provider_args = super::commands::strip_command_prefix(command, "provider")
        .map(|rest| {
            rest.trim_start_matches(|c: char| c == ' ' || c == ':')
                .trim()
        })
        .unwrap_or("");
    if provider_args.is_empty() {
        push_dialog(app, ids::dialog::PROVIDER, DialogArgs::none());
        return Some(false);
    }
    if provider_args.eq_ignore_ascii_case("add") {
        let mut dialog = OnboardingDialogState::open_for_provider_command();
        dialog.enter_provider_add_mode();
        app.onboarding_dialog = Some(dialog);
        open_onboarding_dialog(app, session, handle, "slash_provider");
        return Some(false);
    }
    // Reconnecting reaches the session's runtime cache, which the pure
    // configuration handler has no access to.
    let result = if provider_args.eq_ignore_ascii_case("reconnect") {
        let text = execute_provider_reconnect(session);
        let is_err = text.starts_with('!');
        ProviderCommandResult {
            text: text.trim_start_matches('!').to_string(),
            is_err,
            runtime_update: None,
        }
    } else {
        handle_provider_command(command)
    };
    if result.is_err || result.runtime_update.is_none() {
        command_result(app, result.is_err, &result.text);
    } else if refresh_runtime_model(app, session, result.runtime_update, handle) {
        super::transcript_messages::inject_provider_switch(
            app,
            &session.model.provider_name,
            &session.model.name,
        );
    }
    Some(false)
}

fn run_model(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_model_command(command)?;
    let (app, session, handle, ..) = cx.split();
    // A bare `/model` opens the model picker over the active
    // provider's configured models; `/model <name>` keeps the direct
    // text switch. When no custom provider is active the picker can't
    // open, so we fall through to the text command whose message
    // explains how to activate one first.
    let model_args = super::commands::strip_command_prefix(command, "model")
        .unwrap_or("")
        .trim_start_matches(|c: char| c == ' ' || c == ':')
        .trim();
    if model_args.is_empty() {
        if let Some(dialog) = open_dialog(ids::dialog::MODEL, DialogArgs::none()) {
            accept(app, session, command);
            app.dialogs.push_boxed(dialog);
            return Some(false);
        }
    }
    accept(app, session, command);
    // As with `/effort` a mirrored session's model is the owner's; `refresh` is local.
    if !matches!(model_args.to_ascii_lowercase().as_str(), "" | "refresh")
        && super::remote_session_option::route_session_option_to_owner(
            app, session, "model", model_args,
        )
    {
        return Some(false);
    }
    let result = handle_model_command(command);
    if result.is_err || result.runtime_update.is_none() {
        command_result(app, result.is_err, &result.text);
    } else if refresh_runtime_model(app, session, result.runtime_update, handle)
        && !app.refresh_empty_startup_banner()
    {
        inject_system_message(app, "local_command", &result.text);
    }
    app.follow_transcript_tail = true;
    Some(false)
}

fn run_plugin(cx: Cx<'_>, command: &str) -> Option<bool> {
    parse_plugin_command(command)?;
    let (app, session, ..) = cx.split();
    accept(app, session, command);
    let plugin_args = super::commands::strip_command_prefix(command, "plugin")
        .unwrap_or("")
        .trim_start_matches(|c: char| c == ' ' || c == ':')
        .trim();
    if plugin_args.is_empty() {
        app.slash_picker = None;
        app.at_mention_picker = None;
        let result = handle_plugin_command("/plugin list", session);
        push_dialog(
            app,
            ids::dialog::PLUGINS,
            DialogArgs::values([
                result.text.as_str(),
                if result.is_err { "err" } else { "ok" },
            ]),
        );
        return Some(false);
    }
    let result = handle_plugin_command(command, session);
    command_result(app, result.is_err, &result.text);
    Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn fast_command_success_updates_banner_without_starting_transcript() {
        use crate::session::commands::effort::FastCommand;
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("fast-banner");
        let mut session = super::super::test_support::make_test_tui_session();
        session.model.service_tier_available = true;
        let mut app = AppState::new();
        for (command, enabled) in [(FastCommand::On, true), (FastCommand::Off, false)] {
            let result = execute_fast_command(&session, command);
            assert!(result.is_ok(), "{result:?}");
            super::super::transcript_messages::inject_fast_command_result(&mut app, result, true);
            assert_eq!(session.model.service_tier.is_fast(), enabled);
            assert!(app.pending_inline_banner_refresh);
            assert!(app.rebon_tui.transcript.is_empty());
        }
    }

    #[test]
    fn fast_command_status_invalid_and_unavailable_keep_visible_feedback() {
        use crate::session::commands::effort::FastCommand;
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("fast-feedback");
        let mut session = super::super::test_support::make_test_tui_session();
        session.model.service_tier_available = false;
        for (command, is_update, is_err) in [
            (FastCommand::Status, false, false),
            (FastCommand::Invalid("bad".into()), false, true),
            (FastCommand::On, true, true),
        ] {
            let mut app = AppState::new();
            let result = execute_fast_command(&session, command);
            assert_eq!(result.is_err(), is_err);
            super::super::transcript_messages::inject_fast_command_result(
                &mut app, result, is_update,
            );
            assert_eq!(app.rebon_tui.transcript.rows().len(), 1);
            assert!(!app.pending_inline_banner_refresh);
        }
    }

    #[test]
    fn fast_command_persist_failure_is_not_hidden_by_banner_refresh() {
        use crate::session::commands::effort::FastCommand;
        let home = rebon_tool::tasks::test_support::TestConfigHome::new("fast-persist-error");
        let mut session = super::super::test_support::make_test_tui_session();
        session.model.service_tier_available = true;
        std::fs::create_dir(home.path().join("config.json")).unwrap();
        let result = execute_fast_command(&session, FastCommand::On);
        assert!(result.is_err(), "{result:?}");
        let mut app = AppState::new();
        super::super::transcript_messages::inject_fast_command_result(&mut app, result, true);
        let rebon_tui::Message::System(message) = &app.rebon_tui.transcript.rows()[0] else {
            panic!("expected system message");
        };
        assert!(message
            .content
            .as_deref()
            .unwrap()
            .contains("Persist warning"));
        assert!(!app.pending_inline_banner_refresh);
    }

    /// The `/` picker is seeded from the `command-registry` seat, so a command
    /// registered `Native` on the `Tui` surface is a promise the submit path
    /// has to keep. Nothing enforced that, and four commands had drifted in:
    /// `/shortcuts`, `/runtime`, `/automation` and `/devices` are settings
    /// pages the desktop opens, and picking one in the terminal did not report
    /// anything — it sent the text to the model as a prompt. Those four are
    /// `CommandHandler::Explain` now, which is why they are not below.
    ///
    /// Equality in both directions on purpose. A name here the seat does not
    /// offer is a command nobody can find, which is the same drift walking the
    /// other way.
    #[test]
    fn the_seat_offers_exactly_what_this_module_dispatches() {
        // The catalog reads the seat the kernel boot installs; without a
        // booted kernel there is nothing to compare.
        rebon_harness::kernel_bootstrap::process_kernel();
        let seat = rebon_kernel_seats::kernel_core_commands::command_seat()
            .expect("core-commands is a Core plugin and always loads");
        let mut offered: Vec<String> = seat
            .all()
            .iter()
            .filter(|command| {
                // Every `Native` handler on Tui, whoever registered it. The
                // handler kind *is* the claim: `Native(id)` means "this front
                // end runs it under that id", and a Rust plugin that says so
                // (`plugins/profile` registers `/profile`) is claiming this
                // module the same way a built-in does. Nothing else can reach
                // this arm — a Node plugin's command is only ever Explain,
                // Panel or Prompt.
                command
                    .spec
                    .available_on(rebon_slash_commands::Surface::Tui)
                    && matches!(
                        command.handler,
                        rebon_kernel_seats::kernel_core_commands::CommandHandler::Native(_)
                    )
            })
            .map(|command| command.spec.name.to_string())
            .collect();
        offered.sort_unstable();

        let mut runs: Vec<String> = NATIVE_COMMAND_IDS.iter().map(|id| id.to_string()).collect();
        runs.sort_unstable();
        assert_eq!(
            runs.iter().collect::<HashSet<_>>().len(),
            NATIVE_COMMAND_IDS.len(),
            "NATIVE_COMMAND_IDS lists a name twice"
        );

        let undispatched: Vec<&String> =
            offered.iter().filter(|name| !runs.contains(name)).collect();
        assert!(
            undispatched.is_empty(),
            "the seat offers {undispatched:?} as Native on Tui, which nothing here runs — \
             they would reach the model as prompt text"
        );
        let unreachable: Vec<&String> =
            runs.iter().filter(|name| !offered.contains(name)).collect();
        assert!(
            unreachable.is_empty(),
            "this module claims {unreachable:?}, which the seat does not register Native on Tui"
        );
    }

    /// `/exit` and `/stop` are the two ids `native_dispatch` deliberately has
    /// no arm for: `submit.rs` runs them before the stale-projection gate and
    /// before session-control forwarding, so they cannot wait for the seat
    /// dispatch. Everything else in the list is an arm.
    #[test]
    fn only_exit_and_stop_run_before_the_dispatcher() {
        for id in NATIVE_COMMAND_IDS {
            let dispatched = !HANDLED_BEFORE_DISPATCH.contains(id);
            assert_eq!(
                dispatched,
                !matches!(*id, "exit" | "stop"),
                "/{id} is in the wrong list"
            );
        }
    }

    /// `/agent-view` is the one spelling the terminal answers to that the seat
    /// does not carry.
    #[test]
    fn the_unlisted_agent_view_spelling_still_resolves() {
        assert_eq!(
            unlisted_command_alias("agent-view"),
            Some("background-agents")
        );
        assert_eq!(
            unlisted_command_alias("AGENT-VIEW"),
            Some("background-agents")
        );
        assert_eq!(unlisted_command_alias("agents"), None);
    }
}
