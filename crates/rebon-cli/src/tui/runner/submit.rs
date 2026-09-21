use std::path::Path;

use crate::goal::{
    build_goal_clarification_request, build_goal_continuation_prompt,
    build_goal_refinement_confirmation, build_refined_goal_prompt, goal_needs_clarification,
    PendingGoalClarification,
};
use rebon_core::hooks::{
    append_additional_context, apply_user_prompt_submit_effects, UserPromptSubmitDecision,
};
use rebon_types::ExecutionPolicy;
use tokio::runtime::Handle;

use crate::session::submit_payload::PendingSkillInvocation;
use crate::tui::app::AppState;
use crate::tui::dispatch::{
    apply_submit, apply_submit_with_images_and_uuid, clear_input,
    commit_submit_payload_to_transcript, enqueue_submit_payload, take_submit_payload,
    take_submit_payload_with_images,
};
use crate::tui::mcp_dialog::McpDialogState;
use crate::tui::permission_modal::PendingPermission;
use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::UiMode;
use rebon_plugin_skill::parse_user_skill_invocation;

use super::commands::{parse_exit_command, parse_stop_command, set_goal};
use super::foreground_agent_submit::submit_to_foregrounded_agent;
use super::layout_and_scroll::repin_transcript_to_bottom;
use super::live_agent_view::release_terminal_foreground_agent;
use super::native_commands;
use super::prompt_history::save_to_history_for_session_if_needed;
use super::prompt_lifecycle::{
    admit_active_prompt, live_input_withdrawable_submit_from_payload,
    prepare_for_new_prompt_after_withdrawal,
};
use super::session_detach_attach::stop_attached_background_session;
use super::task_runtime::spawn_inline_shell_command;
use super::title::{apply_session_title, mark_session_title_completed};
use super::transcript_messages::{
    inject_local_command_feedback, inject_local_command_feedback_with_command,
    inject_system_message,
};
use super::ultraplan::{
    format_ultraplan_restore_prompt, maybe_prepare_implicit_ultraplan_submit,
    restore_ultraplan_run_for_runtime, ultraplan_context_for_run_state,
};
use super::{ActivePrompt, LocalTurnSource};
use crate::session::commands::mcp::{parse_mcp_command, McpCommand};
use crate::session::commands::ultraplan_prompt::has_triggerable_ultrawork_keyword;
use crate::session::ultraplan_review::{start_review, ReviewKind};
use crate::session::ultraplan_run::{persist_ultraplan_run, status_phase_from_run_phase};

/// Returns `true` when the app should quit (e.g. `/exit`, `/quit`).
pub(super) fn submit_or_queue(
    app: &mut AppState,
    text: String,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    pending_permission: &mut Option<PendingPermission>,
    ui_mode: UiMode,
) -> bool {
    submit_or_queue_with_images(
        app,
        text,
        Vec::new(),
        session,
        handle,
        active_prompt,
        pending_permission,
        ui_mode,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn submit_or_queue_with_images(
    app: &mut AppState,
    text: String,
    images: Vec<rebon_session_host::BackgroundImageAttachment>,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    pending_permission: &mut Option<PendingPermission>,
    ui_mode: UiMode,
) -> bool {
    submit_or_queue_with_images_and_uuid(
        app,
        text,
        images,
        None,
        session,
        handle,
        active_prompt,
        pending_permission,
        ui_mode,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn submit_or_queue_with_images_and_uuid(
    app: &mut AppState,
    mut text: String,
    images: Vec<rebon_session_host::BackgroundImageAttachment>,
    user_message_uuid: Option<String>,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    pending_permission: &mut Option<PendingPermission>,
    ui_mode: UiMode,
) -> bool {
    let injected_images = images
        .iter()
        .map(rebon_session_host::BackgroundImageAttachment::to_prompt_paste_content)
        .collect::<Vec<_>>();
    let transcript_len_before_submit = app.rebon_tui.transcript.len();
    let cursor_offset_before_submit = app.cursor_offset;
    if !text.trim().is_empty() {
        // The other end of the first-token measurement: the
        // first `tui: append streaming text` after this line is the first
        // token on screen.
        tracing::info!(chars = text.len(), "rebon turn: prompt submitted");
    }
    if let Some(quit) = run_before_dispatch(
        app,
        text.trim_end(),
        session,
        active_prompt,
        pending_permission,
        ui_mode,
    ) {
        return quit;
    }

    // One expansion at a time. A second line submitted while the first is
    // still being expanded would start its turn first, and the expansion
    // would then land behind it -- two prompts in an order neither the person
    // nor the plugin chose. `/exit` and `/stop` are decided above this line
    // and still work.
    if let Some(pending) = app.expanding_command.as_ref() {
        let name = pending.name.clone();
        inject_system_message(
            app,
            "command-expanding",
            &format!("/{name} is still expanding. Press Esc to stop waiting for it."),
        );
        app.follow_transcript_tail = true;
        return false;
    }

    // Commands come from the `command-registry` seat (RFC kernel-plugins §13).
    // `Native` is the terminal's own implementation, which lives in
    // `native_commands`; Prompt / Explain / Panel are answered here. A
    // `Native` branch that declines — its arguments do not parse — leaves the
    // line to go on as prompt text, which is what falling off the end of the
    // fifty-branch chain this replaced used to do.
    let native_id = match rebon_slash_commands::leading_command_token(text.trim_end()) {
        Some(token) => match rebon_kernel_seats::kernel_core_commands::find_command(token) {
            Some(command) => {
                use rebon_kernel_seats::kernel_core_commands::{CommandArgs, CommandHandler};
                match command.handler {
                    CommandHandler::Native(id) => Some(id.to_string()),
                    CommandHandler::Prompt(expand) => {
                        let args = CommandArgs::from_line(
                            text.trim_end(),
                            &command.spec,
                            rebon_slash_commands::Surface::Tui,
                        );
                        // Who registered the command decides how long the
                        // answer takes, and a plugin's answer comes from
                        // another process. Off the loop it goes, so the
                        // terminal keeps drawing while the plugin thinks.
                        if let Some(expansion_tx) = app.command_expansion_tx.clone() {
                            spawn_command_expansion(
                                app,
                                handle,
                                command.spec.name.as_ref(),
                                text.trim_end(),
                                expand,
                                args,
                                expansion_tx,
                            );
                            return false;
                        }
                        match expand(&args) {
                            Ok(expanded) => {
                                text = expanded;
                                None
                            }
                            // A command that could not be expanded is not
                            // prompt text: saying so is the whole answer, and
                            // sending the failure to the model would be worse
                            // than saying nothing.
                            Err(failure) => {
                                save_to_history_for_session_if_needed(
                                    app,
                                    session,
                                    text.trim_end(),
                                );
                                clear_input(app);
                                inject_system_message(app, "command-explain", &failure);
                                return false;
                            }
                        }
                    }
                    CommandHandler::Explain(explanation) => {
                        save_to_history_for_session_if_needed(app, session, text.trim_end());
                        clear_input(app);
                        inject_system_message(app, "command-explain", &explanation);
                        return false;
                    }
                    CommandHandler::Panel(panel) => {
                        save_to_history_for_session_if_needed(app, session, text.trim_end());
                        clear_input(app);
                        // The `ui-registry` seat: a plugin's panel opens like a built-in's,
                        // and no factory for the id says so rather than doing nothing.
                        let args = rebon_ui_seat::DialogArgs::none();
                        match crate::tui::ui_registry::open(&panel, args) {
                            Some(dialog) => app.dialogs.push_boxed(dialog),
                            None => inject_system_message(app, "command-explain",
                                &format!("/{} opens the `{panel}` panel, which this terminal has no dialog for.", command.spec.name)),
                        }
                        return false;
                    }
                }
            }
            None => native_commands::unlisted_command_alias(token).map(str::to_string),
        },
        None => None,
    };
    if let Some(id) = native_id {
        if let Some(done) = native_commands::native_dispatch(
            &id,
            &mut text,
            app,
            session,
            handle,
            active_prompt,
            ui_mode,
            cursor_offset_before_submit,
            transcript_len_before_submit,
        ) {
            return done;
        }
    }

    submit_after_command_dispatch(
        app,
        text,
        injected_images,
        user_message_uuid,
        session,
        handle,
        active_prompt,
        cursor_offset_before_submit,
        transcript_len_before_submit,
    )
}

/// The submit path once the `command-registry` seat has had its look: the
/// line is prompt text now, whatever it was typed as.
///
/// Split out so an expansion that finished after the loop moved on can rejoin
/// the path here (see [`submit_expanded_text`]) instead of going back through
/// the command layer, where the expanded text would be dispatched a second
/// time — a plugin whose expansion begins with a slash would run whatever it
/// named.
#[allow(clippy::too_many_arguments)]
fn submit_after_command_dispatch(
    app: &mut AppState,
    text: String,
    injected_images: Vec<rebon_types::PromptPasteContent>,
    user_message_uuid: Option<String>,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    cursor_offset_before_submit: usize,
    transcript_len_before_submit: usize,
) -> bool {
    if let Some((cmd, history_text)) = parse_bang_shell_command(&app.mode, text.trim_end()) {
        if !cmd.is_empty() {
            if let Some(reason) = super::prompt_lifecycle::local_turn_rejection(app, session, None)
            {
                inject_system_message(app, "error", &reason);
                return false;
            }
            spawn_inline_shell_command(app, session, handle, &cmd, &history_text);
            app.follow_transcript_tail = true;
            tracing::info!(command = %cmd, "rebon-cli: ! started inline shell command");
        }
        save_to_history_for_session_if_needed(app, session, &history_text);
        clear_input(app);
        app.mode = String::from("prompt");
        return false;
    }

    if let Some(message) = disabled_skill_command_message(session, text.trim_end()) {
        inject_system_message(app, "error", &message);
        app.follow_transcript_tail = true;
        return false;
    }

    if let Some(message) = slash_command_typo_message(app, session, text.trim_end()) {
        inject_system_message(app, "error", &message);
        app.follow_transcript_tail = true;
        return false;
    }

    if app.foregrounded_task_id.is_some()
        && release_terminal_foreground_agent(app, session.engine_half.tasks.as_ref())
    {
        return false;
    }

    if let Some(pending_goal) = app.pending_goal_clarification.take() {
        save_to_history_for_session_if_needed(app, session, text.trim_end());
        if let Some(submit) = take_submit_payload_with_images(app, &text, injected_images.clone()) {
            let clarification = submit.prompt_text().trim().to_string();
            if clarification.is_empty() {
                app.pending_goal_clarification = Some(pending_goal);
                inject_system_message(
                    app,
                    "error",
                    "Please provide goal details before the goal can start.",
                );
                app.follow_transcript_tail = true;
                return false;
            }
            let refined_goal = build_refined_goal_prompt(&pending_goal.prompt, &clarification);
            set_refined_goal_and_start(
                app,
                refined_goal,
                pending_goal.max_sessions,
                session,
                handle,
                active_prompt,
            );
        }
        return false;
    }

    if let Some(task_id) = app.foregrounded_task_id.clone() {
        save_to_history_for_session_if_needed(app, session, text.trim_end());
        submit_to_foregrounded_agent(app, session, handle, &task_id, &text);
        return false;
    }

    if let Some(done) =
        route_prompt_to_a_worker(app, session, &text, &injected_images, &user_message_uuid)
    {
        return done;
    }

    if active_prompt.is_some() {
        save_to_history_for_session_if_needed(app, session, text.trim_end());
        let Some(submit) = take_submit_payload_with_images(app, &text, injected_images.clone())
        else {
            return false;
        };
        let mut submit = submit;
        if user_message_uuid.is_some() {
            submit.user_message_uuid = user_message_uuid.clone();
        }
        if !attach_skill_invocation_if_registered(app, session, &mut submit) {
            return false;
        }
        maybe_prepare_implicit_ultraplan_submit(app, session, &mut submit);
        maybe_prepare_implicit_ultrawork_submit(app, &mut submit);
        app.is_loading = true;
        enqueue_submit_payload(app, submit);
        repin_transcript_to_bottom(app);
        return false;
    }

    prepare_for_new_prompt_after_withdrawal(app, &mut session.engine_half.update_rx);
    save_to_history_for_session_if_needed(app, session, text.trim_end());
    let Some(mut submit) = apply_submit_with_images_and_uuid(
        app,
        &text,
        &session.session_id,
        injected_images,
        user_message_uuid,
    ) else {
        return false;
    };
    if !attach_skill_invocation_if_registered(app, session, &mut submit) {
        return false;
    }
    maybe_prepare_implicit_ultrawork_submit(app, &mut submit);
    {
        let verdict = handle.block_on(session.engine_half.runtime.policy.emit(
            rebon_core::policy_seat::HookEventPayload::UserPromptSubmit {
                prompt: submit.prompt_text().to_string(),
            },
        ));
        // The same projection a worker applies before it runs a claimed
        // prompt (`rebon_core::hooks::apply_user_prompt_submit_effects`):
        // a hosted session's UserPromptSubmit runs over there, and what
        // a hook may do to a prompt must not depend on where the turn runs.
        let decision = match verdict.denial() {
            Some(reason) => UserPromptSubmitDecision::Blocked {
                reason: reason.to_string(),
            },
            None => apply_user_prompt_submit_effects(verdict.effects()),
        };
        match decision {
            UserPromptSubmitDecision::Blocked { reason } => {
                inject_system_message(app, "error", &reason);
                app.follow_transcript_tail = true;
                return false;
            }
            UserPromptSubmitDecision::Continue(effects) => {
                for context_text in &effects.additional_context {
                    append_additional_context(&mut submit.text, context_text);
                }
                if let Some(title) = effects.session_title {
                    apply_session_title(app, session, handle, title);
                }
                if effects.mark_session_complete {
                    mark_session_title_completed(app, session, handle);
                }
                for text in &effects.system_messages {
                    inject_system_message(app, "info", text);
                }
            }
        }
    }
    maybe_prepare_implicit_ultraplan_submit(app, session, &mut submit);
    maybe_prepare_implicit_ultrawork_submit(app, &mut submit);
    repin_transcript_to_bottom(app);
    let withdrawable = live_input_withdrawable_submit_from_payload(
        &submit,
        cursor_offset_before_submit,
        transcript_len_before_submit,
        app.rebon_tui.transcript.len(),
    );
    if let Some(admitted) = admit_active_prompt(
        app,
        session,
        handle,
        active_prompt.as_ref(),
        LocalTurnSource::DirectUserSubmit,
        submit,
    ) {
        *active_prompt = Some(admitted.with_withdrawable(withdrawable));
        app.is_loading = true;
    }
    false
}

/// Run a `Prompt` command's expansion off the event loop, and say on screen
/// that it is running.
///
/// The input is cleared the way a submitted line is: the line is on its way,
/// and a person who gives up on it (Esc) or whose command fails gets it back
/// from history, which is why the line as typed is remembered here.
#[allow(clippy::too_many_arguments)]
fn spawn_command_expansion(
    app: &mut AppState,
    handle: &Handle,
    name: &str,
    line: &str,
    expand: std::sync::Arc<
        dyn Fn(&rebon_kernel_seats::kernel_core_commands::CommandArgs) -> Result<String, String>
            + Send
            + Sync,
    >,
    args: rebon_kernel_seats::kernel_core_commands::CommandArgs,
    expansion_tx: tokio::sync::mpsc::UnboundedSender<(u64, Result<String, String>)>,
) {
    let id = app.next_command_expansion_id;
    app.next_command_expansion_id += 1;
    app.expanding_command = Some(crate::tui::app::ExpandingCommand {
        id,
        name: name.to_string(),
        line: line.to_string(),
    });
    clear_input(app);
    inject_system_message(app, "command-expanding", &format!("Expanding /{name}…"));
    app.follow_transcript_tail = true;
    // `spawn_blocking`: the closure came from the seat and may be a plugin
    // proxy that blocks on another process. On a worker thread it blocks
    // nothing that draws.
    handle.spawn_blocking(move || {
        let _ = expansion_tx.send((id, expand(&args)));
    });
}

/// Apply an expansion that finished while the loop went on drawing.
///
/// Called once a pass from the event loop. Results are matched to the wait in
/// flight by id, and one that matches nothing is read and dropped: an
/// abandoned expansion (Esc) still finishes and still answers, and the person
/// who typed a *different* command in the meantime must not have the
/// abandoned one's expansion submitted under the new one's name.
pub(super) fn drain_command_expansion(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    command_expansion_rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u64, Result<String, String>)>,
) {
    while let Ok((id, outcome)) = command_expansion_rx.try_recv() {
        let Some(pending) = app.expanding_command.take_if(|pending| pending.id == id) else {
            continue;
        };
        match outcome {
            Ok(expanded) => {
                submit_expanded_text(app, expanded, session, handle, active_prompt);
            }
            // Same as the inline path: a command that could not be expanded
            // is not prompt text, so it is said rather than sent.
            Err(failure) => {
                save_to_history_for_session_if_needed(app, session, &pending.line);
                inject_system_message(app, "command-explain", &failure);
                app.follow_transcript_tail = true;
            }
        }
        return;
    }
}

/// Submit an expanded command as the prompt it expanded to, entering the
/// submit path below the command layer.
///
/// The withdrawable-input measurements are taken here rather than carried
/// from the typed line: the submit starts now, and the transcript has the
/// "Expanding" notice in it that the typed line did not.
///
/// Nothing is returned: the submit path's "should quit" answer is decided
/// above the command layer (`/exit`, `/stop`), and this enters below it, so
/// the tail can only ever say no.
fn submit_expanded_text(
    app: &mut AppState,
    text: String,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
) {
    let transcript_len_before_submit = app.rebon_tui.transcript.len();
    let cursor_offset_before_submit = app.cursor_offset;
    let quit = submit_after_command_dispatch(
        app,
        text,
        Vec::new(),
        None,
        session,
        handle,
        active_prompt,
        cursor_offset_before_submit,
        transcript_len_before_submit,
    );
    debug_assert!(!quit, "the submit path below the command layer cannot quit");
}

/// Everything that has to happen before the `command-registry` seat gets a
/// look at the line.
///
/// `/exit` and `/stop` are registered built-ins like any other, but they run
/// here rather than in `native_commands`: `/exit` has to precede the
/// stale-projection gate, `/stop` has to follow it, and both have to precede
/// the forwarding below. The mirror's `/mcp` and the session-control
/// forwarding are not commands at all — they are where a command *runs*, and
/// that question is settled before which command it is.
///
/// `Some(quit)` is the submit path's return value; `None` means carry on.
fn run_before_dispatch(
    app: &mut AppState,
    command: &str,
    session: &mut TuiEngineSession,
    active_prompt: &Option<ActivePrompt>,
    pending_permission: &mut Option<PendingPermission>,
    ui_mode: UiMode,
) -> Option<bool> {
    // Intercept "/exit" and "/quit" locally: force-quit the
    // application immediately, mirroring Ctrl-C exit flow.
    //
    // Attached sessions exit too. Hosted is the default, so the old
    // "attached means background it and stay open" branch caught every
    // typed `/exit`, kept the terminal running, and skipped the deliberate
    // lease release the event loop performs on the way out — ten more
    // minutes of a parked plugin and MCP stack, the exact cost the
    // deliberate-exit round was built to end. Keeping the session running
    // past this window is what `/bg` is for.
    if parse_exit_command(command) {
        save_to_history_for_session_if_needed(app, session, command);
        clear_input(app);
        let rebon_dialog::exit_flow::ExitFlowAction::Exit { message, .. } =
            rebon_dialog::exit_flow::resolve_exit(None, 0);
        tracing::info!("{message}");
        return Some(true);
    }
    if session
        .engine_half
        .projection_invalid
        .load(std::sync::atomic::Ordering::Acquire)
    {
        inject_system_message(
            app,
            "rewind-projection-invalid",
            "This session's canonical history changed, but its in-memory projection is stale. Restart or resume the session before sending another turn.",
        );
        return Some(false);
    }

    if parse_stop_command(command) {
        save_to_history_for_session_if_needed(app, session, command);
        clear_input(app);
        // A permission the worker was asking for dies with the worker.
        let remote_query_id = session
            .remote_background_attachment
            .as_ref()
            .and_then(|remote| remote.pending_permission_query_id);
        match stop_attached_background_session(app, session) {
            Ok(stopped) => {
                if let Some(active) = active_prompt.as_ref() {
                    active.cancel.cancel();
                }
                if remote_query_id.is_some()
                    && pending_permission
                        .as_ref()
                        .is_some_and(|pending| Some(pending.outbound.id) == remote_query_id)
                {
                    *pending_permission = None;
                }
                inject_local_command_feedback(app, "stop", &stopped.feedback);
                app.follow_transcript_tail = true;
            }
            Err(err) => {
                inject_system_message(app, "error", &format!("Cannot stop session: {err}"));
                app.follow_transcript_tail = true;
            }
        }
        return Some(false);
    }

    // A mirror's `/mcp` answers the way the host's would — the browser, or
    // the text in inline mode — over what the owner's status says its
    // servers are: they live over there, and this terminal hosts none.
    // Reconnecting or disconnecting one is still the owner's to do, and goes
    // over with the other session-control commands below.
    if matches!(parse_mcp_command(command), Some(Ok(McpCommand::Status))) {
        if let Some(snapshot) = session
            .remote_background_attachment
            .as_ref()
            .and_then(|remote| remote.owner_mcp.clone())
        {
            save_to_history_for_session_if_needed(app, session, command);
            clear_input(app);
            if ui_mode == UiMode::Inline {
                let output = crate::session::commands::mcp::format_mcp_status(snapshot);
                inject_local_command_feedback(app, "mcp", &output);
                app.follow_transcript_tail = true;
            } else {
                app.mcp_dialog = Some(McpDialogState::from_owner_snapshot(
                    &snapshot,
                    Path::new(&session.cwd),
                ));
            }
            return Some(false);
        }
    }

    // A RemoteProxy attachment mirrors a worker that owns the real engine.
    // Session-control commands have to run over there: left to fall through
    // they would execute here, against this process's shell session, and
    // report a context, cost or transcript belonging to nobody. Purely local
    // UI commands (/vim, /help, /theme) are not in the set and stay here.
    if let Some((job_id, endpoint, busy)) =
        session.remote_background_attachment.as_ref().map(|remote| {
            (
                remote.job_id.clone(),
                remote.endpoint(),
                remote.pending_command.is_some(),
            )
        })
    {
        if let Some((name, args)) =
            crate::session::commands::control::parse_session_control_command(command)
        {
            save_to_history_for_session_if_needed(app, session, command);
            clear_input(app);
            // Parked: the session is still the job's, and the command is
            // still the session's — but there is no engine to run it in
            // until a prompt gives the job a worker.
            let Some(endpoint) = endpoint else {
                inject_system_message(
                    app,
                    "error",
                    &format!(
                        "Worker {job_id} is stopped, so /{name} was not sent. Type a prompt to continue the session in a new worker first."
                    ),
                );
                app.follow_transcript_tail = true;
                return Some(false);
            };
            if busy {
                inject_system_message(
                    app,
                    "warning",
                    &format!(
                        "Still waiting on the previous command to answer — /{name} was not sent."
                    ),
                );
                app.follow_transcript_tail = true;
                return Some(false);
            }
            let receiver = crate::background::spawn_remote_session_command(
                &job_id,
                &endpoint,
                name.clone(),
                args,
            );
            if let Some(remote) = session.remote_background_attachment.as_mut() {
                remote.pending_command = Some(receiver);
            }
            inject_local_command_feedback(
                app,
                &name,
                &format!("Sent /{name} to the attached background session…"),
            );
            app.follow_transcript_tail = true;
            return Some(false);
        }
    }
    None
}

/// Put the prompt where the session actually lives, when that is not this
/// process.
///
/// `Some(false)` means the prompt was routed (or refused) and this terminal
/// is done with it; `None` means the session is this terminal's to run.
fn route_prompt_to_a_worker(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    text: &str,
    injected_images: &[rebon_types::PromptPasteContent],
    user_message_uuid: &Option<String>,
) -> Option<bool> {
    // Waiting for a worker's endpoint. What that costs the prompt depends on
    // why we are waiting.
    let pending_hosted = session
        .pending_hosted_session
        .as_ref()
        .map(|pending| (pending.kind, pending.job_id.clone()));
    let submitted_something = !text.trim().is_empty() || !injected_images.is_empty();
    match pending_hosted {
        // Mid-handover this session has already been given away: its active
        // lock is gone, so a turn run *here* would write a transcript the
        // worker is about to own — two writers, one file. But there is
        // somewhere else to put the prompt, and there always was: the job the
        // session went to. A pending prompt is durable and the worker claims
        // it as it starts, so the words wait in the one place the session
        // will actually look, and the user is never told to sit still.
        //
        // A reattach is the same: the job is where the session lives, and
        // its next worker claims the prompt as it starts.
        Some((kind, job_id)) if kind.session_is_the_jobs() => {
            save_to_history_for_session_if_needed(app, session, text);
            let Some(mut submit) =
                take_submit_payload_with_images(app, text, injected_images.to_vec())
            else {
                return Some(false);
            };
            if user_message_uuid.is_some() {
                submit.user_message_uuid = user_message_uuid.clone();
            }
            let message = submit.prompt_text().to_string();
            let images = submit
                .image_pastes
                .iter()
                .cloned()
                .map(rebon_session_host::BackgroundImageAttachment::from_prompt_paste_content)
                .collect::<Vec<_>>();
            match crate::background::reply_to_background_job_with_images(&job_id, message, images) {
                Ok(()) => {
                    // Echoed straight away: the prompt is durable now, and a
                    // composer that swallowed it while the handover finished
                    // would look like a dropped keystroke.
                    commit_submit_payload_to_transcript(app, &mut submit, &session.session_id);
                    app.is_loading = true;
                    repin_transcript_to_bottom(app);
                }
                Err(err) => {
                    app.input = text.to_string();
                    app.cursor_offset = app.input.len();
                    app.pasted_contents = submit.image_pastes;
                    inject_system_message(
                        app,
                        "error",
                        &format!(
                            "This session is moving into worker {job_id} and the prompt could not be queued for it: {err}"
                        ),
                    );
                    app.follow_transcript_tail = true;
                }
            }
            return Some(false);
        }
        // A dispatched job was never this session's — it runs on its own and
        // this is only a mirror that has not arrived. Sending something here
        // says the user would rather work than watch, so give up the mirror
        // instead of making them wait out a process start. The job is
        // untouched.
        Some((_, job_id)) if submitted_something => {
            session.pending_hosted_session = None;
            session.attached_background_job_id = None;
            inject_system_message(
                app,
                "notice",
                &format!(
                    "Stopped waiting to mirror {job_id} — it keeps running on its own. Open it from Agent View, or `rebon attach {job_id}`."
                ),
            );
        }
        // An empty Enter is not a decision. Keep waiting.
        Some(_) => return Some(false),
        None => {}
    }

    if let Some((job_id, live)) = session
        .remote_background_attachment
        .as_ref()
        .map(|remote| (remote.job_id.clone(), remote.is_live()))
    {
        save_to_history_for_session_if_needed(app, session, text);
        let Some(mut submit) = take_submit_payload_with_images(app, text, injected_images.to_vec())
        else {
            return Some(false);
        };
        if user_message_uuid.is_some() {
            submit.user_message_uuid = user_message_uuid.clone();
        }
        let message = submit.prompt_text().to_string();
        let images = submit
            .image_pastes
            .iter()
            .cloned()
            .map(rebon_session_host::BackgroundImageAttachment::from_prompt_paste_content)
            .collect::<Vec<_>>();
        // The prompt goes on the job either way: a live worker claims it
        // from there, and a job without one is given a worker that will.
        // Queued first, so that the worker started for it has something to
        // run rather than parking on the session. A live worker is told
        // over IPC, which is what wakes it; the record is written directly
        // when it does not answer.
        let live_endpoint = session
            .remote_background_attachment
            .as_ref()
            .and_then(|remote| remote.endpoint());
        let queued = match live_endpoint {
            Some(endpoint) => crate::background::reply_to_live_worker(
                &session.session_id,
                &job_id,
                &endpoint,
                message.clone(),
                images.clone(),
            )
            .or_else(|err| {
                tracing::debug!(%job_id, %err, "live reply refused; queuing on the job record");
                crate::background::reply_to_background_job_with_images(&job_id, message, images)
            }),
            None => {
                crate::background::reply_to_background_job_with_images(&job_id, message, images)
            }
        };
        if let Err(err) = queued {
            app.input = text.to_string();
            app.cursor_offset = app.input.len();
            app.pasted_contents = submit.image_pastes;
            inject_system_message(
                app,
                "error",
                &format!("Failed to send prompt to background session {job_id}: {err}"),
            );
            app.follow_transcript_tail = true;
            return Some(false);
        }
        if !live {
            revive_parked_session(app, session, &job_id);
        }
        if let Some(remote) = session.remote_background_attachment.as_mut() {
            if remote.turn_is_terminal() {
                remote.status = rebon_session_host::BackgroundJobStatus::Queued;
            }
            remote.terminal_transcript_synced = false;
            // The clock starts on Enter, as a local session's does. The
            // owner announces the turn when it claims the prompt, and that
            // announcement keeps this instant rather than restarting it.
            remote.begin_turn(std::time::Instant::now(), false);
        }
        commit_submit_payload_to_transcript(app, &mut submit, &session.session_id);
        app.is_loading = true;
        app.prompt_completion_status = None;
        repin_transcript_to_bottom(app);
        return Some(false);
    }
    None
}

pub(super) fn set_goal_and_start(
    app: &mut AppState,
    command_text: &str,
    prompt: String,
    max_sessions: Option<u32>,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
) {
    let output = set_goal(app, prompt.clone(), max_sessions);
    inject_local_command_feedback_with_command(app, "goal", command_text, &output);
    start_goal_prompt_for_goal(app, &prompt, session, handle, active_prompt);
    app.follow_transcript_tail = true;
}

fn set_refined_goal_and_start(
    app: &mut AppState,
    prompt: String,
    max_sessions: Option<u32>,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
) {
    let confirmation = build_goal_refinement_confirmation(&prompt);
    let output = set_goal(app, prompt.clone(), max_sessions);
    let feedback = format!("{confirmation}\n{output}");
    inject_local_command_feedback_with_command(app, "goal", &format!("/goal {prompt}"), &feedback);
    start_goal_prompt_for_goal(app, &prompt, session, handle, active_prompt);
    app.follow_transcript_tail = true;
}

fn start_goal_prompt_for_goal(
    app: &mut AppState,
    prompt: &str,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
) {
    let goal_model_prompt = build_goal_continuation_prompt(prompt, None, Some(prompt), None);
    start_goal_prompt(
        app,
        prompt,
        &goal_model_prompt,
        session,
        handle,
        active_prompt,
    );
}

pub(in crate::tui::runner) fn prepare_goal_clarification_if_needed(
    app: &mut AppState,
    command_text: &str,
    prompt: &str,
    max_sessions: Option<u32>,
) -> bool {
    if !goal_needs_clarification(prompt) {
        return false;
    }
    app.pending_goal_clarification = Some(PendingGoalClarification::new(prompt, max_sessions));
    app.deferred_goal_submit_payloads.clear();
    inject_local_command_feedback_with_command(
        app,
        "goal",
        command_text,
        &build_goal_clarification_request(prompt),
    );
    app.follow_transcript_tail = true;
    true
}

fn start_goal_prompt(
    app: &mut AppState,
    transcript_prompt: &str,
    model_prompt: &str,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
) {
    let transcript_len_before_goal_prompt = app.rebon_tui.transcript.len();
    tracing::info!(goal = %transcript_prompt, "rebon-cli: starting newly set goal");
    submit_model_text(
        app,
        transcript_prompt,
        Some(model_prompt.to_string()),
        session,
        handle,
        active_prompt,
        transcript_prompt.len(),
        transcript_len_before_goal_prompt,
    );
}

/// A prompt was typed into a session whose worker is gone: give the job a
/// worker, and wait to mirror it in place.
///
/// This is `rebon attach` on a stopped job, from inside the session: the
/// explicit act that a passive mirror never performs on its own. The prompt
/// is already on the job, so the worker started here runs it rather than
/// parking. If somebody else got there first and a worker is already up,
/// it is followed instead.
fn revive_parked_session(app: &mut AppState, session: &mut TuiEngineSession, job_id: &str) {
    match crate::background::attach_background_job(job_id) {
        Ok(target) if target.mode == crate::background::BackgroundAttachMode::RemoteProxy => {
            if let (Some(remote), Some(endpoint)) = (
                session.remote_background_attachment.as_mut(),
                target.remote_endpoint,
            ) {
                remote.link_worker(endpoint);
            }
            inject_local_command_feedback(app, "attach", &format!("Attached to worker {job_id}."));
        }
        Ok(_) => {
            session.pending_hosted_session = Some(
                crate::background::PendingHostedSession::reattach(job_id.to_string(), true),
            );
            inject_local_command_feedback(
                app,
                "attach",
                &format!("Starting a new worker for {job_id} — the prompt runs once it is up…"),
            );
        }
        Err(err) => {
            // The prompt is on the record; the next worker, however it is
            // started, runs it. Say what failed here.
            inject_system_message(
                app,
                "error",
                &format!(
                    "The prompt was queued on {job_id}, but no worker could be started for it: {err}. Type again to retry, or `rebon attach {job_id}` from another terminal."
                ),
            );
        }
    }
    app.follow_transcript_tail = true;
}

/// Whether this session's engine is somewhere other than this process.
///
/// True while a worker owns the session (mirrored attachment, live or
/// parked) and while it is being handed to one. Any command that would
/// otherwise start a **local model turn** has to consult this: the
/// transcript belongs to whoever owns the session, and a turn run here
/// would write the file that owner is writing.
///
/// Commands that only read local UI state (`/help`, `/theme`) are unaffected
/// — those never touch the transcript. Commands that are model turns in
/// disguise (`/review`) fall through to the routing below instead of
/// spawning locally.
pub(super) fn session_engine_lives_elsewhere(session: &TuiEngineSession) -> bool {
    session.remote_background_attachment.is_some() || session_is_mid_handover(session)
}

/// Whether a `/hosted` handover for **this** session is in flight.
///
/// Only a handover counts. The other pending kind is a dispatch — a job
/// started on its own that this terminal is merely waiting to mirror — and
/// that session is still ours, still holding its lock, still perfectly able
/// to run a turn. Treating the two alike refused work on a session nothing
/// had taken.
fn session_is_mid_handover(session: &TuiEngineSession) -> bool {
    session
        .pending_hosted_session
        .as_ref()
        .is_some_and(|pending| pending.kind.session_is_the_jobs())
}

/// Refuse a slash command that would start **local** work on a session whose
/// engine is not in this process, and say why. Returns `true` when the
/// command was refused.
///
/// Two kinds of command land here. Some are model turns in disguise
/// (`/ceo <task>`, `/ultraplan`, `/goal`, `/permissions retry`, `/compact`):
/// they flip local state first and then run against this process's engine,
/// so unlike `/review` they cannot simply be handed to the worker, and
/// running them would put a second writer on the transcript that worker
/// owns. The others (`/run`, `/agent`) start work in *this* terminal while
/// attributing it to a session that lives elsewhere — work the session will
/// never see and that dies when this terminal closes.
///
/// A plain `!command` is deliberately not in this set: it runs a shell in
/// this terminal, says so, writes nothing to the session, and leaves
/// nothing behind.
pub(super) fn refuse_local_turn_if_session_is_elsewhere(
    app: &mut AppState,
    session: &TuiEngineSession,
    command: &str,
) -> bool {
    let where_it_runs = if let Some(remote) = session.remote_background_attachment.as_ref() {
        if remote.is_live() {
            format!("runs in background worker {}", remote.job_id)
        } else {
            format!(
                "belongs to background worker {}, which is stopped",
                remote.job_id
            )
        }
    } else if let Some(pending) = session
        .pending_hosted_session
        .as_ref()
        .filter(|_| session_is_mid_handover(session))
    {
        format!("is moving into background worker {}", pending.job_id)
    } else {
        return false;
    };
    inject_system_message(
        app,
        "error",
        &format!(
            "{command} runs in this terminal, but this session {where_it_runs}. Run it in a session this terminal owns."
        ),
    );
    app.follow_transcript_tail = true;
    true
}

pub(super) fn submit_model_text(
    app: &mut AppState,
    prompt: &str,
    model_prompt: Option<String>,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    cursor_offset_before_submit: usize,
    transcript_len_before_submit: usize,
) {
    if active_prompt.is_some() {
        let Some(mut submit) = take_submit_payload(app, prompt) else {
            return;
        };
        submit.model_text = model_prompt;
        if !attach_skill_invocation_if_registered(app, session, &mut submit) {
            return;
        }
        maybe_prepare_implicit_ultraplan_submit(app, session, &mut submit);
        maybe_prepare_implicit_ultrawork_submit(app, &mut submit);
        app.is_loading = true;
        enqueue_submit_payload(app, submit);
        repin_transcript_to_bottom(app);
        return;
    }

    prepare_for_new_prompt_after_withdrawal(app, &mut session.engine_half.update_rx);
    let Some(mut submit) = apply_submit(app, prompt, &session.session_id) else {
        return;
    };
    submit.model_text = model_prompt;
    if !attach_skill_invocation_if_registered(app, session, &mut submit) {
        return;
    }
    maybe_prepare_implicit_ultraplan_submit(app, session, &mut submit);
    maybe_prepare_implicit_ultrawork_submit(app, &mut submit);
    repin_transcript_to_bottom(app);
    let withdrawable = live_input_withdrawable_submit_from_payload(
        &submit,
        cursor_offset_before_submit,
        transcript_len_before_submit,
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

pub(super) fn execute_ultraplan_manual_review_command(
    app: &mut AppState,
    session: &TuiEngineSession,
) -> String {
    let Some(status) = app.ultraplan_status.as_ref() else {
        return "No active /ultraplan run to review.".into();
    };
    let Some(state) =
        rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, &status.run_id)
    else {
        return format!(
            "Active /ultraplan run {} could not be loaded.",
            status.run_id
        );
    };
    let Some(plan) = state
        .last_plan_draft
        .as_deref()
        .filter(|plan| !plan.trim().is_empty())
    else {
        return "No /ultraplan draft has been submitted yet, so there is nothing to review.".into();
    };
    // The review is on-demand and advisory, so the current persisted draft is
    // enough; no release or confirmation handshake gates it.
    let Some(plan_hash) = state.plan_hash.as_deref() else {
        return "The active /ultraplan run has no plan hash for its draft yet.".into();
    };
    match start_review(session, &state, plan, plan_hash, ReviewKind::Manual) {
        Ok(()) => format!("Started manual /ultraplan review for plan hash {plan_hash}."),
        Err(err) => format!("Could not start manual /ultraplan review: {err}"),
    }
}

pub(super) fn format_ultraplan_run_list(session: &TuiEngineSession) -> String {
    let runs = rebon_session::list_ultraplan_runs(&session.projects_root, &session.cwd);
    if runs.is_empty() {
        return "No ultraplan runs found for this project.".into();
    }
    let mut output = String::from("Ultraplan runs:\n");
    for run in runs {
        output.push_str(&format!(
            "- {} | {} | phase {:?} | round {} | updated {}\n",
            run.run_id, run.task, run.phase, run.round, run.updated_at_ms
        ));
    }
    output.trim_end().to_string()
}

pub(super) fn prepare_ultraplan_resume_submit(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    run_id: &str,
) -> Option<crate::session::submit_payload::SubmitPayload> {
    let Some(mut state) =
        rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
    else {
        inject_system_message(
            app,
            "local_command",
            &format!("/ultraplan resume error: run {run_id} was not found."),
        );
        app.follow_transcript_tail = true;
        return None;
    };
    if !state.is_active() {
        inject_system_message(
            app,
            "local_command",
            &format!(
                "/ultraplan resume error: run {run_id} is not active (phase {:?}).",
                state.phase
            ),
        );
        app.follow_transcript_tail = true;
        return None;
    }
    state.attach_session(session.session_id.clone());
    state.updated_at_ms = rebon_types::wall_clock_ms_u128() as u64;
    let outcome = restore_ultraplan_run_for_runtime(app, &mut state);
    if !outcome.restored {
        inject_system_message(
            app,
            "local_command",
            &format!("/ultraplan resume error: run {run_id} is not in a resumable phase."),
        );
        app.follow_transcript_tail = true;
        return None;
    }
    state.prepare_for_persist();
    if let Err(diagnostic) =
        crate::session::ultraplan_preflight::preflight_ultraplan_run(session, &mut state)
    {
        inject_system_message(
            app,
            "local_command",
            &format!("/ultraplan resume preflight failed: {diagnostic}"),
        );
        app.follow_transcript_tail = true;
        return None;
    }
    persist_ultraplan_run(session, &state);

    let mut submit = apply_submit(
        app,
        &format!("/ultraplan --resume {run_id}"),
        &session.session_id,
    )?;
    submit.model_text = Some(
        outcome
            .reconcile_prompt
            .unwrap_or_else(|| format_ultraplan_restore_prompt(&state, None)),
    );
    if let Some(phase) = status_phase_from_run_phase(state.phase) {
        if !matches!(
            phase,
            crate::session::ultraplan_run::UltraplanPhase::Executing
        ) {
            submit.execution_policy =
                ultraplan_context_for_run_state(&state, phase).map(ExecutionPolicy::ultraplan);
        }
    }
    Some(submit)
}

pub(super) fn parse_ultrawork_workflow_command(text: &str) -> Option<(&'static str, String)> {
    // Both spellings, and neither is case-sensitive — the rest of the submit
    // path stopped being so, and `/Ultrawork` losing its workflow policy in
    // silence is exactly the failure that costs the most to notice.
    for prefix in ["/ultrawork", "/ulw"] {
        let Some(rest) = text
            .get(..prefix.len())
            .filter(|typed| typed.eq_ignore_ascii_case(prefix))
            .map(|_| &text[prefix.len()..])
        else {
            continue;
        };
        if rest.is_empty() {
            return Some((prefix, String::new()));
        }
        if rest.starts_with(' ') {
            return Some((prefix, rest.trim().to_string()));
        }
    }
    None
}

pub(super) fn prepare_ultrawork_workflow_submit(
    app: &mut AppState,
    request: &str,
    session_id: &str,
) -> Option<crate::session::submit_payload::SubmitPayload> {
    let model_request =
        crate::tui::dispatch::expand_paste_references(request, &app.pasted_contents);
    let mut submit = apply_submit(app, request, session_id)?;
    submit.model_text = Some(ultrawork_workflow_model_text(&model_request));
    promote_workflow_tool(&mut submit);
    Some(submit)
}

fn maybe_prepare_implicit_ultrawork_submit(
    app: &mut AppState,
    submit: &mut crate::session::submit_payload::SubmitPayload,
) {
    if submit.model_text.is_some() || !has_triggerable_ultrawork_keyword(&submit.text) {
        return;
    }
    submit.model_text = Some(ultrawork_workflow_model_text(&submit.text));
    promote_workflow_tool(submit);
    app.follow_transcript_tail = true;
}

fn promote_workflow_tool(submit: &mut crate::session::submit_payload::SubmitPayload) {
    submit.execution_policy = Some(ExecutionPolicy::workflow_controller());
}

fn ultrawork_workflow_model_text(request: &str) -> String {
    format!(
        "<system-reminder>\nThe user explicitly requested ultrawork workflow orchestration. Use the Workflow tool to fulfill this request. This opt-in is a quality dial, not just permission: aim for the most exhaustive, correct result the task allows, and lean toward adversarially verifying findings rather than trusting a single pass. Scout first with Read/Glob/Grep when you need a work-list, then orchestrate. For research/review/audit-style requests, include a verification stage — adversarial verifiers prompted to REFUTE each finding, a multi-lens judge vote, or a completeness critic — and scale it to the request: a quick check earns a single-vote verify, a thorough audit earns a 3-5 vote adversarial pass plus a synthesis stage. Workflow waits for completion and returns the final result directly; do not call Sleep, poll task state, or read workflow journal files after Workflow unless the tool result reports an error requiring artifact inspection. Keep workflow meta as a static object literal only, with no functions or computed values.\n</system-reminder>\n\n{request}"
    )
}

pub(super) fn current_session_is_blank(app: &AppState, session: &TuiEngineSession) -> bool {
    if !app.rebon_tui.transcript.is_empty() || !app.rebon_tui.overlay.is_empty() {
        return false;
    }

    session
        .engine_half
        .handler
        .state()
        .get_session(&session.session_id)
        .is_some_and(|record| record.messages.is_empty() && record.loaded_transcript.is_empty())
}

fn parse_bang_shell_command(mode: &str, text: &str) -> Option<(String, String)> {
    if mode == "bash" {
        let cmd = text
            .strip_prefix('!')
            .unwrap_or(text)
            .trim_start()
            .to_string();
        let history_text = format!("!{cmd}");
        return Some((cmd, history_text));
    }

    let cmd = text.strip_prefix('!')?.trim_start().to_string();
    let history_text = format!("!{cmd}");
    Some((cmd, history_text))
}

fn disabled_skill_command_message(session: &TuiEngineSession, text: &str) -> Option<String> {
    let (name, _) = parse_user_skill_invocation(text)?;
    if !session.engine_half.skill_registry.is_disabled(&name)
        || !session
            .engine_half
            .skill_registry
            .all_entries()
            .iter()
            .any(|skill| skill.id == name)
    {
        return None;
    }

    Some(format!(
        "Skill `/{name}` is disabled. Use `/skills` to re-enable it."
    ))
}

fn slash_command_typo_message(
    app: &AppState,
    session: &TuiEngineSession,
    text: &str,
) -> Option<String> {
    let (name, _) = parse_user_skill_invocation(text)?;
    if app
        .slash_commands
        .iter()
        .any(|command| command.matches_name_or_alias(&name))
        || session.engine_half.skill_registry.get(&name).is_some()
    {
        return None;
    }

    let suggestion = closest_slash_command_name(&name, &app.slash_commands)?;
    Some(format!(
        "Unknown slash command `/{name}`. Did you mean `/{suggestion}`?"
    ))
}

fn closest_slash_command_name(
    name: &str,
    commands: &[rebon_types::SlashCommand],
) -> Option<String> {
    let name = name.to_ascii_lowercase();
    commands
        .iter()
        .flat_map(|command| {
            std::iter::once(command.name.as_str()).chain(command.aliases.iter().map(String::as_str))
        })
        .filter_map(|candidate| {
            let distance = slash_command_edit_distance(&name, &candidate.to_ascii_lowercase());
            let max_distance = match name.len().max(candidate.len()) {
                0..=4 => 1,
                5..=8 => 2,
                _ => 3,
            };
            (distance <= max_distance).then(|| (distance, candidate.len(), candidate.to_string()))
        })
        .min_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        })
        .map(|(_, _, candidate)| candidate)
}

fn slash_command_edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];

    for (left_index, left_byte) in left.bytes().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_byte) in right.bytes().enumerate() {
            current[right_index + 1] = (current[right_index] + 1)
                .min(previous[right_index + 1] + 1)
                .min(previous[right_index] + usize::from(left_byte != right_byte));
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[right.len()]
}

fn attach_skill_invocation_if_registered(
    app: &mut AppState,
    session: &TuiEngineSession,
    submit: &mut crate::session::submit_payload::SubmitPayload,
) -> bool {
    let Some((skill, args)) = parse_user_skill_invocation(&submit.text) else {
        return true;
    };
    if session.engine_half.skill_registry.is_disabled(&skill) {
        inject_system_message(
            app,
            "error",
            &format!("Skill `/{skill}` is disabled. Use `/skills` to re-enable it."),
        );
        app.follow_transcript_tail = true;
        return false;
    }
    let Some(definition) = session.engine_half.skill_registry.get(&skill) else {
        return true;
    };
    if !definition.user_invocable {
        inject_system_message(
            app,
            "error",
            &format!(
                "This skill can only be invoked by the model, not directly by users. Ask Rebon to use the \"{skill}\" skill for you."
            ),
        );
        app.follow_transcript_tail = true;
        return false;
    }
    submit
        .skill_invocations
        .push(PendingSkillInvocation { skill, args });
    true
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rebon_tui::input::VimMode;
    use tokio::runtime::{Builder, Handle, Runtime};
    use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
    use tokio::sync::oneshot;

    use crate::session::ultraplan_run::UltraplanPhase;
    use crate::tui::app::AppState;
    use crate::tui::wiring::TuiEngineSession;
    use crate::ui_config::UiMode;
    use rebon_types::{
        ContentBlock, PromptCancel, RunPhase, SessionUpdate, SessionUpdateParams, TextContent,
        UltraplanProfile, UltraplanRunState,
    };
    use tempfile::TempDir;

    use super::super::dialog_keys::handle_goal_confirm_replace;
    use super::super::test_support::{
        insert_local_agent_task, make_test_tui_session, make_ultraplan_test_tui_session,
        push_test_user_message, RuntimeModeEnvGuard,
    };
    use super::super::{ActivePrompt, LocalTurnSource};
    use super::{
        drain_command_expansion, parse_user_skill_invocation, prepare_ultraplan_resume_submit,
        spawn_command_expansion, submit_or_queue, submit_or_queue_with_images_and_uuid,
    };

    fn make_immediate_handle() -> (Runtime, Handle) {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let handle = runtime.handle().clone();
        (runtime, handle)
    }

    #[derive(Default)]
    struct RecordingPromptExecutor {
        requests: Mutex<Vec<rebon_agent_core::PromptRequest>>,
    }

    #[async_trait::async_trait]
    impl rebon_agent_core::PromptExecutor for RecordingPromptExecutor {
        async fn execute(
            &self,
            request: rebon_agent_core::PromptRequest,
        ) -> Result<rebon_agent_core::PromptOutcome, rebon_agent_core::PromptExecutorError>
        {
            self.requests.lock().expect("requests lock").push(request);
            Ok(rebon_agent_core::PromptOutcome::end_turn())
        }
    }

    impl RecordingPromptExecutor {
        fn take_requests(&self) -> Vec<rebon_agent_core::PromptRequest> {
            std::mem::take(&mut *self.requests.lock().expect("requests lock"))
        }
    }

    fn prompt_request_text(request: &rebon_agent_core::PromptRequest) -> &str {
        match &request.prompt[0] {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("expected text prompt, got {other:?}"),
        }
    }

    fn submit_compact(
        app: &mut AppState,
        session: &mut TuiEngineSession,
        handle: &Handle,
        active_prompt: &mut Option<ActivePrompt>,
        text: &str,
    ) {
        app.input = text.into();
        app.cursor_offset = app.input.len();
        assert!(!submit_or_queue(
            app,
            text.into(),
            session,
            handle,
            active_prompt,
            &mut None,
            UiMode::Screen,
        ));
    }

    fn system_transcript_text(app: &AppState) -> String {
        app.rebon_tui
            .transcript
            .rows()
            .iter()
            .filter_map(|row| match row {
                rebon_tui::Message::System(system) => system.content.clone(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `/compact` used to only arm the *next* model request, so nothing
    /// happened until the user sent another prompt. At an idle prompt it now
    /// runs immediately, which is what the progress widget renders.
    #[test]
    fn compact_at_an_idle_prompt_starts_a_run_instead_of_arming_the_next_request() {
        let (_runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let mut app = AppState::new();
        let mut active_prompt = None;

        submit_compact(
            &mut app,
            &mut session,
            &handle,
            &mut active_prompt,
            "/compact",
        );

        assert!(app.compact_run.is_some(), "the run must be under way");
        assert!(session.engine_half.compact_runtime.is_running());
        assert!(app.input.is_empty());
        assert!(
            !system_transcript_text(&app).contains("next model request"),
            "the deferred wording must not be printed for an immediate run"
        );
    }

    /// Mid-turn the engine's in-turn manual path owns the live context
    /// manager. Starting a second compaction against the same history would
    /// race it, so the command stays deferred — and says so.
    #[test]
    fn compact_during_a_turn_falls_back_to_arming_the_next_request() {
        let (_runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let mut app = AppState::new();
        let (_tx, rx) = oneshot::channel();
        let mut active_prompt = Some(ActivePrompt::new(rx, PromptCancel::new()));

        submit_compact(
            &mut app,
            &mut session,
            &handle,
            &mut active_prompt,
            "/compact keep the failures",
        );

        assert!(app.compact_run.is_none(), "no immediate run during a turn");
        assert!(!session.engine_half.compact_runtime.is_running());
        let rendered = system_transcript_text(&app);
        assert!(rendered.contains("next model request"), "{rendered}");
        assert!(rendered.contains("keep the failures"), "{rendered}");
    }

    fn expansion_args(
        raw: &str,
        rest: &str,
    ) -> rebon_kernel_seats::kernel_core_commands::CommandArgs {
        rebon_kernel_seats::kernel_core_commands::CommandArgs {
            raw: raw.to_string(),
            rest: rest.to_string(),
            surface: rebon_slash_commands::Surface::Tui,
        }
    }

    /// A `Prompt` command may be a plugin's, and asking a plugin what a line
    /// expands to is a call to another process. The submit path hands it to a
    /// worker thread and returns immediately; the pass of the loop that finds
    /// the answer is the one that submits it.
    #[test]
    fn a_prompt_command_expands_off_the_loop_and_is_submitted_when_it_lands() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let (tx, mut rx) = unbounded_channel();
        app.command_expansion_tx = Some(tx.clone());
        app.input = "/slow tea".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        spawn_command_expansion(
            &mut app,
            &handle,
            "slow",
            "/slow tea",
            Arc::new(
                |args: &rebon_kernel_seats::kernel_core_commands::CommandArgs| {
                    Ok(format!("please make {}", args.rest))
                },
            ),
            expansion_args("/slow tea", "tea"),
            tx,
        );

        assert!(
            app.input.is_empty(),
            "the line is on its way, so the input is cleared"
        );
        assert_eq!(
            app.expanding_command
                .as_ref()
                .map(|pending| pending.name.as_str()),
            Some("slow")
        );
        assert!(system_transcript_text(&app).contains("Expanding /slow"));
        assert!(
            active_prompt.is_none(),
            "nothing is sent while the expansion runs"
        );
        assert!(recorder.take_requests().is_empty());

        // What the loop does every pass while the worker thread expands.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.expanding_command.is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "the expansion never reached the loop"
            );
            std::thread::yield_now();
            drain_command_expansion(&mut app, &mut session, &handle, &mut active_prompt, &mut rx);
        }
        runtime.block_on(tokio::task::yield_now());

        assert!(
            active_prompt.is_some(),
            "the expansion is submitted once it lands"
        );
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(prompt_request_text(&requests[0]), "please make tea");

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    /// An expansion that failed is a sentence for the person, not prompt
    /// text. The loop shows it and sends nothing — the same answer the
    /// inline path gives, arriving a few frames later.
    #[test]
    fn an_expansion_that_failed_is_shown_and_not_sent() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let (tx, mut rx) = unbounded_channel();
        app.command_expansion_tx = Some(tx.clone());
        app.expanding_command = Some(crate::tui::app::ExpandingCommand {
            id: 7,
            name: "slow".into(),
            line: "/slow tea".into(),
        });
        let mut active_prompt = None;

        tx.send((
            7,
            Err("/slow failed: the plugin did not answer".to_string()),
        ))
        .expect("the loop is listening");
        drain_command_expansion(&mut app, &mut session, &handle, &mut active_prompt, &mut rx);
        runtime.block_on(tokio::task::yield_now());

        let rendered = system_transcript_text(&app);
        assert!(rendered.contains("the plugin did not answer"), "{rendered}");
        assert!(active_prompt.is_none(), "a failure is not a prompt");
        assert!(recorder.take_requests().is_empty());
        assert!(
            app.expanding_command.is_none(),
            "the wait is over either way"
        );
        runtime.shutdown_background();
    }

    /// Giving up on an expansion does not cancel it: the call finishes on its
    /// own time and answers to a prompt that has moved on. The answer is
    /// matched to the wait that asked for it, so the abandoned one is dropped
    /// rather than submitted as whatever was typed next.
    #[test]
    fn an_abandoned_expansions_late_answer_is_not_submitted_as_the_next_commands() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let (tx, mut rx) = unbounded_channel();
        app.command_expansion_tx = Some(tx.clone());
        let mut active_prompt = None;

        // `/slow`, then Esc before it answers.
        spawn_command_expansion(
            &mut app,
            &handle,
            "slow",
            "/slow tea",
            Arc::new(
                |_: &rebon_kernel_seats::kernel_core_commands::CommandArgs| {
                    Ok("brew tea slowly".to_string())
                },
            ),
            expansion_args("/slow tea", "tea"),
            tx.clone(),
        );
        let abandoned = app
            .expanding_command
            .take()
            .expect("the wait Esc gives up on");

        // `/fast`, which is now the wait in flight.
        spawn_command_expansion(
            &mut app,
            &handle,
            "fast",
            "/fast coffee",
            Arc::new(
                |_: &rebon_kernel_seats::kernel_core_commands::CommandArgs| {
                    Ok("pour coffee".to_string())
                },
            ),
            expansion_args("/fast coffee", "coffee"),
            tx.clone(),
        );
        let in_flight = app
            .expanding_command
            .as_ref()
            .expect("the wait that replaced it")
            .id;
        assert_ne!(abandoned.id, in_flight);

        // The abandoned command answers first, then the one in flight.
        tx.send((abandoned.id, Ok("brew tea slowly".to_string())))
            .expect("the loop is listening");
        tx.send((in_flight, Ok("pour coffee".to_string())))
            .expect("the loop is listening");
        drain_command_expansion(&mut app, &mut session, &handle, &mut active_prompt, &mut rx);
        runtime.block_on(tokio::task::yield_now());

        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1, "one submit, not two");
        assert_eq!(
            prompt_request_text(&requests[0]),
            "pour coffee",
            "the abandoned expansion must not be submitted as /fast"
        );
        assert!(app.expanding_command.is_none());

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    fn assert_slash_prompt_submitted_verbatim(text: &str) {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        app.slash_commands = rebon_slash_commands::for_surface(rebon_slash_commands::Surface::Tui);
        app.input = text.into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            text.into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        assert!(app.input.is_empty());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(prompt_request_text(&requests[0]), text);
        assert!(requests[0].skill_invocations.is_empty());
        assert!(!app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(system)
                if system.subtype == "error"
                    && system
                        .content
                        .as_deref()
                        .is_some_and(|content| content.contains("Unknown slash command"))
        )));

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    fn assert_goal_prompt(prompt_text: &str, goal: &str) {
        assert!(prompt_text.contains(&format!("<goal>\n{goal}\n</goal>")));
        assert!(prompt_text.contains(&format!("Next requested step:\n{goal}")));
        assert!(prompt_text.contains("Completion audit before stopping"));
    }

    fn visible_prompt_text(app: &AppState) -> Option<&str> {
        let Some(rebon_tui::Message::User(message)) = app.rebon_tui.transcript.rows().last() else {
            return None;
        };
        message
            .message
            .content
            .iter()
            .find_map(|block| match block {
                rebon_tui::UserContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
    }

    fn send_text_chunk(tx: &UnboundedSender<SessionUpdateParams>, session: &str, text: &str) {
        tx.send(SessionUpdateParams {
            session_id: session.into(),
            update: SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(TextContent {
                    text: text.into(),
                    annotations: None,
                }),
            },
        })
        .unwrap();
    }

    #[test]
    fn foreground_agent_terminal_release_restores_main_and_preserves_input() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Completed,
            false,
        );
        let reg = Arc::new(reg);
        app.tasks = reg.clone();
        session.engine_half.tasks = reg.clone();
        app.main_agent_view = Some(crate::tui::app::StoredTranscriptView::default());
        app.foregrounded_task_id = Some("agent-1".into());
        app.input = "follow up".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "follow up".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(active_prompt.is_none());
        assert_eq!(app.foregrounded_task_id, None);
        assert_eq!(app.input, "follow up");
        assert_eq!(app.cursor_offset, "follow up".len());
        let snapshot = reg
            .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"))
            .expect("agent task");
        assert!(snapshot.is_backgrounded);
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &snapshot.data else {
            panic!("expected local agent");
        };
        assert!(data.pending_messages.is_empty());
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(system)
                if system.subtype == "local_command"
                    && system
                        .content
                        .as_deref()
                        .unwrap_or("")
                        .contains("can no longer receive messages")
        )));
        runtime.shutdown_background();
    }

    #[test]
    fn active_prompt_blocks_session_swap_command_matrix() {
        let (runtime, handle) = make_immediate_handle();
        for command in ["/new", "/resume", "/rewind", "/kernel dsh", "/agent:local"] {
            let mut app = AppState::new();
            app.input = command.into();
            app.cursor_offset = app.input.len();
            let mut session = make_test_tui_session();
            let before_session_id = session.session_id.clone();
            let before_runtime = session.engine_half.runtime.clone();
            let before_agent = session.engine_half.runtime.session_agents.current_id();
            let (_tx, rx) = oneshot::channel();
            let mut active_prompt = Some(ActivePrompt::new(rx, PromptCancel::new()));

            assert!(!submit_or_queue(
                &mut app,
                command.into(),
                &mut session,
                &handle,
                &mut active_prompt,
                &mut None,
                UiMode::Screen,
            ));

            assert!(active_prompt.is_some(), "{command}");
            assert_eq!(session.session_id, before_session_id, "{command}");
            assert!(
                Arc::ptr_eq(&session.engine_half.runtime, &before_runtime),
                "{command}"
            );
            assert_eq!(
                session.engine_half.runtime.session_agents.current_id(),
                before_agent,
                "{command}"
            );
            assert!(app.resume_dialog.is_none(), "{command}");
            assert!(app.rewind_dialog.is_none(), "{command}");
            assert!(
                system_transcript_text(&app).contains("while a prompt is running"),
                "{command}: {}",
                system_transcript_text(&app)
            );
        }
        runtime.shutdown_background();
    }

    #[test]
    fn slash_new_on_blank_session_clears_input_without_creating_session() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        app.input = "/new".into();
        app.cursor_offset = app.input.len();
        let mut session = make_test_tui_session();
        let original_session_id = session.session_id.clone();
        let original_session_count = session.server_state.session_count();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/new".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert_eq!(session.session_id, original_session_id);
        assert_eq!(session.server_state.session_count(), original_session_count);
        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
        assert!(app.rebon_tui.transcript.is_empty());
        assert!(active_prompt.is_none());
        runtime.shutdown_background();
    }

    #[test]
    fn slash_new_on_nonblank_session_creates_fresh_session() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u1", "hello");
        app.input = "/new".into();
        app.cursor_offset = app.input.len();
        let mut session = make_test_tui_session();
        let original_session_id = session.session_id.clone();
        let original_session_count = session.server_state.session_count();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/new".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert_ne!(session.session_id, original_session_id);
        assert_eq!(
            session.server_state.session_count(),
            original_session_count + 1
        );
        assert!(app.input.is_empty());
        assert!(app.rebon_tui.transcript.is_empty());
        assert!(active_prompt.is_none());
        runtime.shutdown_background();
    }

    #[test]
    fn slash_help_opens_overlay_and_does_not_submit_prompt() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let mut app = AppState::new();
        app.input = "/help".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/help".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(app.help_open, "/help should open the help overlay");
        assert!(app.input.is_empty(), "/help should clear the prompt input");
        assert_eq!(app.cursor_offset, 0, "/help should reset the prompt cursor");
        assert!(
            active_prompt.is_none(),
            "/help should not spawn or queue a normal prompt"
        );
        assert!(
            app.queued_commands.is_empty(),
            "/help should not queue a normal prompt"
        );
        assert!(
            app.rebon_tui.transcript.rows().is_empty(),
            "/help should not commit a user prompt to the transcript"
        );
        drop(runtime);
    }

    #[test]
    fn slash_provider_opens_switcher_inline_and_screen() {
        // A bare `/provider` opens the lightweight switcher (not the form)
        // in BOTH inline and screen modes.
        for ui_mode in [UiMode::Screen, UiMode::Inline] {
            let (runtime, handle) = make_immediate_handle();
            let mut session = make_test_tui_session();
            let mut app = AppState::new();
            app.input = "/provider".into();
            app.cursor_offset = app.input.len();
            let mut active_prompt = None;

            assert!(!submit_or_queue(
                &mut app,
                "/provider".into(),
                &mut session,
                &handle,
                &mut active_prompt,
                &mut None,
                ui_mode,
            ));

            assert!(
                app.dialogs.top_id() == Some(rebon_ui_seat::ids::dialog::PROVIDER),
                "{ui_mode:?}: /provider opens the switcher"
            );
            assert!(
                app.onboarding_dialog.is_none(),
                "{ui_mode:?}: /provider does not open the full form"
            );
            assert!(
                app.input.is_empty(),
                "{ui_mode:?}: /provider clears the prompt input"
            );
            assert!(
                active_prompt.is_none(),
                "{ui_mode:?}: /provider does not spawn a normal prompt"
            );
            assert!(
                app.rebon_tui.transcript.rows().is_empty(),
                "{ui_mode:?}: /provider does not commit a user prompt"
            );
            drop(runtime);
        }
    }

    #[test]
    fn slash_provider_add_opens_form_in_add_mode() {
        // `/provider add` (no extra args) escalates straight to the full
        // provider form, opened in add mode, in both modes.
        for ui_mode in [UiMode::Screen, UiMode::Inline] {
            let (runtime, handle) = make_immediate_handle();
            let mut session = make_test_tui_session();
            let mut app = AppState::new();
            app.input = "/provider add".into();
            app.cursor_offset = app.input.len();
            let mut active_prompt = None;

            assert!(!submit_or_queue(
                &mut app,
                "/provider add".into(),
                &mut session,
                &handle,
                &mut active_prompt,
                &mut None,
                ui_mode,
            ));

            let dialog = app
                .onboarding_dialog
                .as_ref()
                .expect("/provider add opens the provider form");
            assert!(
                dialog.is_provider_panel(),
                "{ui_mode:?}: /provider add opens the focused provider panel"
            );
            assert!(
                !app.dialogs.is_open(),
                "{ui_mode:?}: /provider add does not open the switcher"
            );
            drop(runtime);
        }
    }

    #[test]
    fn slash_provider_subcommand_still_uses_text_surface() {
        // `/provider list` (and other subcommands) keep the text CRUD
        // surface instead of opening the panel.
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let mut app = AppState::new();
        app.input = "/provider list".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/provider list".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Inline,
        ));

        assert!(
            app.onboarding_dialog.is_none(),
            "/provider list should not open the panel"
        );
        assert!(
            app.rebon_tui
                .transcript
                .rows()
                .iter()
                .any(|row| matches!(row, rebon_tui::Message::System(_))),
            "/provider list should emit a text result"
        );
        drop(runtime);
    }

    #[test]
    fn slash_cost_is_local_feedback_and_doctor_opens_browser_not_prompts() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/cost".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));
        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert!(app.input.is_empty());
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(system)
                if system.subtype == "local_command"
                    && system.uuid.starts_with("s-cost-")
                    && system.content.as_deref().unwrap_or("").contains("USD estimate")
        )));

        assert!(!submit_or_queue(
            &mut app,
            "/doctor".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));
        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert_eq!(
            app.dialogs.top_id(),
            Some(rebon_ui_seat::ids::dialog::DOCTOR)
        );
        assert!(local_command_output(&app, "doctor").is_none());
        drop(runtime);
    }

    #[test]
    fn slash_review_base_submits_synthesized_prompt_not_literal_command() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        app.input = "/review --base main".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/review --base main".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(active_prompt.is_some());
        runtime.block_on(tokio::task::yield_now());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let request_text = prompt_request_text(&requests[0]);
        assert!(request_text.contains("strict code review"));
        assert!(request_text.contains("base branch `main`"));
        assert!(!request_text.contains("/review --base main"));
        drop(runtime);
    }

    #[test]
    fn slash_review_invalid_injects_error_not_prompt() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/review --base".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(system)
                if system.subtype == "error"
                    && system.content.as_deref().unwrap_or("").contains("Usage: /review --base <branch>")
        )));
        drop(runtime);
    }

    /// The launch fields a background job needs but this test does not care
    /// about.
    fn background_runtime_fields_for_test() -> rebon_session_host::BackgroundRuntimeFields {
        rebon_session_host::BackgroundRuntimeFields {
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

    /// `REBON_CONFIG_DIR` for the duration, so a test that reaches the real
    /// background store reaches a temporary one instead. Serialized on the
    /// crate's env lock, because the variable is process-global.
    struct HandoverStoreGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HandoverStoreGuard {
        fn set(dir: &std::path::Path) -> Self {
            let lock = crate::test_env::lock_env();
            let previous = std::env::var_os("REBON_CONFIG_DIR");
            std::env::set_var("REBON_CONFIG_DIR", dir);
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for HandoverStoreGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
        }
    }

    /// Mid-handover the session belongs to a worker that is still starting:
    /// its active lock is already gone, so a turn run *here* would write a
    /// transcript the worker is about to own. The prompt must never reach the
    /// local executor — and must not be thrown away either. It goes to the job
    /// the session went to, where the starting worker claims it, and it is
    /// echoed so the composer does not look like it swallowed a keystroke.
    #[test]
    fn a_prompt_sent_mid_handover_is_queued_for_the_worker_not_run_locally() {
        let (runtime, handle) = make_immediate_handle();
        let config_dir = TempDir::new().unwrap();
        let _config = HandoverStoreGuard::set(config_dir.path());
        let store = crate::background::cli_default_store();
        let mut job = store
            .create_job(
                "handover".into(),
                std::path::PathBuf::from("."),
                background_runtime_fields_for_test(),
            )
            .unwrap();
        // A handover hands an existing session to the job, which is what makes
        // it a place a prompt can wait.
        job.identity.session_id = Some("sess-handover".into());
        store.write_state(&job).unwrap();

        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        session.pending_hosted_session = Some(crate::background::PendingHostedSession::handover(
            job.identity.job_id.clone(),
        ));
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "keep working on the parser".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(
            recorder.take_requests().is_empty(),
            "the local engine must not run a turn for a session being handed over"
        );
        assert!(active_prompt.is_none());
        assert!(
            app.input.is_empty(),
            "the composer is cleared because the prompt was taken, not refused"
        );
        let queued = store.read_state(&job.identity.job_id).unwrap();
        assert!(
            queued
                .identity
                .pending_prompts
                .iter()
                .any(|prompt| prompt.text.contains("keep working on the parser")),
            "the prompt waits where the starting worker will claim it"
        );
        assert!(
            app.rebon_tui.transcript.rows().iter().any(|row| matches!(
                row,
                rebon_tui::Message::User(user)
                    if user.message.content.iter().any(|block| matches!(
                        block,
                        rebon_tui::UserContentBlock::Text(text)
                            if text.text.contains("keep working on the parser")
                    ))
            )),
            "the prompt is echoed the moment it is durable"
        );
        drop(runtime);
    }

    /// Typing before the default startup's worker is up is the ordinary
    /// case, not an edge: the prompt goes to the job the session was born
    /// on, is echoed at once, and never runs in this process.
    #[test]
    fn a_prompt_typed_before_the_startup_worker_is_up_waits_on_the_job() {
        let (runtime, handle) = make_immediate_handle();
        let config_dir = TempDir::new().unwrap();
        let _config = HandoverStoreGuard::set(config_dir.path());
        let store = crate::background::cli_default_store();
        let mut job = store
            .create_job(
                "startup".into(),
                std::path::PathBuf::from("."),
                background_runtime_fields_for_test(),
            )
            .unwrap();
        job.identity.session_id = Some("sess-born-hosted".into());
        store.write_state(&job).unwrap();

        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        session.attached_background_job_id = Some(job.identity.job_id.clone());
        session.pending_hosted_session = Some(crate::background::PendingHostedSession::startup(
            job.identity.job_id.clone(),
        ));
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "hello from second zero".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(recorder.take_requests().is_empty());
        assert!(active_prompt.is_none());
        assert!(app.input.is_empty());
        let queued = store.read_state(&job.identity.job_id).unwrap();
        assert!(queued
            .identity
            .pending_prompts
            .iter()
            .any(|prompt| prompt.text.contains("hello from second zero")));
        assert!(
            !queued.identity.resume_only,
            "the worker that comes up runs it instead of parking"
        );
        assert!(
            session.pending_hosted_session.is_some(),
            "typing does not give up the mirror: the session is nowhere else"
        );
        drop(runtime);
    }

    /// A dispatched job was never this session's: it runs in its own worker
    /// and the wait is only for a mirror. Making the user sit through a
    /// process start before they can type would be charging them for a view.
    /// Sending gives up the mirror — and says where the job went.
    #[test]
    fn a_prompt_sent_while_waiting_for_a_dispatch_mirror_gives_up_the_mirror_and_runs_here() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        session.attached_background_job_id = Some("bg-dispatch-mirror".into());
        session.pending_hosted_session = Some(crate::background::PendingHostedSession::dispatch(
            "bg-dispatch-mirror".into(),
        ));
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "keep working on the parser".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));
        runtime.block_on(tokio::task::yield_now());

        assert!(session.pending_hosted_session.is_none());
        assert!(session.attached_background_job_id.is_none());
        assert_eq!(
            recorder.take_requests().len(),
            1,
            "the prompt the user typed has to actually run here"
        );
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(system)
                if system.content.as_deref().is_some_and(|c| c.contains("Stopped waiting to mirror bg-dispatch-mirror"))
        )));
        drop(runtime);
    }

    /// Enter on an empty prompt is not a decision — it must not silently
    /// throw away a mirror the user is waiting for.
    #[test]
    fn an_empty_enter_does_not_give_up_a_pending_dispatch_mirror() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        session.attached_background_job_id = Some("bg-empty-enter".into());
        session.pending_hosted_session = Some(crate::background::PendingHostedSession::dispatch(
            "bg-empty-enter".into(),
        ));
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "   ".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(session.pending_hosted_session.is_some());
        assert!(recorder.take_requests().is_empty());
        assert!(app.rebon_tui.transcript.is_empty());
        drop(runtime);
    }

    #[test]
    fn slash_vim_toggles_locally_without_submitting_prompt() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        app.input = "/vim".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/vim".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert_eq!(app.vim_mode, Some(VimMode::Insert));
        assert!(app.input.is_empty());
        assert!(active_prompt.is_none());
        assert!(app.queued_commands.is_empty());
        assert!(recorder.take_requests().is_empty());
        assert!(
            app.rebon_tui
                .transcript
                .rows()
                .iter()
                .any(|row| matches!(row, rebon_tui::Message::System(system) if system.subtype == "local_command" && system.uuid.starts_with("s-vim-") && system.content.as_deref() == Some("Vim mode enabled (INSERT)."))),
            "/vim should append local feedback instead of committing a user prompt"
        );

        assert!(!submit_or_queue(
            &mut app,
            "/vim".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert_eq!(app.vim_mode, None);
        assert!(active_prompt.is_none());
        assert!(app.queued_commands.is_empty());
        assert!(recorder.take_requests().is_empty());
        assert!(
            app.rebon_tui
                .transcript
                .rows()
                .iter()
                .any(|row| matches!(row, rebon_tui::Message::System(system) if system.subtype == "local_command" && system.uuid.starts_with("s-vim-") && system.content.as_deref() == Some("Vim mode disabled."))),
            "/vim should report the disabled state locally"
        );
        drop(runtime);
    }

    #[test]
    fn slash_permissions_retry_intercepts_locally_and_spawns_replay() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let stale_runtime = session.engine_half.runtime.clone();
        session
            .swap_runtime("permission-retry-session", "/permission-retry", false)
            .expect("install retry runtime");
        let intended_runtime = session.engine_half.runtime.clone();
        let mut app = AppState::new();
        app.mid_turn_queued_submit_poller =
            Some(session.engine_half.runtime.mid_turn_queue.clone());
        let id = app.auto_mode_denials.lock().unwrap().record(
            rebon_permissions::auto_mode_denials::AutoModeDenialInput {
                tool_use_id: "toolu-retry-1".into(),
                tool_name: "Bash".into(),
                tool_input: "{\"command\":\"echo replay\"}".into(),
                reason: "classifier denied".into(),
                display: "Bash: echo replay".into(),
                timestamp_ms: 1,
                task_id: Some("task-1".into()),
                conversation_id: Some("conv-1".into()),
            },
        );
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            format!("/permissions retry {id}"),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));
        runtime.block_on(tokio::task::yield_now());

        let active = active_prompt
            .as_ref()
            .expect("retry should spawn a replay turn");
        assert_eq!(active.source, LocalTurnSource::PermissionRetry);
        assert!(Arc::ptr_eq(
            active.runtime.as_ref().expect("retry runtime"),
            &intended_runtime
        ));
        assert!(!Arc::ptr_eq(
            active.runtime.as_ref().expect("retry runtime"),
            &stale_runtime
        ));
        assert!(
            app.queued_commands.is_empty(),
            "retry should not queue a model prompt"
        );
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].session_id, "permission-retry-session");
        assert_eq!(
            requests[0].cwd,
            std::path::PathBuf::from("/permission-retry")
        );
        assert!(
            requests[0].prompt.is_empty(),
            "replay turn is not a model prompt"
        );
        assert_eq!(requests[0].replay_requests.len(), 1);
        let replay = &requests[0].replay_requests[0];
        assert_eq!(replay.denial_id, id);
        assert_eq!(replay.tool_use_id, "toolu-retry-1");
        assert_eq!(replay.tool_name, "Bash");
        assert_eq!(replay.tool_input, "{\"command\":\"echo replay\"}");
        assert_eq!(replay.task_id.as_deref(), Some("task-1"));
        assert_eq!(replay.conversation_id.as_deref(), Some("conv-1"));
        drop(runtime);
    }

    #[test]
    fn user_skill_slash_command_submits_explicit_skill_invocation() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let registry = Arc::new(rebon_plugin_skill::SkillRegistry::new());
        registry.register(rebon_plugin_skill::Skill {
            id: "radare2".into(),
            title: "radare2".into(),
            description: "Reverse engineering workflow.".into(),
            prompt_template: "Use radare2 for this task.".into(),
            suggested_tools: Vec::new(),
            source: rebon_plugin_skill::SkillSource::Project,
            argument_hint: None,
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: true,
            disable_model_invocation: false,
            required_tools: Vec::new(),
        });
        session.engine_half.skill_registry = registry;
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/radare2 analyze main".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].skill_invocations.len(), 1);
        assert_eq!(requests[0].skill_invocations[0].skill, "radare2");
        assert_eq!(
            requests[0].skill_invocations[0].args.as_deref(),
            Some("analyze main")
        );
        drop(runtime);
    }

    #[test]
    fn unrelated_slash_command_without_arguments_is_submitted_as_prompt() {
        assert_slash_prompt_submitted_verbatim("/epub");
    }

    #[test]
    fn unrelated_slash_command_with_arguments_is_submitted_as_prompt() {
        assert_slash_prompt_submitted_verbatim("/zzzzzz value");
    }

    #[test]
    fn misspelled_slash_command_is_blocked_with_typo_suggestion() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        app.slash_commands = rebon_slash_commands::for_surface(rebon_slash_commands::Surface::Tui);
        app.input = "/effect max".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/effect max".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert_eq!(app.input, "/effect max");
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(system)
                if system.subtype == "error"
                    && system.content.as_deref()
                        == Some("Unknown slash command `/effect`. Did you mean `/effort`?")
        )));
        drop(runtime);
    }

    #[test]
    fn slash_prefixed_paths_are_not_treated_as_commands() {
        assert!(parse_user_skill_invocation("/tmp/file explain this").is_none());
    }

    #[test]
    fn slash_skills_opens_modal_without_submitting_or_writing_transcript() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let registry = Arc::new(rebon_plugin_skill::SkillRegistry::new());
        registry.register(rebon_plugin_skill::Skill {
            id: "project-skill".into(),
            title: "Project Skill".into(),
            description: "Loaded only from this test registry.".into(),
            prompt_template: "Do project-specific work".into(),
            suggested_tools: Vec::new(),
            source: rebon_plugin_skill::SkillSource::Project,
            argument_hint: None,
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: true,
            disable_model_invocation: false,
            required_tools: Vec::new(),
        });
        session.engine_half.skill_registry = registry;
        let mut app = AppState::new();
        app.input = "/skills".into();
        app.cursor_offset = app.input.len();
        let transcript_len = app.rebon_tui.transcript.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/skills".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(app.input.is_empty());
        assert_eq!(
            app.dialogs.top_id(),
            Some(rebon_ui_seat::ids::dialog::SKILLS)
        );
        assert!(app.has_modal_overlay());
        assert!(active_prompt.is_none());
        assert!(app.queued_commands.is_empty());
        assert!(recorder.take_requests().is_empty());
        assert_eq!(app.rebon_tui.transcript.len(), transcript_len);
        drop(runtime);
    }

    #[test]
    fn slash_skills_with_empty_registry_shows_only_local_feedback() {
        let (_runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        session.engine_half.skill_registry = Arc::new(rebon_plugin_skill::SkillRegistry::new());
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        app.input = "/skills".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/skills".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(app.input.is_empty());
        assert!(!app.dialogs.is_open());
        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert_eq!(
            local_command_output(&app, "skills"),
            Some("No skills are currently loaded for this session.")
        );
    }

    #[test]
    fn manually_typed_disabled_skill_is_blocked_with_reenable_hint() {
        let (_runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let registry = Arc::new(rebon_plugin_skill::SkillRegistry::new());
        registry.register(rebon_plugin_skill::Skill {
            id: "project-skill".into(),
            title: "Project Skill".into(),
            description: "Project workflow.".into(),
            prompt_template: "Do project-specific work".into(),
            suggested_tools: Vec::new(),
            source: rebon_plugin_skill::SkillSource::Project,
            argument_hint: None,
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: true,
            disable_model_invocation: false,
            required_tools: Vec::new(),
        });
        registry.set_disabled_skills(["project-skill"]);
        session.engine_half.skill_registry = registry;
        let mut app = AppState::new();
        // Include a deliberately stale slash row to prove the registry check
        // wins even before every UI snapshot has refreshed.
        app.slash_commands.push(rebon_types::SlashCommand {
            name: "project-skill".into(),
            description: "stale".into(),
            input: None,
            category: Some(rebon_types::SlashCommandCategory::Skill),
            aliases: Vec::new(),
        });
        app.input = "/project-skill target".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/project-skill target".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(system)
                if system.subtype == "error"
                    && system.content.as_deref()
                        == Some("Skill `/project-skill` is disabled. Use `/skills` to re-enable it.")
        )));
    }

    fn local_command_output<'a>(app: &'a AppState, label: &str) -> Option<&'a str> {
        let prefix = format!("s-{label}-");
        app.rebon_tui
            .transcript
            .rows()
            .iter()
            .find_map(|row| match row {
                rebon_tui::Message::System(system)
                    if system.subtype == "local_command" && system.uuid.starts_with(&prefix) =>
                {
                    system.content.as_deref()
                }
                _ => None,
            })
    }

    fn goal_local_command_outputs(app: &AppState) -> Vec<&str> {
        app.rebon_tui
            .transcript
            .rows()
            .iter()
            .filter_map(|row| match row {
                rebon_tui::Message::System(system)
                    if system.subtype == "local_command" && system.uuid.starts_with("s-goal-") =>
                {
                    system.content.as_deref()
                }
                _ => None,
            })
            .collect()
    }

    struct EnvVarGuard {
        vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        fn new(names: &[&'static str]) -> Self {
            let _lock = crate::test_env::lock_env();
            let vars = names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect();
            Self { vars, _lock }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                for (name, value) in self.vars.drain(..) {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    #[derive(Clone)]
    struct TestNamedTool {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait::async_trait]
    impl rebon_tool::Tool for TestNamedTool {
        fn id(&self) -> rebon_tools_core::ToolId {
            rebon_tools_core::ToolId::new(self.name)
        }

        fn description(&self) -> &str {
            self.description
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn call(
            &self,
            _input: serde_json::Value,
            _context: &rebon_tool::ToolContext,
        ) -> rebon_tools_core::ToolResult<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
    }

    #[test]
    fn slash_memory_opens_browser_without_submitting_prompt() {
        let _env = EnvVarGuard::new(&[
            "REBON_CONFIG_DIR",
            "REBON_DISABLE_AUTO_MEMORY",
            "REBON_SIMPLE",
        ]);
        unsafe {
            std::env::remove_var("REBON_CONFIG_DIR");
            std::env::set_var("REBON_DISABLE_AUTO_MEMORY", "1");
            std::env::remove_var("REBON_SIMPLE");
        }
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("REBON.md"), "memory fixture content".repeat(4)).unwrap();
        session.cwd = std::fs::canonicalize(&project)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let expected_path = std::path::PathBuf::from(&session.cwd).join("REBON.md");
        let mut app = AppState::new();
        app.input = "/memory".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/memory".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(app.input.is_empty());
        assert!(active_prompt.is_none());
        assert!(app.queued_commands.is_empty());
        assert!(recorder.take_requests().is_empty());
        assert!(local_command_output(&app, "memory").is_none());
        assert_eq!(
            app.dialogs.top_id(),
            Some(rebon_ui_seat::ids::dialog::MEMORY),
            "/memory browser"
        );
        let key = |code| {
            ratatui::crossterm::event::KeyEvent::new(
                code,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )
        };
        crate::tui::dialog_host::handle_key(
            &mut app.dialogs,
            &key(ratatui::crossterm::event::KeyCode::End),
        );
        let outcome = crate::tui::dialog_host::handle_key(
            &mut app.dialogs,
            &key(ratatui::crossterm::event::KeyCode::Enter),
        );
        let crate::tui::dialog_host::HostKey::Action(action) = outcome else {
            panic!("expected an open action, got {outcome:?}");
        };
        assert_eq!(action.action, rebon_ui_seat::ids::action::OPEN);
        assert_eq!(std::path::PathBuf::from(action.value()), expected_path);
        drop(runtime);
    }

    #[test]
    fn slash_hooks_shows_read_only_metadata_without_submitting_prompt() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        app.input = "/hooks".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/hooks".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(app.input.is_empty());
        assert!(active_prompt.is_none());
        assert!(app.queued_commands.is_empty());
        assert!(recorder.take_requests().is_empty());
        assert_eq!(
            app.dialogs.top_id(),
            Some(rebon_ui_seat::ids::dialog::HOOKS)
        );
        assert!(local_command_output(&app, "hooks").is_none());
        drop(runtime);
    }

    #[test]
    fn slash_hooks_known_event_and_unknown_event_are_local() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let mut active_prompt = None;
        let known_event = rebon_hooks::HOOK_EVENTS[0].name();

        assert!(!submit_or_queue(
            &mut app,
            format!("/hooks {known_event}"),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));
        assert_eq!(
            app.dialogs.top_id(),
            Some(rebon_ui_seat::ids::dialog::HOOKS)
        );
        assert!(local_command_output(&app, "hooks").is_none());
        assert!(active_prompt.is_none());
        assert!(app.queued_commands.is_empty());
        assert!(recorder.take_requests().is_empty());

        assert!(!submit_or_queue(
            &mut app,
            "/hooks NoSuchEvent".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));
        let output = app
            .rebon_tui
            .transcript
            .rows()
            .iter()
            .rev()
            .find_map(|row| match row {
                rebon_tui::Message::System(system)
                    if system.subtype == "local_command" && system.uuid.starts_with("s-hooks-") =>
                {
                    system.content.as_deref()
                }
                _ => None,
            })
            .expect("/hooks unknown feedback");
        assert!(output.contains("Unknown hook event: NoSuchEvent"));
        assert!(output.contains("Run `/hooks`"));
        assert!(active_prompt.is_none());
        assert!(app.queued_commands.is_empty());
        assert!(recorder.take_requests().is_empty());
        drop(runtime);
    }

    #[test]
    fn bang_prefixed_input_executes_inline_shell_without_submitting_prompt_or_task() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut engine = rebon_core::Engine::new();
        engine.register_tool(Arc::new(TestNamedTool {
            name: if cfg!(target_os = "windows") {
                "PowerShell"
            } else {
                "Bash"
            },
            description: "test shell tool",
        }));
        session.engine_half.engine = Arc::new(engine);
        let mut app = AppState::new();
        app.tasks = session.engine_half.tasks.clone();
        app.input = "!echo ok".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        {
            let _guard = handle.enter();
            assert!(!submit_or_queue(
                &mut app,
                "!echo ok".into(),
                &mut session,
                &handle,
                &mut active_prompt,
                &mut None,
                UiMode::Screen,
            ));
        }

        assert!(app.input.is_empty());
        assert_eq!(app.mode, "prompt");
        assert!(active_prompt.is_none());
        assert!(app.queued_commands.is_empty());
        assert!(recorder.take_requests().is_empty());
        assert!(app.task_snapshots().is_empty());
        assert!(app.background_tasks_dialog.is_none());
        assert_eq!(
            local_command_output(&app, "shell"),
            Some("!echo ok\n⎿ Running… (0s)")
        );
        assert_eq!(
            app.history.last().map(|entry| entry.display.as_str()),
            Some("!echo ok")
        );
        runtime.shutdown_background();
    }

    #[test]
    fn bash_mode_submit_executes_inline_shell_without_bang_prefix() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut engine = rebon_core::Engine::new();
        engine.register_tool(Arc::new(TestNamedTool {
            name: if cfg!(target_os = "windows") {
                "PowerShell"
            } else {
                "Bash"
            },
            description: "test shell tool",
        }));
        session.engine_half.engine = Arc::new(engine);
        let mut app = AppState::new();
        app.tasks = session.engine_half.tasks.clone();
        app.mode = "bash".into();
        app.input = "pwd".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        {
            let _guard = handle.enter();
            assert!(!submit_or_queue(
                &mut app,
                "pwd".into(),
                &mut session,
                &handle,
                &mut active_prompt,
                &mut None,
                UiMode::Screen,
            ));
        }

        assert!(app.input.is_empty());
        assert_eq!(app.mode, "prompt");
        assert!(active_prompt.is_none());
        assert!(app.queued_commands.is_empty());
        assert!(recorder.take_requests().is_empty());
        assert!(app.task_snapshots().is_empty());
        assert!(app.background_tasks_dialog.is_none());
        assert_eq!(
            local_command_output(&app, "shell"),
            Some("!pwd\n⎿ Running… (0s)")
        );
        assert_eq!(
            app.history.last().map(|entry| entry.display.as_str()),
            Some("!pwd")
        );
        runtime.shutdown_background();
    }

    #[test]
    fn slash_mcp_opens_browser_with_loaded_tools_without_submitting_prompt() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut engine = rebon_core::Engine::new();
        engine.register_tool(Arc::new(TestNamedTool {
            name: "mcp__fixture__list_items",
            description: "fixture mcp tool",
        }));
        session.engine_half.engine = Arc::new(engine);
        let mut app = AppState::new();
        app.input = "/mcp".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/mcp".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(app.input.is_empty());
        assert!(active_prompt.is_none());
        assert!(app.queued_commands.is_empty());
        assert!(recorder.take_requests().is_empty());
        assert!(local_command_output(&app, "mcp").is_none());
        let dialog = app.mcp_dialog.as_ref().expect("/mcp browser");
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20))
            .expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                dialog.render(frame, area);
            })
            .expect("render /mcp browser");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Loaded tools (1)"));
        assert!(rendered.contains("mcp__fixture__list_items"));
        drop(runtime);
    }

    /// A mirror hosts no servers, so its `/mcp` browser is drawn over what
    /// the owner last reported — answered here, not sent over as a command.
    #[test]
    fn slash_mcp_on_a_mirror_opens_the_browser_over_the_owners_snapshot() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut remote = crate::background::RemoteBackgroundAttachment::new(
            "bg-mirror".into(),
            session.session_id.clone(),
            ".".into(),
            rebon_session_host::BackgroundJobStatus::Idle,
            0,
            crate::background::BackgroundIpcEndpoint {
                pid: 1,
                port: 1,
                token: "mirror-token".into(),
            },
        );
        remote.owner_mcp = Some(rebon_session_host::McpStatusSnapshot {
            loader: "ready".into(),
            client: "ready".into(),
            servers: vec![rebon_session_host::McpServerSnapshot {
                name: "fixture".into(),
                transport: "stdio".into(),
                source: "project".into(),
            }],
            tools: vec![rebon_session_host::McpToolSnapshot {
                name: "mcp__fixture__list_items".into(),
                tokens: 9,
            }],
            warnings: Vec::new(),
            error: None,
        });
        session.remote_background_attachment = Some(remote);
        let mut app = AppState::new();
        app.input = "/mcp".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/mcp".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(app.input.is_empty());
        assert!(recorder.take_requests().is_empty());
        assert!(
            session
                .remote_background_attachment
                .as_ref()
                .expect("still attached")
                .pending_command
                .is_none(),
            "answered from the snapshot, not forwarded to the owner"
        );
        let dialog = app.mcp_dialog.as_ref().expect("/mcp browser");
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20))
            .expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                dialog.render(frame, area);
            })
            .expect("render /mcp browser");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("fixture [stdio]"), "{rendered}");
        assert!(rendered.contains("mcp__fixture__list_items"), "{rendered}");

        // Inline mode has no browser; the same snapshot answers as text,
        // still without a round trip to the owner.
        app.input = "/mcp".into();
        app.cursor_offset = app.input.len();
        assert!(!submit_or_queue(
            &mut app,
            "/mcp".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Inline,
        ));
        let text = local_command_output(&app, "mcp").expect("/mcp text");
        assert!(text.contains("loader: ready"), "{text}");
        assert!(text.contains("mcp__fixture__list_items"), "{text}");
        assert!(session
            .remote_background_attachment
            .as_ref()
            .expect("still attached")
            .pending_command
            .is_none());
        drop(runtime);
    }

    #[test]
    fn slash_status_opens_status_settings_in_screen_mode() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        app.input = "/status".into();
        app.cursor_offset = app.input.len();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/status".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(app.input.is_empty());
        assert_eq!(
            app.dialogs.top_id(),
            Some(rebon_ui_seat::ids::dialog::SETTINGS)
        );
        assert!(app.rebon_tui.transcript.is_empty());
        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        drop(runtime);
    }

    #[test]
    fn slash_status_reports_session_state_in_inline_mode_without_submitting_prompt() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        session.cwd = "/tmp/status-cwd".into();
        session.ui_mode = UiMode::Inline;
        let mut app = AppState::new();
        app.input = "/status".into();
        app.cursor_offset = app.input.len();
        app.vim_mode = Some(VimMode::Normal);
        app.session_title = Some("Status Test".into());
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/status".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Inline,
        ));

        assert!(app.input.is_empty());
        assert!(!app.dialogs.is_open());
        assert!(active_prompt.is_none());
        assert!(app.queued_commands.is_empty());
        assert!(recorder.take_requests().is_empty());
        let output = local_command_output(&app, "status").expect("/status feedback");
        assert!(output.contains("Session status"));
        assert!(output.contains("provider: test"));
        assert!(output.contains("model: test-model"));
        assert!(output.contains("cwd: /tmp/status-cwd"));
        assert!(output.contains("ui mode: Inline"));
        assert!(output.contains("vim mode: NORMAL"));
        assert!(output.contains("MCP client: not connected"));
        assert!(output.contains("active agents:"));
        assert!(output.contains("Update status"));
        drop(runtime);
    }

    #[test]
    fn slash_ultraplan_direct_submit_enters_plan_mode_before_spawning_prompt() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_ultraplan_test_tui_session();
        let mut active_prompt = None;

        let should_quit = submit_or_queue(
            &mut app,
            "/ultraplan please plan the migration".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        );

        assert!(!should_quit);
        assert!(active_prompt.is_some());
        assert_eq!(app.permission_mode, rebon_permissions::PermissionMode::Plan);
        assert_eq!(
            *app.permission_mode_cell.lock().expect("permission cell"),
            rebon_permissions::types::PermissionMode::Plan
        );
        let stored_mode = session
            .engine_half
            .handler
            .state()
            .get_session(&session.session_id)
            .expect("session record")
            .permission_mode;
        assert_eq!(stored_mode, "plan");
        let status = app.ultraplan_status.expect("ultraplan status");
        assert!(matches!(status.phase, UltraplanPhase::Orchestrating));
        assert!(status.run_id.starts_with("ultraplan-"));
        assert_eq!(status.task_title, "please plan the migration");
        assert!(status.started_at_ms.is_some());
        assert_eq!(status.worker_count, None);

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn slash_grill_starts_one_ultraplan_run_with_questioning_preauthorized() {
        let _env = RuntimeModeEnvGuard::set_ultraplan_runtime_policy(None, Some("enforce"));
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_ultraplan_test_tui_session();
        let mut active_prompt = None;

        let should_quit = submit_or_queue(
            &mut app,
            "/grill please challenge the migration plan".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        );

        assert!(!should_quit);
        assert!(active_prompt.is_some());
        let status = app.ultraplan_status.as_ref().expect("ultraplan status");
        assert_eq!(status.task_title, "please challenge the migration plan");
        // `/grill` no longer forks the protocol; it only tells the model the
        // user has already authorized deeper questioning.
        assert!(status
            .context
            .as_ref()
            .is_some_and(|context| { context.profile == rebon_types::UltraplanProfile::Standard }));
        let state =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, &status.run_id)
                .expect("persisted run");
        assert_eq!(state.profile, rebon_types::UltraplanProfile::Standard);

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn slash_grill_runs_without_strict_policy_enforcement() {
        for (runtime_value, enforce_value) in [(None, None), (None, Some("observe"))] {
            let _env =
                RuntimeModeEnvGuard::set_ultraplan_runtime_policy(runtime_value, enforce_value);
            let (runtime, handle) = make_immediate_handle();
            let mut app = AppState::new();
            let mut session = make_ultraplan_test_tui_session();
            let mut active_prompt = None;

            let should_quit = submit_or_queue(
                &mut app,
                "/grill strict plan".into(),
                &mut session,
                &handle,
                &mut active_prompt,
                &mut None,
                UiMode::Screen,
            );

            assert!(!should_quit);
            assert!(active_prompt.is_some());
            assert!(app.ultraplan_status.is_some());
            assert_eq!(app.permission_mode, rebon_permissions::PermissionMode::Plan);
            drop(active_prompt.take());
            runtime.shutdown_background();
        }
    }

    #[test]
    fn slash_ultraplan_expands_pasted_text_before_wrapping_model_prompt() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        app.pasted_contents.push(rebon_types::PromptPasteContent {
            id: 1,
            kind: "text".into(),
            content: "Problem: row contamination\nCheck wrapping\nVerify clearing".into(),
            media_type: None,
            filename: None,
            source_path: None,
        });
        let mut session = make_ultraplan_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut active_prompt = None;

        let should_quit = submit_or_queue(
            &mut app,
            "/ultraplan [Pasted text #1 +2 lines]".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        );
        runtime.block_on(tokio::task::yield_now());

        assert!(!should_quit);
        assert!(active_prompt.is_some());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let prompt_text = prompt_request_text(&requests[0]);
        assert!(prompt_text.contains("Problem: row contamination\nCheck wrapping\nVerify clearing"));
        assert!(!prompt_text.contains("[Pasted text #1"));
        let status = app.ultraplan_status.as_ref().expect("ultraplan status");
        assert_eq!(
            status.task_title,
            "Problem: row contamination\nCheck wrapping\nVerify clearing"
        );

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn slash_ultrawork_expands_pasted_text_before_wrapping_model_prompt() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        app.pasted_contents.push(rebon_types::PromptPasteContent {
            id: 1,
            kind: "text".into(),
            content: "Build markdown previewer\nUse nested workflows\nReview XSS".into(),
            media_type: None,
            filename: None,
            source_path: None,
        });
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut active_prompt = None;

        let should_quit = submit_or_queue(
            &mut app,
            "/ultrawork [Pasted text #1 +2 lines]".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        );
        runtime.block_on(tokio::task::yield_now());

        assert!(!should_quit);
        assert!(active_prompt.is_some());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let prompt_text = prompt_request_text(&requests[0]);
        assert!(prompt_text.contains("Use the Workflow tool"));
        let policy = requests[0]
            .execution_policy
            .as_ref()
            .expect("workflow controller policy");
        assert!(policy.auto_mode_script_continuity);
        let ctx = policy.ultraplan.as_ref().expect("workflow policy context");
        assert_eq!(
            ctx.allowed_tools,
            vec!["Workflow", "RunWorkflow", "Read", "Glob", "Grep"]
        );
        assert!(ctx.denied_tools.iter().any(|tool| tool == "Write"));
        assert!(ctx.denied_tools.iter().any(|tool| tool == "Bash"));
        assert!(prompt_text.contains("Build markdown previewer\nUse nested workflows\nReview XSS"));
        assert!(!prompt_text.contains("[Pasted text #1"));
        assert_eq!(
            visible_prompt_text(&app),
            Some("Build markdown previewer\nUse nested workflows\nReview XSS")
        );

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn slash_ultraplan_list_empty_outputs_empty_state() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let temp = TempDir::new().expect("temp projects root");
        session.projects_root = temp.path().to_path_buf();
        session.cwd = temp.path().join("empty-project").display().to_string();
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/ultraplan --list".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert_eq!(
            local_command_output(&app, "ultraplan").as_deref(),
            Some("No ultraplan runs found for this project.")
        );
        drop(runtime);
    }

    #[test]
    fn slash_ultraplan_list_outputs_saved_runs() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let temp = TempDir::new().expect("temp projects root");
        session.projects_root = temp.path().to_path_buf();
        session.cwd = temp.path().join("list-project").display().to_string();
        let mut state = UltraplanRunState::new(
            "ultraplan-test-run".into(),
            session.session_id.clone(),
            "saved task".into(),
            None,
            10,
        );
        state.phase = RunPhase::Researching;
        state.round = 3;
        state.updated_at_ms = 20;
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state)
            .expect("save run");
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/ultraplan --list".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        let output = local_command_output(&app, "ultraplan").expect("list output");
        assert!(output.contains("ultraplan-test-run"));
        assert!(output.contains("saved task"));
        assert!(output.contains("Researching"));
        assert!(recorder.take_requests().is_empty());
        drop(runtime);
    }

    #[test]
    fn slash_ultraplan_resume_reports_missing_and_inactive_runs() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let temp = TempDir::new().expect("temp projects root");
        session.projects_root = temp.path().to_path_buf();
        session.cwd = temp.path().join("resume-project").display().to_string();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/ultraplan --resume missing-run".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));
        assert!(local_command_output(&app, "ultraplan").is_none());
        assert!(app
            .rebon_tui
            .transcript
            .rows()
            .iter()
            .any(|row| format!("{:?}", row).contains("missing-run")));

        let mut inactive = UltraplanRunState::new(
            "ultraplan-inactive".into(),
            session.session_id.clone(),
            "done task".into(),
            None,
            10,
        );
        inactive.phase = RunPhase::Done;
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &inactive)
            .expect("save inactive");
        assert!(!submit_or_queue(
            &mut app,
            "/ultraplan --resume ultraplan-inactive".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));
        assert!(app
            .rebon_tui
            .transcript
            .rows()
            .iter()
            .any(|row| format!("{:?}", row).contains("not active")));
        assert!(recorder.take_requests().is_empty());
        drop(runtime);
    }

    #[test]
    fn slash_ultraplan_resume_restores_a_legacy_grill_run_without_its_protocol() {
        let _env = RuntimeModeEnvGuard::set_ultraplan_runtime_policy(None, Some("enforce"));
        let mut session = make_ultraplan_test_tui_session();
        let temp = TempDir::new().expect("temp projects root");
        session.projects_root = temp.path().to_path_buf();
        session.cwd = temp
            .path()
            .join("resume-grill-project")
            .display()
            .to_string();
        std::fs::create_dir_all(&session.cwd).expect("create resume project root");
        let run_id = "ultraplan-grill-resume";
        let mut state = UltraplanRunState::new(
            run_id.into(),
            "old-session".into(),
            "challenge the rollout".into(),
            None,
            10,
        )
        .with_profile(UltraplanProfile::Grill);
        state.phase = RunPhase::Researching;
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state)
            .expect("save grill run");
        let mut app = AppState::new();

        let submit =
            prepare_ultraplan_resume_submit(&mut app, &mut session, run_id).expect("resume submit");

        let policy = submit.execution_policy.expect("resume execution policy");
        let context = policy.ultraplan.expect("resume ultraplan context");
        // The run resumes on the single planning profile even though it was
        // persisted as Grill, so no interview protocol comes back with it.
        assert_eq!(context.profile, UltraplanProfile::Standard);
        assert!(submit.model_text.as_deref().is_some_and(|prompt| {
            prompt.contains("<untrusted_user_task>")
                && prompt.contains("no stage has to be replayed")
                && !prompt.contains("Grill protocol")
        }));
        let status = app.ultraplan_status.as_ref().expect("restored status");
        assert!(status
            .context
            .as_ref()
            .is_some_and(|context| { context.profile == UltraplanProfile::Standard }));
        let persisted =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
                .unwrap();
        // The persisted profile stays as a historical record of how the run
        // was started; nothing reads it as a protocol switch any more.
        assert_eq!(persisted.profile, UltraplanProfile::Grill);
        assert_eq!(persisted.session_id, session.session_id);
    }

    #[test]
    fn slash_ultraplan_with_active_prompt_enqueues_without_replacing_active_handle() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_ultraplan_test_tui_session();
        let (_tx, rx) = oneshot::channel();
        let active_cancel = PromptCancel::new();
        let mut active_prompt = Some(ActivePrompt::with_task_notifications(
            rx,
            active_cancel.clone(),
            vec![rebon_plugin_tasks::runtime::TaskId::new(
                "sentinel-ultraplan",
            )],
            Vec::new(),
        ));

        let should_quit = submit_or_queue(
            &mut app,
            "/ultraplan plan x".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        );

        assert!(!should_quit);
        assert!(active_prompt.is_some());
        assert_eq!(
            active_prompt
                .as_ref()
                .expect("active prompt")
                .cancel
                .is_cancelled(),
            active_cancel.is_cancelled()
        );
        assert_eq!(
            active_prompt
                .as_ref()
                .expect("active prompt")
                .pending_task_notification_ids
                .as_slice(),
            &[rebon_plugin_tasks::runtime::TaskId::new(
                "sentinel-ultraplan"
            )]
        );
        assert_eq!(app.queued_commands.len(), 1);
        assert_eq!(app.queued_submit_payloads.len(), 1);
        assert!(app.follow_transcript_tail);
        assert_eq!(app.scroll_offset, app.total_content_lines);
        let submit = &app.queued_submit_payloads[0];
        assert_eq!(submit.text, "/ultraplan plan x");
        assert!(submit
            .model_text
            .as_deref()
            .is_some_and(|text| text.starts_with("You are starting REBON LOCAL ULTRAPLAN")));
        let policy = submit.execution_policy.as_ref().expect("ultraplan policy");
        let ctx = policy.ultraplan.as_ref().expect("ultraplan context");
        let status = app.ultraplan_status.as_ref().expect("ultraplan status");
        assert_eq!(ctx.run_id, status.run_id);
        assert_eq!(status.phase, UltraplanPhase::PlanModeActive);
        assert_eq!(status.task_title, "plan x");

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn slash_ceo_task_with_active_prompt_enqueues_without_replacing_active_handle() {
        let _env = RuntimeModeEnvGuard::set_coordinator(None);
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let (_tx, rx) = oneshot::channel();
        let active_cancel = PromptCancel::new();
        let mut active_prompt = Some(ActivePrompt::with_task_notifications(
            rx,
            active_cancel.clone(),
            vec![rebon_plugin_tasks::runtime::TaskId::new("sentinel-ceo")],
            Vec::new(),
        ));

        let should_quit = submit_or_queue(
            &mut app,
            "/ceo do work".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        );

        assert!(!should_quit);
        assert!(active_prompt.is_some());
        assert_eq!(
            active_prompt
                .as_ref()
                .expect("active prompt")
                .cancel
                .is_cancelled(),
            active_cancel.is_cancelled()
        );
        assert_eq!(
            active_prompt
                .as_ref()
                .expect("active prompt")
                .pending_task_notification_ids
                .as_slice(),
            &[rebon_plugin_tasks::runtime::TaskId::new("sentinel-ceo")]
        );
        assert_eq!(app.queued_commands.len(), 1);
        assert_eq!(app.queued_submit_payloads.len(), 1);
        assert_eq!(app.queued_submit_payloads[0].text, "do work");
        assert!(app.follow_transcript_tail);
        assert_eq!(app.scroll_offset, app.total_content_lines);
        assert!(app.coordinator_mode);

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn slash_context_opens_dialog_in_screen_mode_and_prints_in_inline_mode() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();

        let mut screen_app = AppState::new();
        let mut screen_active = None;
        assert!(!submit_or_queue(
            &mut screen_app,
            "/context".into(),
            &mut session,
            &handle,
            &mut screen_active,
            &mut None,
            UiMode::Screen,
        ));
        assert_eq!(
            screen_app.dialogs.top_id(),
            Some(rebon_ui_seat::ids::dialog::CONTEXT)
        );

        let mut inline_app = AppState::new();
        let mut inline_active = None;
        assert!(!submit_or_queue(
            &mut inline_app,
            "/context".into(),
            &mut session,
            &handle,
            &mut inline_active,
            &mut None,
            UiMode::Inline,
        ));
        assert!(!inline_app.dialogs.is_open());
        assert!(
            inline_app
                .rebon_tui
                .transcript
                .rows()
                .iter()
                .any(|row| matches!(row, rebon_tui::Message::System(system) if system.subtype == "local_command" && system.uuid.starts_with("s-context-") && system.content.as_deref().unwrap_or("").contains("Context"))),
            "inline /context should append visible local-command context output to transcript"
        );
        drop(runtime);
    }

    #[test]
    fn context_command_opens_dialog_without_transcript_residue() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::default();
        let mut session = make_test_tui_session();
        let mut active_prompt: Option<ActivePrompt> = None;

        let should_quit = submit_or_queue(
            &mut app,
            "/context".to_string(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        );

        assert!(!should_quit);
        assert_eq!(app.rebon_tui.transcript.len(), 0);
        let dialog = app
            .dialogs
            .top_as::<rebon_dialog::context_dialog::ContextDialogState>()
            .expect("context dialog should be open");
        let rebon_dialog::model::ViewSpec::Outline(view) =
            rebon_dialog::model::DialogModel::view(dialog)
        else {
            panic!("the context browser paints an outline");
        };
        assert!(view
            .rows
            .iter()
            .any(|row| row.text.contains("Context Usage")));
        drop(runtime);
    }

    #[test]
    fn submit_or_queue_enqueues_and_clears_input_while_loading() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        app.input = "queued".into();
        app.cursor_offset = 6;
        let (_tx, rx) = oneshot::channel();
        let mut active = Some(ActivePrompt::new(rx, rebon_types::PromptCancel::new()));
        let mut session = make_test_tui_session();

        submit_or_queue(
            &mut app,
            "queued  ".into(),
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
        );
        assert_eq!(app.queued_commands.len(), 1);
        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
        drop(runtime);
    }

    #[test]
    fn external_user_uuid_is_preserved_for_mid_turn_queue() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let (_tx, rx) = oneshot::channel();
        let mut active = Some(ActivePrompt::new(rx, rebon_types::PromptCancel::new()));
        let mut session = make_test_tui_session();

        submit_or_queue_with_images_and_uuid(
            &mut app,
            "queued from mobile".into(),
            Vec::new(),
            Some("u-mobile-command-queued".into()),
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
        );

        assert_eq!(
            app.queued_submit_payloads[0].user_message_uuid.as_deref(),
            Some("u-mobile-command-queued")
        );
        drop(runtime);
    }

    #[test]
    fn external_user_uuid_is_committed_for_idle_submit() {
        let (_runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut active = None;
        let mut session = make_test_tui_session();

        submit_or_queue_with_images_and_uuid(
            &mut app,
            "sent from mobile".into(),
            Vec::new(),
            Some("u-mobile-command-idle".into()),
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
        );

        assert!(app
            .rebon_tui
            .transcript
            .get("u-mobile-command-idle")
            .is_some());
    }

    /// `/review` is a model turn wearing a slash command. On a session whose
    /// engine lives in a worker it used to spawn that turn locally — a
    /// second process writing the transcript the worker owns — because the
    /// command was handled above every ownership gate. It has to route like
    /// any other prompt — to the job, even when the job's worker is gone:
    /// a parked session is still the job's, and a prompt is what gives it
    /// a worker.
    #[test]
    fn review_on_a_parked_session_goes_to_the_job_not_the_local_engine() {
        let (_runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut active = None;
        let mut session = make_test_tui_session();
        session.attached_background_job_id = Some("bg-review-parked-test".into());
        session.remote_background_attachment = Some(
            crate::background::RemoteBackgroundAttachment::without_worker(
                "bg-review-parked-test".into(),
                session.session_id.clone(),
                session.cwd.clone(),
                crate::background::BackgroundJobStatus::Stopped,
                0,
            ),
        );

        submit_or_queue_with_images_and_uuid(
            &mut app,
            "/review".into(),
            Vec::new(),
            None,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
        );

        assert!(
            active.is_none(),
            "a parked session must not start a local turn for /review"
        );
        // The job does not exist in this test's store, so queueing fails —
        // and the failure names the job the prompt was meant for, rather
        // than the turn running here.
        assert!(
            system_transcript_text(&app).contains("bg-review-parked-test"),
            "{}",
            system_transcript_text(&app)
        );
    }

    /// `/review` was the one this review found, but it is a class: every
    /// slash command that spawns a turn against this process's engine has
    /// the same hole. These cannot be rerouted — they flip local state
    /// first — so they refuse, loudly, instead of writing a transcript the
    /// worker owns.
    #[test]
    fn turn_starting_slash_commands_refuse_on_a_session_owned_elsewhere() {
        for command in [
            "/ultrawork do it",
            "/ceo ship it",
            "/ultraplan build it",
            "/goal ship it",
            // Not turns, but work bound to this terminal and attributed to
            // a session that lives elsewhere.
            "/run echo hi",
            "/agent look into it",
            // The switch half of `/agent`. It cannot be forwarded the way
            // `/backend` is — the same words also mean "spawn a sub-agent with
            // this prompt" — so it refuses. It used to be refused only because
            // the spawn branch swallowed every input and refused there; once
            // that branch started yielding whatever names an agent, these fell
            // through to a switch that flipped this process's state and told
            // the user it had switched their session.
            "/agent local",
            "/agent Local",
            "/agent:local",
            "/agent reconnect",
        ] {
            let (_runtime, handle) = make_immediate_handle();
            let mut app = AppState::new();
            let mut active = None;
            let mut session = make_test_tui_session();
            session.remote_background_attachment =
                Some(crate::background::RemoteBackgroundAttachment::new(
                    "bg-elsewhere".into(),
                    "sess-elsewhere".into(),
                    ".".into(),
                    rebon_session_host::BackgroundJobStatus::Running,
                    0,
                    crate::background::BackgroundIpcEndpoint {
                        pid: 1,
                        port: 1,
                        token: "t".into(),
                    },
                ));

            submit_or_queue_with_images_and_uuid(
                &mut app,
                command.into(),
                Vec::new(),
                None,
                &mut session,
                &handle,
                &mut active,
                &mut None,
                UiMode::Screen,
            );

            assert!(
                active.is_none(),
                "{command} started a local turn on a session the worker owns"
            );
            assert!(
                app.rebon_tui.transcript.rows().iter().any(|row| matches!(
                    row,
                    rebon_tui::Message::System(message)
                        if message.content.as_deref().is_some_and(|c| c.contains("bg-elsewhere"))
                )),
                "{command} has to say where the session actually runs"
            );
        }
    }

    /// `/backend` runs where the session does. On one this terminal owns that
    /// is here — and it has to *be* here: the catalog advertises the command on
    /// this surface, so a terminal with no interceptor for it puts the command
    /// in its own picker and then sends the words to the model.
    #[test]
    fn backend_runs_locally_on_a_session_this_terminal_owns() {
        let (_runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut active = None;
        let mut session = make_test_tui_session();

        submit_or_queue_with_images_and_uuid(
            &mut app,
            "/backend".into(),
            Vec::new(),
            None,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
        );

        assert!(active.is_none(), "/backend must not start an engine turn");
        assert!(
            system_transcript_text(&app).contains("Agents for this session:"),
            "/backend has to answer here: {}",
            system_transcript_text(&app)
        );
    }

    /// On a session a worker owns it goes to the worker instead — the same
    /// channel `/compact` uses, which is why `/backend` was given a name of its
    /// own rather than `/agent`'s second meaning.
    #[test]
    fn backend_is_forwarded_to_the_worker_that_owns_the_session() {
        let (_runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut active = None;
        let mut session = make_test_tui_session();
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                "bg-elsewhere".into(),
                "sess-elsewhere".into(),
                ".".into(),
                rebon_session_host::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "t".into(),
                },
            ));

        submit_or_queue_with_images_and_uuid(
            &mut app,
            "/backend codex".into(),
            Vec::new(),
            None,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
        );

        assert!(active.is_none());
        assert!(
            system_transcript_text(&app).contains("Sent /backend to the attached"),
            "{}",
            system_transcript_text(&app)
        );
        assert_eq!(
            session.engine_half.runtime.session_agents.current_id(),
            rebon_config::LOCAL_AGENT_ID,
            "the switch belongs to the worker, not to this process"
        );
    }

    /// A session parked on a stopped worker is still that worker's, and the
    /// switch would still write the sidecar the next worker reads — so it
    /// is refused, and the refusal says which worker to give a prompt to.
    #[test]
    fn backend_refuses_on_a_session_parked_on_a_stopped_worker() {
        let (_runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut active = None;
        let mut session = make_test_tui_session();
        session.attached_background_job_id = Some("bg-handed-over".into());
        session.remote_background_attachment = Some(
            crate::background::RemoteBackgroundAttachment::without_worker(
                "bg-handed-over".into(),
                session.session_id.clone(),
                session.cwd.clone(),
                crate::background::BackgroundJobStatus::Stopped,
                0,
            ),
        );

        submit_or_queue_with_images_and_uuid(
            &mut app,
            "/backend local".into(),
            Vec::new(),
            None,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
        );

        assert!(active.is_none());
        assert!(
            system_transcript_text(&app).contains("bg-handed-over"),
            "the refusal has to say where the session went: {}",
            system_transcript_text(&app)
        );
    }

    /// Everything `/agent` understands has to be reachable through it. The
    /// spawn branch yields for the switch, and what it fails to recognise it
    /// swallows: `/agent reconnect` used to start a sub-agent whose prompt was
    /// the word "reconnect", and `/agent Codex` one whose prompt was "Codex".
    #[test]
    fn agent_subcommands_reach_the_switch_rather_than_the_spawner() {
        // A test session carries no runtime factory, so `reconnect` answers
        // with the surface refusal rather than reaching a backend. Either way
        // it answered: the spawner writes nothing to the transcript at all.
        for (command, expected) in [
            ("/agent reconnect", "cannot reconnect an agent"),
            ("/agent Local", "Switched to"),
            ("/agent list", "Agents for this session:"),
        ] {
            let (_runtime, handle) = make_immediate_handle();
            let mut app = AppState::new();
            let mut active = None;
            let mut session = make_test_tui_session();

            submit_or_queue_with_images_and_uuid(
                &mut app,
                command.into(),
                Vec::new(),
                None,
                &mut session,
                &handle,
                &mut active,
                &mut None,
                UiMode::Screen,
            );

            assert!(
                app.background_tasks_dialog.is_none(),
                "{command} spawned a sub-agent instead of running"
            );
            assert!(
                system_transcript_text(&app).contains(expected),
                "{command}: {}",
                system_transcript_text(&app)
            );
        }
    }

    /// The other direction: a prompt that opens with a word the switch knows is
    /// still a prompt, or `/agent list the open files` stops working.
    #[test]
    fn a_prompt_that_opens_with_a_subcommand_still_spawns() {
        for command in [
            "/agent list the open files",
            "/agent reconnect the parser and the lexer",
            "/agent local variables are shadowing the globals",
        ] {
            let (_runtime, handle) = make_immediate_handle();
            let mut app = AppState::new();
            let mut active = None;
            let mut session = make_test_tui_session();

            submit_or_queue_with_images_and_uuid(
                &mut app,
                command.into(),
                Vec::new(),
                None,
                &mut session,
                &handle,
                &mut active,
                &mut None,
                UiMode::Screen,
            );

            assert!(
                app.background_tasks_dialog.is_some(),
                "{command} is a prompt and has to reach the spawner"
            );
        }
    }

    /// The same command on a session this terminal really owns still runs
    /// here — the gate is about ownership, not about `/review`.
    #[test]
    fn review_on_a_local_session_still_runs_locally() {
        let (_runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut active = None;
        let mut session = make_test_tui_session();

        submit_or_queue_with_images_and_uuid(
            &mut app,
            "/review".into(),
            Vec::new(),
            None,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
        );

        assert!(active.is_some(), "an owned session reviews in place");
    }

    #[test]
    fn new_submit_clears_withdrawn_prompt_late_update_suppression() {
        let (tx, update_rx) = unbounded_channel();
        send_text_chunk(&tx, "sess-1", "stale reply");
        let mut app = AppState::new();
        app.suppress_late_visible_updates_after_withdrawal = true;
        app.input = "new prompt".into();
        app.cursor_offset = app.input.len();
        let mut session = make_test_tui_session();
        session.engine_half.update_rx = update_rx;
        let (_runtime, handle) = make_immediate_handle();
        let mut active = None;
        let mut pending_permission = None;

        assert!(!submit_or_queue(
            &mut app,
            "new prompt".into(),
            &mut session,
            &handle,
            &mut active,
            &mut pending_permission,
            UiMode::Screen,
        ));
        assert!(!app.suppress_late_visible_updates_after_withdrawal);
        assert!(app.rebon_tui.overlay.is_empty());
        assert_eq!(app.rebon_tui.transcript.len(), 1);
    }

    /// `/ultrawork` (like the other direct-spawn slash intercepts) bypasses
    /// the generic submit path, so it must clear the post-withdrawal
    /// suppression latch itself — otherwise every visible update of the new
    /// turn (thinking, tool cards, workflow progress) is silently dropped
    /// for the rest of the session.
    #[test]
    fn ultrawork_submit_clears_withdrawn_prompt_late_update_suppression() {
        let (tx, update_rx) = unbounded_channel();
        send_text_chunk(&tx, "sess-1", "stale reply");
        let mut app = AppState::new();
        app.suppress_late_visible_updates_after_withdrawal = true;
        app.input = "/ultrawork 研究下核心玩法".into();
        app.cursor_offset = app.input.len();
        let mut session = make_test_tui_session();
        session.engine_half.update_rx = update_rx;
        let (_runtime, handle) = make_immediate_handle();
        let mut active = None;
        let mut pending_permission = None;

        assert!(!submit_or_queue(
            &mut app,
            "/ultrawork 研究下核心玩法".into(),
            &mut session,
            &handle,
            &mut active,
            &mut pending_permission,
            UiMode::Screen,
        ));
        assert!(!app.suppress_late_visible_updates_after_withdrawal);
        assert!(app.rebon_tui.overlay.is_empty());
        assert!(active.is_some(), "ultrawork submit should spawn a prompt");
    }

    #[test]
    fn slash_effort_bare_opens_picker_inline_and_screen() {
        for ui_mode in [UiMode::Screen, UiMode::Inline] {
            let (runtime, handle) = make_immediate_handle();
            let mut app = AppState::new();
            app.input = "/effort".into();
            app.cursor_offset = app.input.len();
            let mut session = make_test_tui_session();
            let mut active_prompt = None;

            assert!(!submit_or_queue(
                &mut app,
                "/effort".into(),
                &mut session,
                &handle,
                &mut active_prompt,
                &mut None,
                ui_mode,
            ));

            assert!(
                app.dialogs.top_id() == Some(rebon_ui_seat::ids::dialog::EFFORT),
                "{ui_mode:?}: /effort opens the picker"
            );
            assert!(app.input.is_empty());
            assert_eq!(app.cursor_offset, 0);
            assert!(active_prompt.is_none());
            assert!(app.rebon_tui.transcript.rows().is_empty());
            runtime.shutdown_background();
        }
    }

    #[test]
    fn slash_effort_max_emits_local_feedback_inline_and_screen() {
        let _env = EnvVarGuard::new(&["REBON_CONFIG_DIR"]);
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::set_var("REBON_CONFIG_DIR", tmp.path());
        }

        for ui_mode in [UiMode::Screen, UiMode::Inline] {
            let (runtime, handle) = make_immediate_handle();
            let mut app = AppState::new();
            app.input = "/effort max".into();
            app.cursor_offset = app.input.len();
            app.follow_transcript_tail = false;
            let mut session = make_test_tui_session();
            let mut active_prompt = None;

            let should_quit = submit_or_queue(
                &mut app,
                "/effort max".into(),
                &mut session,
                &handle,
                &mut active_prompt,
                &mut None,
                ui_mode,
            );

            drop(active_prompt.take());
            runtime.shutdown_background();

            assert!(!should_quit);
            assert_eq!(app.effort_level.map(|level| level.as_str()), Some("max"));
            assert!(!app.dialogs.is_open());
            assert!(app.input.is_empty());
            assert!(active_prompt.is_none());
            assert!(app.follow_transcript_tail);
            let rows = app.rebon_tui.transcript.rows();
            assert_eq!(rows.len(), 1, "{ui_mode:?}");
            let rebon_tui::Message::System(system) = &rows[0] else {
                panic!("{ui_mode:?}: expected local feedback system row");
            };
            assert_eq!(system.subtype, "local_command");
            assert!(system.uuid.starts_with("s-effort-"));
            let content = system.content.as_deref().unwrap_or("");
            assert!(content.contains("/effort max"), "{ui_mode:?}: {content}");
            assert!(
                content.contains("Set effort level to max"),
                "{ui_mode:?}: {content}"
            );
        }
    }

    #[test]
    fn slash_fast_updates_handle_with_local_feedback() {
        let _guard = crate::test_env::lock_env();
        let tmp = TempDir::new().unwrap();
        let previous_config_dir = std::env::var_os("REBON_CONFIG_DIR");
        std::env::set_var("REBON_CONFIG_DIR", tmp.path());

        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        session.model.service_tier_available = true;
        let mut active_prompt = None;
        // The feedback row only appears when there is a transcript to put it
        // in: on the empty startup screen `/fast` refreshes the banner
        // instead, which the test below covers. Put one row in first so this
        // test is about the row it is named after.
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_tui::Message::System(rebon_tui::SystemMessage {
                uuid: "seed".into(),
                timestamp: String::new(),
                subtype: "info".into(),
                content: Some("earlier turn".into()),
                level: None,
                is_meta: None,
            })),
        );

        let should_quit = submit_or_queue(
            &mut app,
            "/fast on".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        );

        match previous_config_dir {
            Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
            None => std::env::remove_var("REBON_CONFIG_DIR"),
        }
        drop(active_prompt.take());
        runtime.shutdown_background();

        assert!(!should_quit);
        assert!(session.model.service_tier.is_fast());
        assert!(app.input.is_empty());
        assert!(active_prompt.is_none());
        assert!(app.follow_transcript_tail);
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 2);
        let rebon_tui::Message::System(system) = &rows[1] else {
            panic!("expected local feedback system row");
        };
        assert_eq!(system.subtype, "local_command");
        assert!(system.uuid.starts_with("s-fast-"));
        let content = system.content.as_deref().unwrap_or("");
        assert!(content.contains("Fast mode"), "{content}");
        assert!(content.contains("enabled"), "{content}");
    }

    #[test]
    fn slash_fast_status_emits_local_feedback() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        session.model.service_tier_available = true;
        let mut active_prompt = None;

        let should_quit = submit_or_queue(
            &mut app,
            "/fast status".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        );

        drop(active_prompt.take());
        runtime.shutdown_background();

        assert!(!should_quit);
        assert!(app.input.is_empty());
        assert!(active_prompt.is_none());
        assert!(app.follow_transcript_tail);
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 1);
        let rebon_tui::Message::System(system) = &rows[0] else {
            panic!("expected local feedback system row");
        };
        assert_eq!(system.subtype, "local_command");
        assert!(system.uuid.starts_with("s-fast-"));
        let content = system.content.as_deref().unwrap_or("");
        assert!(content.contains("Fast mode"), "{content}");
    }

    #[test]
    fn slash_goal_set_starts_goal_prompt_immediately() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/goal ship the feature and verify cargo test".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let prompt_text = prompt_request_text(&requests[0]);
        assert_eq!(
            visible_prompt_text(&app),
            Some("ship the feature and verify cargo test")
        );
        let policy = requests[0]
            .execution_policy
            .as_ref()
            .expect("goal continuity policy");
        assert!(policy.auto_mode_script_continuity);
        assert!(policy.ultraplan.is_none());
        assert_goal_prompt(prompt_text, "ship the feature and verify cargo test");
        assert!(app.queued_commands.is_empty());
        assert_eq!(
            app.goal.as_ref().map(|goal| goal.prompt.as_str()),
            Some("ship the feature and verify cargo test")
        );
        let output = local_command_output(&app, "goal").expect("/goal feedback");
        assert_eq!(
            output,
            "/goal ship the feature and verify cargo test\n⎿ goal set: ship the feature and verify cargo test"
        );
        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn slash_goal_short_goal_asks_for_clarification_without_submitting() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/goal ship".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert!(app.goal.is_none());
        assert_eq!(
            app.pending_goal_clarification
                .as_ref()
                .map(|pending| pending.prompt.as_str()),
            Some("ship")
        );
        let output = local_command_output(&app, "goal").expect("clarification feedback");
        assert!(output.contains("Goal is too brief"), "{output}");
        assert!(output.contains("verification criteria"), "{output}");
        assert!(app.input.is_empty());
        drop(runtime);
    }

    #[test]
    fn goal_clarification_reply_refines_goal_and_starts_prompt() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/goal ship".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));
        assert!(!submit_or_queue(
            &mut app,
            "release the CLI after cargo test passes".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        runtime.block_on(tokio::task::yield_now());

        let refined_goal = "ship — release the CLI after cargo test passes";
        assert!(active_prompt.is_some());
        assert!(app.pending_goal_clarification.is_none());
        assert_eq!(
            app.goal.as_ref().map(|goal| goal.prompt.as_str()),
            Some(refined_goal)
        );
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert_goal_prompt(prompt_request_text(&requests[0]), refined_goal);
        assert_eq!(visible_prompt_text(&app), Some(refined_goal));
        let goal_outputs = goal_local_command_outputs(&app);
        let confirmation = goal_outputs
            .iter()
            .find(|output| output.contains("Goal clarified"))
            .copied()
            .expect("goal refinement confirmation");
        assert!(confirmation.contains(refined_goal), "{confirmation}");
        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn slash_goal_clear_clears_goal_and_pending_goal_continuation() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        app.goal = Some(crate::goal::GoalState::new_with_started_at("ship it", 10));
        app.pending_goal_clarification =
            Some(crate::goal::PendingGoalClarification::new("ship", Some(3)));
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
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/goal clear".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert!(app.goal.is_none());
        assert!(app.pending_goal_clarification.is_none());
        assert!(app.deferred_goal_submit_payloads.is_empty());
        assert_eq!(app.deferred_internal_submit_payloads.len(), 1);
        assert_eq!(
            local_command_output(&app, "goal").as_deref(),
            Some("/goal clear\n⎿ goal cleared")
        );
        drop(runtime);
    }

    #[test]
    fn slash_goal_stop_pauses_goal_without_submitting() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        app.goal = Some(crate::goal::GoalState::new_with_started_at("ship it", 10));
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/goal stop".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert!(app.goal.as_ref().is_some_and(|goal| goal.is_paused()));
        assert_eq!(
            local_command_output(&app, "goal").as_deref(),
            Some("/goal stop\n⎿ goal paused")
        );
        drop(runtime);
    }

    #[test]
    fn slash_goal_off_turns_goal_off_without_submitting() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        app.goal = Some(crate::goal::GoalState::new_with_started_at("ship it", 10));
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/goal off".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert!(app.goal.is_none());
        assert_eq!(
            local_command_output(&app, "goal").as_deref(),
            Some("/goal off\n⎿ goal turned off")
        );
        drop(runtime);
    }

    #[test]
    fn slash_goal_complete_existing_goal_opens_replace_confirmation() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let mut old_goal = crate::goal::GoalState::new_with_started_at("old goal", 10);
        old_goal.mark_complete(Some("done".into()));
        app.goal = Some(old_goal);
        let mut active_prompt = None;

        assert!(!submit_or_queue(
            &mut app,
            "/goal new goal".into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        ));

        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert_eq!(
            app.goal.as_ref().map(|goal| goal.prompt.as_str()),
            Some("old goal")
        );
        let dialog = app
            .goal_confirm_dialog
            .as_ref()
            .expect("replace confirmation");
        assert_eq!(dialog.prompt, "new goal");
        assert_eq!(dialog.max_sessions, None);
        assert!(local_command_output(&app, "goal").is_none());
        drop(runtime);
    }

    #[test]
    fn confirming_completed_goal_replacement_starts_goal_prompt_immediately() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let mut old_goal = crate::goal::GoalState::new_with_started_at("old goal", 10);
        old_goal.mark_complete(Some("done".into()));
        app.goal = Some(old_goal);
        app.goal_confirm_dialog = Some(
            crate::tui::goal_confirm_dialog::GoalConfirmDialogState::new(
                "new goal with cargo test verification".into(),
                None,
            ),
        );
        let mut active_prompt = None;

        handle_goal_confirm_replace(
            &mut app,
            "new goal with cargo test verification".into(),
            None,
            &mut session,
            &handle,
            &mut active_prompt,
        );
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let prompt_text = prompt_request_text(&requests[0]);
        assert_goal_prompt(prompt_text, "new goal with cargo test verification");
        assert_eq!(
            app.goal.as_ref().map(|goal| goal.prompt.as_str()),
            Some("new goal with cargo test verification")
        );
        assert!(!app.goal.as_ref().expect("goal").is_complete());
        assert_eq!(
            local_command_output(&app, "goal").as_deref(),
            Some("/goal new goal with cargo test verification\n⎿ goal set: new goal with cargo test verification")
        );
        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn confirming_archived_goal_replacement_starts_goal_prompt_immediately() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let mut old_goal = crate::goal::GoalState::new_with_started_at("old goal", 10);
        old_goal.mark_archived_with_time(20);
        app.goal = Some(old_goal);
        app.goal_confirm_dialog = Some(
            crate::tui::goal_confirm_dialog::GoalConfirmDialogState::new(
                "new archived replacement with cargo test verification".into(),
                Some(2),
            ),
        );
        let mut active_prompt = None;

        handle_goal_confirm_replace(
            &mut app,
            "new archived replacement with cargo test verification".into(),
            Some(2),
            &mut session,
            &handle,
            &mut active_prompt,
        );
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let prompt_text = prompt_request_text(&requests[0]);
        assert_goal_prompt(
            prompt_text,
            "new archived replacement with cargo test verification",
        );
        let goal = app.goal.as_ref().expect("goal");
        assert_eq!(
            goal.prompt,
            "new archived replacement with cargo test verification"
        );
        assert_eq!(goal.max_sessions, Some(2));
        assert!(!goal.is_archived());
        assert_eq!(
            local_command_output(&app, "goal").as_deref(),
            Some("/goal new archived replacement with cargo test verification\n⎿ goal set: new archived replacement with cargo test verification")
        );
        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn slash_update_invalid_submit_path_emits_usage_without_model_submit() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let mut active_prompt = None;
        let command = "/update check extra";
        app.input = command.into();
        app.cursor_offset = app.input.len();

        let should_quit = submit_or_queue(
            &mut app,
            command.into(),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
        );

        drop(active_prompt.take());
        runtime.shutdown_background();

        assert!(!should_quit);
        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
        assert!(active_prompt.is_none());
        assert!(app.follow_transcript_tail);
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 1);
        let rebon_tui::Message::System(system) = &rows[0] else {
            panic!("expected local feedback system row");
        };
        assert_eq!(system.subtype, "local_command");
        assert!(system.uuid.starts_with("s-update-"));
        let content = system.content.as_deref().unwrap_or("");
        assert!(
            content.contains(
                "Usage: /update [status|check|skip|channel <latest|stable>|auto <on|off|status>]"
            ),
            "{content}"
        );
    }
}
