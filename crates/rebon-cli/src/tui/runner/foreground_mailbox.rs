//! Foreground command-mailbox runtime.
//!
//! Gives the interactive TUI session a control channel an external process (the
//! GPUI desktop app) can drive: inject a prompt, stop the in-flight turn, or
//! answer a pending permission prompt. The transport is the on-disk mailbox +
//! status sidecar defined in [`rebon_session_host::foreground`]; this module is
//! the foreground half — it drains queued commands each idle event-loop pass and
//! applies them through the same in-loop functions a local keypress would
//! (`submit_or_queue`, the remote cancel, the permission `response_tx`), and it
//! publishes a status sidecar so the controller can render a live session's
//! busy/permission state and confirm command delivery.
//!
//! Only a session that holds its on-disk active lock (`session_active_lock`) is
//! the authoritative owner; without it another process owns the session and we
//! neither publish status nor drain its mailbox.
//!
//! **Retirement withdrawn — kept indefinitely**. That RFC once
//! scheduled this module for deletion one release after hosted
//! sessions became the default; on 8/31 hosted went back to being opt-in
//! (`--hosted`), so a plain `rebon` hosts its own session in-process again,
//! holds its own lock, and publishes no endpoint. That makes this mailbox the
//! desktop app's only way to reach the default session shape — the one it
//! resolves as `OwnedOpaque` and reaches through files — rather than an escape
//! hatch for `--local`. Hosted sessions still go through the endpoint, and the
//! module stays inert for them: a mirror holds no lock. Revisit retirement only
//! after the mirror's control and presentation faces are finished, hosted is
//! the default again, and one release cycle has passed; retiring then still
//! means deleting this module, the app's `send_foreground_command` fallback,
//! the `<sid>.live.json` writer and its readers, and
//! `rebon_session_host::foreground` together.

use tokio::runtime::Handle;

use rebon_session_host::{
    ForegroundCommand, ForegroundCommandOutcome, ForegroundQuestion, ForegroundQuestionAnswer,
    ForegroundQuestionOption,
};

use crate::background::{
    command_was_already_resolved, is_compact_command, validate_question_answer,
};
use crate::session_shell::session_command_inputs_from_app;
use crate::tui::app::AppState;
use crate::tui::permission_modal::{PendingPermission, PermissionKind, PermissionModalAction};
use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::UiMode;

use super::interrupt_flow::remote_cancel_active_prompt;
use super::permission_flow::{
    apply_permission_modal_action_for_session, set_permission_mode_for_session,
    submit_ask_user_question_if_ready,
};
use super::submit::submit_or_queue_with_images_and_uuid;
use super::ActivePrompt;

/// Runs one mailbox `RunCommand`.
///
/// `Ok(None)` means the answer is **deferred**: the command started work that
/// outlives this drain, and whatever owns that work writes the response
/// sidecar when it finishes. `/compact` is the only such command today — it
/// runs a ~30s provider call, and blocking the event loop on it would freeze
/// the local TUI for a command the local user did not type.
pub(super) type ForegroundCommandHandler =
    fn(
        &mut AppState,
        &mut TuiEngineSession,
        &Handle,
        &mut Option<ActivePrompt>,
        &str,
        &str,
        &[String],
    ) -> Result<Option<rebon_session_host::CommandOutput>, String>;

pub(super) fn execute_foreground_session_command(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    command_id: &str,
    name: &str,
    args: &[String],
) -> Result<Option<rebon_session_host::CommandOutput>, String> {
    if active_prompt.is_some()
        && name
            .trim()
            .trim_start_matches('/')
            .eq_ignore_ascii_case("kernel")
        && !args.is_empty()
        && !(args.len() == 1 && args[0].eq_ignore_ascii_case("list"))
    {
        return Err("Cannot switch kernels while a prompt is running.".to_string());
    }

    if let Some(output) = crate::background::permission_retry_blocked_by_running_turn(name, args) {
        if active_prompt.is_some() {
            return Ok(Some(output));
        }
        if let Some(reason) =
            super::prompt_lifecycle::local_turn_rejection(app, session, active_prompt.as_ref())
        {
            return Err(reason);
        }
    }

    // `/compact` at an idle prompt compacts immediately instead of arming the
    // next request, so the controller's answer has to wait for the run. With a
    // turn in flight the engine's in-turn manual path owns the history, so the
    // command falls through to the ordinary (deferred-flag) handling below and
    // answers straight away.
    if active_prompt.is_none() && is_compact_command(name) {
        let instructions = (!args.is_empty()).then(|| args.join(" "));
        return match super::compact_runtime::start_manual_compact(
            app,
            session,
            handle,
            instructions,
            Some(command_id.to_string()),
        ) {
            super::compact_runtime::CompactStart::Started => Ok(None),
            super::compact_runtime::CompactStart::Rejected(reason) => Err(reason),
        };
    }

    let inputs = session_command_inputs_from_app(app, session.ui_mode);
    let result = crate::session::commands::control::execute_session_control_command(
        &inputs, session, name, args,
    )?;
    if !result.replay_requests.is_empty() {
        super::prompt_lifecycle::prepare_for_new_prompt_after_withdrawal(
            app,
            &mut session.engine_half.update_rx,
        );
        *active_prompt = super::prompt_lifecycle::admit_permission_replay_turn(
            app,
            session,
            handle,
            active_prompt.as_ref(),
            super::LocalTurnSource::ForegroundPermissionRetry,
            result.replay_requests,
        );
    }
    Ok(Some(result.output))
}

/// The terminal half of the foreground mailbox: what a command *means*.
///
/// The traffic — draining claims, the idempotency bookkeeping, the status
/// sidecar — is [`crate::background::ForegroundMailbox`]. What is left here is
/// the part that needs a screen: every command is applied through the same
/// in-loop functions a local keypress would use, so injecting a prompt, stopping
/// a turn and answering a permission behave identically however they arrive.
pub(super) struct ForegroundControl {
    mailbox: crate::background::ForegroundMailbox,
    command_handler: Option<ForegroundCommandHandler>,
}

impl ForegroundControl {
    pub(super) fn new(session: &TuiEngineSession) -> Self {
        Self {
            mailbox: crate::background::ForegroundMailbox::new(session),
            command_handler: None,
        }
    }

    pub(super) fn set_command_handler(&mut self, handler: ForegroundCommandHandler) {
        self.command_handler = Some(handler);
    }

    /// Publish the status sidecar for what is on screen right now.
    ///
    /// The mailbox is told three facts rather than shown the screen: whether a
    /// turn is running, the pending permission as a background snapshot, and the
    /// questions an interactive prompt is asking. Reading those off the screen is
    /// the terminal's job; writing the file is not.
    pub(super) fn publish(
        &mut self,
        active_prompt: &Option<ActivePrompt>,
        pending_permission: &Option<PendingPermission>,
    ) -> bool {
        let busy = active_prompt.is_some();
        let pending_snapshot = pending_permission
            .as_ref()
            .map(|p| rebon_session_runtime::host::background_permission_snapshot(&p.outbound));
        // When the pending permission is an interactive AskUserQuestion, also
        // publish the parsed questions so the controller can render the prompt.
        let ask_user_questions = pending_permission
            .as_ref()
            .and_then(|p| ask_user_questions_snapshot(&p.view.kind));
        self.mailbox.publish(
            busy,
            pending_snapshot.as_ref(),
            ask_user_questions.as_deref(),
        )
    }

    /// Drain and apply every command queued for this session. Runs on the idle
    /// background-pipeline pass, so it never competes with hot key handling.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn drain(
        &mut self,
        app: &mut AppState,
        session: &mut TuiEngineSession,
        handle: &Handle,
        active_prompt: &mut Option<ActivePrompt>,
        pending_permission: &mut Option<PendingPermission>,
        ui_mode: UiMode,
    ) {
        self.mailbox.sync_session(session);
        if !self.mailbox.enabled() {
            return;
        }
        for claim in self.mailbox.claims() {
            let command_id = claim.envelope.command_id.clone();
            let is_run_command = matches!(
                &claim.envelope.command,
                ForegroundCommand::RunCommand { .. }
            );
            // Idempotency: a command already applied (for example when removing
            // its durable claim previously failed) is acknowledged again but not
            // re-run.
            if self.mailbox.already_processed(&command_id) {
                tracing::debug!(%command_id, "foreground mailbox: duplicate command ignored");
                // A redelivered `RunCommand` still owes its caller an answer: the
                // client blocks on the response sidecar, which the first pass
                // either never wrote or the reader has since consumed. Replay the
                // remembered payload rather than let it sit out the full timeout.
                if is_run_command {
                    let response = self
                        .mailbox
                        .remembered_response(&command_id)
                        .unwrap_or_else(|| rebon_session_host::ForegroundCommandResponse {
                            command_id: command_id.clone(),
                            processed_at_ms: rebon_session_host::now_ms(),
                            output: None,
                            error: Some("foreground command result is no longer cached".into()),
                        });
                    self.mailbox.write_response(&response);
                }
                if self.publish(active_prompt, pending_permission) {
                    self.mailbox.complete(&claim);
                }
                continue;
            }
            let result = self.apply(
                app,
                session,
                handle,
                active_prompt,
                pending_permission,
                ui_mode,
                command_id.as_str(),
                claim.envelope.command.clone(),
            );
            let processed_at_ms = rebon_session_host::now_ms();
            // A `RunCommand` that applied without producing output deferred its
            // answer (see [`ForegroundCommandHandler`]); the work that owns it
            // writes the response sidecar when it lands, so writing an empty
            // one here would hand the caller a "no output" error instead.
            let deferred_response = is_run_command && matches!(result, Ok(None));
            let (outcome, output, error) = match result {
                Ok(output) => (ForegroundCommandOutcome::Applied, output, None),
                Err(error) if command_was_already_resolved(&error) => {
                    (ForegroundCommandOutcome::AlreadyResolved, None, Some(error))
                }
                Err(error) => (ForegroundCommandOutcome::Rejected, None, Some(error)),
            };
            if is_run_command && !deferred_response {
                let response = rebon_session_host::ForegroundCommandResponse {
                    command_id: command_id.clone(),
                    processed_at_ms,
                    output,
                    error: error.clone(),
                };
                self.mailbox.write_response(&response);
                self.mailbox.remember_response(response);
            }
            self.mailbox.remember_processed(command_id.clone());
            self.mailbox
                .record_result(command_id, processed_at_ms, outcome, error);
            // The claim is removed only after the outcome is durably reflected in
            // the status acknowledgement. A crash before this point re-delivers
            // the claim at least once after restart.
            if self.publish(active_prompt, pending_permission) {
                self.mailbox.complete(&claim);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply(
        &self,
        app: &mut AppState,
        session: &mut TuiEngineSession,
        handle: &Handle,
        active_prompt: &mut Option<ActivePrompt>,
        pending_permission: &mut Option<PendingPermission>,
        ui_mode: UiMode,
        command_id: &str,
        command: ForegroundCommand,
    ) -> Result<Option<rebon_session_host::CommandOutput>, String> {
        match command {
            ForegroundCommand::Inject {
                message,
                user_message_uuid,
                images,
            } => {
                if message.trim().is_empty() {
                    return Err("empty inject message".to_string());
                }
                tracing::info!(target: "stream_dbg", image_count = images.len(), "foreground mailbox: inject prompt");
                // Runs the prompt now, or queues it behind the in-flight turn —
                // same path as a locally-typed submit. The "should exit" return
                // is ignored: a remote controller cannot quit the local TUI.
                let _ = submit_or_queue_with_images_and_uuid(
                    app,
                    message,
                    images,
                    user_message_uuid,
                    session,
                    handle,
                    active_prompt,
                    pending_permission,
                    ui_mode,
                );
                Ok(None)
            }
            ForegroundCommand::RunCommand { name, args } => {
                let handler = self
                    .command_handler
                    .ok_or_else(|| format!("command handler is not registered: {name}"))?;
                handler(
                    app,
                    session,
                    handle,
                    active_prompt,
                    command_id,
                    &name,
                    &args,
                )
            }
            ForegroundCommand::Stop => {
                tracing::info!(target: "stream_dbg", "foreground mailbox: stop turn");
                remote_cancel_active_prompt(app, session, active_prompt, ui_mode);
                Ok(None)
            }
            ForegroundCommand::AnswerPermission {
                query_id,
                option_id,
                extra_text,
            } => answer_pending_permission(
                app,
                session,
                pending_permission,
                query_id,
                option_id,
                extra_text,
            )
            .map(|_| None),
            ForegroundCommand::AnswerQuestions { query_id, answers } => {
                answer_ask_user_questions(app, session, pending_permission, query_id, answers)
                    .map(|_| None)
            }
            ForegroundCommand::SetPermissionMode { mode } => {
                // Reject rather than silently coercing: `from_wire` maps anything
                // unknown to `Default`, which would quietly *lower* the mode the
                // controller asked for and report success.
                let parsed = rebon_permissions::PermissionMode::from_wire(&mode);
                if parsed.as_wire() != mode {
                    return Err(format!("unknown permission mode `{mode}`"));
                }
                tracing::info!(mode = %mode, "foreground mailbox: set permission mode");
                // Same call the settings dialog and the Shift+Tab cycle use, so
                // the live mode cell the engine broker reads is updated here too
                // — that is what makes this take effect on the next tool call
                // rather than the next session.
                set_permission_mode_for_session(app, Some(session), parsed);
                Ok(None)
            }
        }
    }
}

/// Resolve a remote permission answer against the currently-pending prompt.
///
/// Selects the requested option in the live modal, then drives the **same**
/// confirm/cancel path a local keypress would (`apply_permission_modal_action_
/// for_session`). That is what makes allow-always rule persistence + option-id
/// canonicalization + ultraplan gating happen correctly — a raw
/// `response_tx.send` of the snapshot's (possibly synthetic, e.g.
/// `allow_always_generalized`) option id would not be understood by the engine
/// and the turn would hang.
///
/// Guards: `query_id` must match the live prompt (an answer racing a
/// just-replaced prompt, or arriving with nothing pending, is rejected rather
/// than misattributed), and a non-`None` `option_id` must name a real option.
/// Returns `Err(reason)` on any guard miss so the caller can surface it as the
/// command's ack error.
fn answer_pending_permission(
    app: &mut AppState,
    session: &TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    query_id: u64,
    option_id: Option<String>,
    extra_text: Option<String>,
) -> Result<(), String> {
    // Guard + select the requested option inside a scoped borrow, then drop it so
    // the modal-action machinery can take its own `&mut`.
    {
        let Some(pending) = pending_permission.as_mut() else {
            return Err("no permission pending".to_string());
        };
        if pending.outbound.id != query_id {
            return Err(format!(
                "stale permission answer: have q{}, got q{query_id}",
                pending.outbound.id
            ));
        }
        if let Some(option_id) = option_id.as_deref() {
            let idx = pending
                .view
                .options
                .iter()
                .position(|opt| opt.option_id == option_id)
                .ok_or_else(|| format!("unknown permission option_id '{option_id}'"))?;
            pending.view.selected = idx;
        }
        if let Some(text) = extra_text {
            pending.view.extra_text = text;
        }
    }
    // `option_id == None` ⇒ cancel/deny; otherwise confirm the selected option.
    let action = if option_id.is_some() {
        PermissionModalAction::Confirm
    } else {
        PermissionModalAction::Cancel
    };
    apply_permission_modal_action_for_session(
        app,
        session,
        pending_permission,
        action,
        &session.engine_half.live_policy_store,
        &session.cwd,
    );
    Ok(())
}

/// Build the controller-facing AskUserQuestion snapshot from a live modal kind.
/// `None` when the pending permission is an ordinary (non-question) prompt.
fn ask_user_questions_snapshot(kind: &PermissionKind) -> Option<Vec<ForegroundQuestion>> {
    let PermissionKind::AskUserQuestion { questions, .. } = kind else {
        return None;
    };
    Some(
        questions
            .iter()
            .map(|q| ForegroundQuestion {
                header: q.header.clone(),
                question: q.question.clone(),
                multi_select: q.multi_select,
                options: q
                    .options
                    .iter()
                    .map(|o| ForegroundQuestionOption {
                        label: o.label.clone(),
                        description: o.description.clone(),
                        preview: o.preview.clone(),
                    })
                    .collect(),
            })
            .collect(),
    )
}

/// Apply a remote answer to an interactive AskUserQuestion prompt: populate the
/// live modal's per-question answers from the command, then submit through the
/// same path a local keypress takes (`submit_ask_user_question_if_ready`), so the
/// rebuilt `updated_input` + ultraplan gating are identical to a local answer.
///
/// Guards: matching `query_id`, the pending prompt actually being an
/// AskUserQuestion, and an answer count matching the question count. Returns
/// `Err(reason)` (recorded as the command's ack error) on any guard miss or when
/// the supplied answers don't fully satisfy every question.
fn answer_ask_user_questions(
    app: &mut AppState,
    session: &TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    query_id: u64,
    answers_input: Vec<ForegroundQuestionAnswer>,
) -> Result<(), String> {
    {
        let Some(pending) = pending_permission.as_mut() else {
            return Err("no permission pending".to_string());
        };
        if pending.outbound.id != query_id {
            return Err(format!(
                "stale question answer: have q{}, got q{query_id}",
                pending.outbound.id
            ));
        }
        let PermissionKind::AskUserQuestion {
            ref questions,
            ref mut answers,
            ref mut active_question,
            ..
        } = pending.view.kind
        else {
            return Err("pending permission is not an interactive question".to_string());
        };
        if answers_input.len() != questions.len() {
            return Err(format!(
                "expected {} answers, got {}",
                questions.len(),
                answers_input.len()
            ));
        }
        for (index, (question, input)) in questions.iter().zip(&answers_input).enumerate() {
            validate_question_answer(question.options.len(), question.multi_select, input)
                .map_err(|error| format!("invalid answer {}: {error}", index + 1))?;
        }
        for (slot, input) in answers.iter_mut().zip(answers_input.into_iter()) {
            slot.selected_options = input.selected_options;
            slot.other_text = input
                .other_text
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_default();
            slot.other_cursor_offset = slot.other_text.len();
            slot.highlighted_row = 0;
        }
        *active_question = 0;
    }
    if submit_ask_user_question_if_ready(app, Some(session), pending_permission) {
        Ok(())
    } else {
        Err("interactive question not fully answered".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/compact` from the desktop app runs a ~30s provider call. Answering
    /// it inline would freeze the local TUI for a command its user never
    /// typed, so the handler starts the run and defers — `Ok(None)` — and the
    /// run writes the response sidecar when it lands.
    #[tokio::test]
    async fn a_compact_command_defers_its_answer_instead_of_blocking_the_loop() {
        let mut session = super::super::test_support::make_test_tui_session();
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();

        let answered = execute_foreground_session_command(
            &mut app,
            &mut session,
            &handle,
            &mut None,
            "cmd-compact",
            "compact",
            &[],
        );

        assert!(
            matches!(answered, Ok(None)),
            "compact must defer its answer, got {answered:?}"
        );
        assert!(app.compact_run.is_some(), "the run must be under way");
        assert_eq!(
            app.compact_run
                .as_ref()
                .and_then(|run| run.respond_to_command_id.clone())
                .as_deref(),
            Some("cmd-compact"),
            "the run has to know which mailbox command it owes an answer"
        );
    }

    /// Every other session command still answers inline, so the deferral
    /// path cannot swallow ordinary command output.
    #[tokio::test]
    async fn an_ordinary_session_command_still_answers_inline() {
        let mut session = super::super::test_support::make_test_tui_session();
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();

        let answered = execute_foreground_session_command(
            &mut app,
            &mut session,
            &handle,
            &mut None,
            "cmd-context",
            "context",
            &[],
        );

        assert!(matches!(answered, Ok(Some(_))), "got {answered:?}");
        assert!(app.compact_run.is_none());
    }

    #[tokio::test]
    async fn foreground_kernel_switch_is_rejected_while_a_prompt_is_active() {
        let mut session = super::super::test_support::make_test_tui_session();
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();
        let before = session.engine_half.runtime.session_agents.current_id();
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut active = Some(ActivePrompt::new(rx, rebon_types::PromptCancel::new()));

        let result = execute_foreground_session_command(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            "cmd-kernel",
            "kernel",
            &["dsh".into()],
        );

        assert!(result.is_err_and(|err| err.contains("prompt is running")));
        assert_eq!(
            session.engine_half.runtime.session_agents.current_id(),
            before
        );
    }

    /// A foreground session has no per-turn rebuild to pick up a stored mode,
    /// so the command has to land on the live mode cell — that cell is what the
    /// engine broker reads on every dispatch, which is what makes the switch
    /// take effect on the next tool call instead of the next session.
    #[tokio::test]
    async fn set_permission_mode_command_updates_the_live_mode_cell() {
        use rebon_permissions::PermissionMode;

        let mut session = super::super::test_support::make_test_tui_session();
        let control = ForegroundControl::new(&session);
        let mut app = AppState::new();
        app.permission_mode_cell = session.engine_half.permission_mode_cell.clone();
        let handle = tokio::runtime::Handle::current();

        let apply = |app: &mut AppState, session: &mut TuiEngineSession, mode: &str| {
            control.apply(
                app,
                session,
                &handle,
                &mut None,
                &mut None,
                UiMode::Screen,
                "cmd-test",
                ForegroundCommand::SetPermissionMode { mode: mode.into() },
            )
        };

        // Read the cell the way the engine broker does, so this asserts what the
        // permission gate will actually see rather than just where the value was
        // stored. `AutoModeHooks` is the broker's only view of the live mode.
        let provider_cell = std::sync::Arc::clone(&session.engine_half.permission_mode_cell);
        let hooks = rebon_permissions::denial_sink::AutoModeHooks::new(
            std::sync::Arc::new(rebon_permissions::denial_sink::NullDenialSink),
            std::sync::Arc::new(move || *provider_cell.lock().expect("mode cell")),
        );

        apply(&mut app, &mut session, "bypassPermissions").expect("bypass should apply");
        assert_eq!(
            *session
                .engine_half
                .permission_mode_cell
                .lock()
                .expect("mode cell"),
            PermissionMode::BypassPermissions
        );
        assert_eq!(app.permission_mode, PermissionMode::BypassPermissions);
        assert!(hooks.is_bypass(), "the broker must observe the new mode");

        // Switching back has to work too, or a session could never leave bypass.
        apply(&mut app, &mut session, "default").expect("default should apply");
        assert_eq!(
            *session
                .engine_half
                .permission_mode_cell
                .lock()
                .expect("mode cell"),
            PermissionMode::Default
        );
        assert!(
            !hooks.is_bypass(),
            "leaving bypass must reach the broker too"
        );

        // An unrecognized wire value must not silently coerce to Default and
        // report success — that would quietly lower the requested mode.
        let error = apply(&mut app, &mut session, "bypass")
            .expect_err("an unknown wire value must be rejected");
        assert!(error.contains("unknown permission mode"), "{error}");
        assert_eq!(
            *session
                .engine_half
                .permission_mode_cell
                .lock()
                .expect("mode cell"),
            PermissionMode::Default
        );
    }
}
