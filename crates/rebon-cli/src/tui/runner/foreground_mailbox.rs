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
//! holds its own lock, and publishes no *owner* endpoint. It does advertise a
//! narrow loopback agent endpoint in `.live.json` (see
//! `ForegroundControl::sync_agent_endpoint`), but control traffic still has no
//! endpoint: that makes this mailbox the desktop app's only way to reach the default
//! session shape — the one it resolves as `OwnedOpaque` and reaches through
//! files — rather than an escape hatch for `--local`. Hosted sessions still go
//! through the endpoint, and the module stays inert for them: a mirror holds
//! no lock. Revisit retirement only after the mirror's control and presentation
//! faces are finished, hosted is the default again, and one release cycle has
//! passed; retiring then still means deleting this module, the app's
//! `send_foreground_command` fallback, the `<sid>.live.json` writer and its
//! readers, and `rebon_session_host::foreground` together.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::runtime::Handle;

use rebon_session_host::{
    ForegroundCommand, ForegroundCommandOutcome, ForegroundQuestion, ForegroundQuestionAnswer,
    ForegroundQuestionOption,
};

use crate::background::{
    command_was_already_resolved, is_compact_command, validate_question_answer,
};
use crate::session::mid_turn_queue::DeferredAgentMessage;
use crate::session::submit_payload::SubmitPayload;
use crate::session_shell::session_command_inputs_from_app;
use crate::tui::app::AppState;
use crate::tui::permission_modal::{PendingPermission, PermissionKind, PermissionModalAction};
use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::UiMode;

use super::interrupt_flow::remote_cancel_active_prompt;
use super::live_agent_view::with_main_agent_view;
use super::permission_flow::{
    apply_permission_modal_action_for_session, set_permission_mode_for_session,
    submit_ask_user_question_if_ready,
};
use super::prompt_lifecycle::{admit_active_prompt, local_turn_rejection};
use super::submit::{submit_or_queue_from, TypedLine};
use super::ActivePrompt;
use crate::tui::dispatch::apply_internal_submit;

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
    /// Messages accepted by the loopback endpoint, waiting for the event loop.
    agent_inbox: Arc<rebon_session_host::ForegroundAgentInbox>,
    /// The listener, alive only while this session holds the lock.
    agent_endpoint: Option<rebon_session_host::ForegroundAgentEndpoint>,
    /// The session the listener was started for, so a session switch restarts
    /// it on the new session's project directory instead of keeping the old.
    agent_endpoint_session: Option<(PathBuf, String, String)>,
    /// Where this session's waiting agent messages are kept on disk, once it
    /// holds the lock.
    waiting_path: Option<PathBuf>,
    /// What that file holds, so it is rewritten only when the set changes.
    waiting_saved: Vec<rebon_session_host::WaitingAgentMessage>,
    /// The session whose file was last read back. Reading happens once per
    /// session: after that, memory is what is waiting and the file follows it.
    waiting_restored_for: Option<(PathBuf, String, String)>,
}

impl ForegroundControl {
    pub(super) fn new(session: &TuiEngineSession) -> Self {
        Self {
            mailbox: crate::background::ForegroundMailbox::new(session),
            command_handler: None,
            agent_inbox: Arc::new(rebon_session_host::ForegroundAgentInbox::for_session(
                &session.session_id,
            )),
            agent_endpoint: None,
            agent_endpoint_session: None,
            waiting_path: None,
            waiting_saved: Vec::new(),
            waiting_restored_for: None,
        }
    }

    pub(super) fn set_command_handler(&mut self, handler: ForegroundCommandHandler) {
        self.command_handler = Some(handler);
    }

    /// Publish the status sidecar for what is on screen right now.
    ///
    /// The mailbox is told four facts rather than shown the screen: whether a
    /// turn is running, the pending permission as a background snapshot, the
    /// questions an interactive prompt is asking, and what the running turn is
    /// working on. Reading those off the screen is the terminal's job; writing
    /// the file is not.
    pub(super) fn publish(
        &mut self,
        active_prompt: &Option<ActivePrompt>,
        pending_permission: &Option<PendingPermission>,
        paused: bool,
        activity: Option<&str>,
    ) -> bool {
        let busy = active_prompt.is_some();
        // The listener answers a sender before the event loop can act, so it
        // needs the same two facts the status sidecar carries: a running turn
        // takes the message, a paused terminal leaves it queued.
        self.agent_inbox.set_busy(busy);
        self.agent_inbox.set_blocked(paused);
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
            activity,
        )
    }

    /// Start, keep, or stop the loopback agent endpoint with the mailbox.
    ///
    /// A listener belongs to the process that holds the lock. Handing the
    /// session to a worker drops the lock, and this drops the listener with it;
    /// switching to another session's project directory restarts it there.
    fn sync_agent_endpoint(&mut self, session: &TuiEngineSession) {
        if !self.mailbox.enabled() {
            self.stop_agent_endpoint();
            return;
        }
        let identity = (
            session.projects_root.clone(),
            session.cwd.clone(),
            session.session_id.clone(),
        );
        if self.agent_endpoint.is_some() && self.agent_endpoint_session.as_ref() == Some(&identity)
        {
            return;
        }
        // The inbox outlives a session switch but its bounds must not: what the
        // old session had been told says nothing about the new one.
        self.agent_inbox.set_session(&identity.2);
        self.stop_agent_endpoint();
        // Before the listener can accept anything new: what this session was
        // still holding when its last owner exited comes back first.
        self.restore_waiting_agent_messages(session, &identity);
        // Remembered even when start fails: retrying every event-loop pass
        // would bind and discard a port each time, and the mailbox capability
        // is still advertised either way.
        self.agent_endpoint_session = Some(identity.clone());
        match rebon_session_host::ForegroundAgentEndpoint::start(
            &identity.0,
            &identity.1,
            &identity.2,
            self.agent_inbox.clone(),
        ) {
            Ok(endpoint) => {
                self.mailbox.set_agent_endpoint(Some(endpoint.info()));
                self.agent_endpoint = Some(endpoint);
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    session_id = %identity.2,
                    "rebon: could not start the foreground agent endpoint; the mailbox capability remains"
                );
                self.mailbox.set_agent_endpoint(None);
            }
        }
    }

    /// Put back the agent messages this session's file says were still
    /// waiting, and remember the file for [`Self::record_waiting_agent_messages`].
    ///
    /// They come back as riders: the person who quit did not get to see them,
    /// so they join the next turn, but reopening a session is not a reason for
    /// it to start one on its own.
    ///
    /// Once per session. The listener failing to start, or the lock being lost
    /// and taken back, brings this session here again, and by then the file
    /// may still list a message a turn has since read; memory is right and the
    /// next snapshot corrects the file.
    ///
    /// On a switch from another session, the inbox still holds that session's
    /// messages (it outlives the runtime). They go back to that session's file,
    /// so they reach it when it is reopened rather than riding into this one.
    fn restore_waiting_agent_messages(
        &mut self,
        session: &TuiEngineSession,
        identity: &(PathBuf, String, String),
    ) {
        if self.waiting_restored_for.as_ref() == Some(identity) {
            return;
        }
        self.hand_back_waiting_agent_messages(session);
        self.waiting_restored_for = Some(identity.clone());
        let path =
            rebon_session_host::waiting_agent_messages_path(&identity.0, &identity.1, &identity.2);
        let saved = rebon_session_host::load_waiting_agent_messages(&path);
        let held = waiting_agent_messages(session);
        let returning = saved
            .iter()
            .filter(|message| {
                !held
                    .iter()
                    .any(|waiting| waiting.message_id == message.message_id)
            })
            .map(|message| DeferredAgentMessage {
                message_id: message.message_id.clone(),
                text: message.text.clone(),
                from_session_id: message.from_session_id.clone(),
                hops: message.hops,
                verdict: rebon_tool::peer::WakeVerdict::Refused,
            })
            .collect::<Vec<_>>();
        if !returning.is_empty() {
            tracing::info!(
                count = returning.len(),
                session_id = %identity.2,
                "rebon: restored agent messages still waiting from the last run"
            );
            let mid_turn_queue = &session.engine_half.runtime.mid_turn_queue;
            session
                .engine_half
                .session_loop
                .inbox()
                .push_agent_messages(returning, &mid_turn_queue.reply_ledger());
        }
        self.waiting_saved = saved;
        self.waiting_path = Some(path);
    }

    /// Move the agent messages the inbox holds for the session it last served
    /// into that session's file, beside what the file already listed.
    fn hand_back_waiting_agent_messages(&mut self, session: &TuiEngineSession) {
        let Some(path) = self.waiting_path.take() else {
            return;
        };
        let mut waiting = std::mem::take(&mut self.waiting_saved);
        let held = session
            .engine_half
            .session_loop
            .inbox()
            .take_agent_items()
            .into_iter()
            .filter_map(waiting_from_context_item);
        for message in held {
            if !waiting
                .iter()
                .any(|seen| seen.message_id == message.message_id)
            {
                waiting.push(message);
            }
        }
        if let Err(error) = rebon_session_host::save_waiting_agent_messages(&path, &waiting) {
            tracing::warn!(
                %error,
                path = %path.display(),
                "rebon: could not hand waiting agent messages back to the session they were sent to"
            );
        }
    }

    /// Write what is still waiting to this session's file when it changed.
    fn record_waiting_agent_messages(&mut self, session: &TuiEngineSession) {
        let Some(path) = self.waiting_path.as_ref() else {
            return;
        };
        let waiting = waiting_agent_messages(session);
        if waiting == self.waiting_saved {
            return;
        }
        match rebon_session_host::save_waiting_agent_messages(path, &waiting) {
            Ok(()) => self.waiting_saved = waiting,
            Err(error) => tracing::warn!(
                %error,
                path = %path.display(),
                "rebon: could not record the agent messages still waiting"
            ),
        }
    }

    fn stop_agent_endpoint(&mut self) {
        self.agent_endpoint = None;
        self.agent_endpoint_session = None;
        self.mailbox.set_agent_endpoint(None);
    }

    /// Move accepted agent messages into the turn they belong to.
    ///
    /// A running turn takes them at its next tool round; an idle session leaves
    /// them where the idle-context trigger will start a turn from. Anything the
    /// running turn never reached — it ended first, or it is an external agent
    /// with no tool rounds at all — comes back out through
    /// `take_pending_agent_messages` and joins the same idle path.
    ///
    /// A message the listener accepted but refused a turn of its own goes into
    /// the inbox without being a reason to start one: it rides into the next
    /// turn somebody else starts. Dropping it instead would lose a message the
    /// sender was told had been accepted. One that only joined a running turn
    /// was never judged; if it comes back unread, the inbox judges it before it
    /// may start a turn, so writing while a turn runs is no way past the bounds.
    fn apply_pending_agent_messages(
        &self,
        session: &TuiEngineSession,
        active_prompt: &Option<ActivePrompt>,
    ) {
        let pending = self.agent_inbox.take_pending_messages();
        let mid_turn_queue = &session.engine_half.runtime.mid_turn_queue;
        if active_prompt.is_some() {
            for accepted in pending {
                let framed = rebon_session_host::frame_agent_message(&accepted.envelope);
                mid_turn_queue.enqueue_agent_message(
                    accepted.envelope.message_id.clone(),
                    framed,
                    &accepted.envelope.from_session_id,
                    accepted.envelope.hops,
                    accepted.verdict,
                );
            }
            return;
        }
        let accepted = pending.into_iter().map(|accepted| DeferredAgentMessage {
            text: rebon_session_host::frame_agent_message(&accepted.envelope),
            message_id: accepted.envelope.message_id,
            from_session_id: accepted.envelope.from_session_id,
            hops: accepted.envelope.hops,
            verdict: accepted.verdict,
        });
        session
            .engine_half
            .session_loop
            .inbox()
            .push_agent_messages(
                mid_turn_queue
                    .take_pending_agent_messages()
                    .into_iter()
                    .chain(accepted),
                &mid_turn_queue.reply_ledger(),
            );
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
        self.sync_agent_endpoint(session);
        if !self.mailbox.enabled() {
            return;
        }
        self.apply_pending_agent_messages(session, active_prompt);
        self.record_waiting_agent_messages(session);
        let activity = current_streaming_activity(app);
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
                if self.publish(
                    active_prompt,
                    pending_permission,
                    app.resume_dialog.is_some(),
                    activity.as_deref(),
                ) {
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
            if self.publish(
                active_prompt,
                pending_permission,
                app.resume_dialog.is_some(),
                current_streaming_activity(app).as_deref(),
            ) {
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
                // Never read as `@name message`: only a line typed at this
                // terminal is an address.
                let _ = submit_or_queue_from(
                    app,
                    message,
                    images,
                    user_message_uuid,
                    session,
                    handle,
                    active_prompt,
                    pending_permission,
                    ui_mode,
                    TypedLine::Injected,
                );
                Ok(None)
            }
            ForegroundCommand::GroupPrompt { message } => {
                if message.trim().is_empty() {
                    return Err("empty group message".to_string());
                }
                let submit = SubmitPayload {
                    text: message,
                    model_text: None,
                    user_message_uuid: Some(format!("u-internal-group-{command_id}")),
                    image_pastes: Vec::new(),
                    directory_attachments: Vec::new(),
                    execution_policy: None,
                    skill_invocations: Vec::new(),
                };
                if active_prompt.is_some() || app.goal_check.is_pending() {
                    app.deferred_internal_submit_payloads.push(submit);
                    return Ok(None);
                }
                if let Some(reason) = local_turn_rejection(app, session, None) {
                    return Err(reason);
                }
                with_main_agent_view(app, |app| {
                    let submit = apply_internal_submit(app, submit, &session.session_id)
                        .expect("group prompt is nonempty");
                    if let Some(active) = admit_active_prompt(
                        app,
                        session,
                        handle,
                        None,
                        super::LocalTurnSource::IdleContext,
                        submit,
                    ) {
                        *active_prompt = Some(active);
                        app.is_loading = true;
                        Ok(None)
                    } else {
                        Err("group prompt could not start".into())
                    }
                })
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
            command @ ForegroundCommand::AgentMessage { .. } => {
                // The mailbox rung of the same delivery the endpoint serves.
                // The inbox is shared, so a message id repeated across the two
                // paths is still delivered once.
                if let Some(envelope) = command.into_agent_message() {
                    let _ = self.agent_inbox.accept(envelope);
                    self.apply_pending_agent_messages(session, active_prompt);
                }
                Ok(None)
            }
        }
    }
}

impl Drop for ForegroundControl {
    /// A message the listener accepted after the last event-loop pass never
    /// reached the session's inbox, so the last snapshot does not list it.
    /// Close the listener first, so nothing more arrives, then add whatever it
    /// was still holding to the file.
    fn drop(&mut self) {
        self.stop_agent_endpoint();
        let Some(path) = self.waiting_path.as_ref() else {
            return;
        };
        let arrived = self.agent_inbox.take_pending_messages();
        if arrived.is_empty() {
            return;
        }
        let mut waiting = std::mem::take(&mut self.waiting_saved);
        for accepted in arrived {
            if waiting
                .iter()
                .any(|message| message.message_id == accepted.envelope.message_id)
            {
                continue;
            }
            waiting.push(rebon_session_host::WaitingAgentMessage {
                text: rebon_session_host::frame_agent_message(&accepted.envelope),
                message_id: accepted.envelope.message_id,
                from_session_id: accepted.envelope.from_session_id,
                hops: accepted.envelope.hops,
            });
        }
        if let Err(error) = rebon_session_host::save_waiting_agent_messages(path, &waiting) {
            tracing::warn!(
                %error,
                path = %path.display(),
                "rebon: could not record the agent messages that arrived while exiting"
            );
        }
    }
}

/// Every agent message this session holds and no turn has read yet: those
/// waiting in its inbox for the next turn, then those queued for the running
/// turn's next tool round. Each appears once.
fn waiting_agent_messages(
    session: &TuiEngineSession,
) -> Vec<rebon_session_host::WaitingAgentMessage> {
    use crate::session::session_inbox::ContextSource;
    let inbox = session
        .engine_half
        .session_loop
        .inbox()
        .pending_context_items(ContextSource::Agent)
        .into_iter()
        .filter_map(waiting_from_context_item);
    let queued = session
        .engine_half
        .runtime
        .mid_turn_queue
        .pending_agent_messages()
        .into_iter()
        .map(|message| rebon_session_host::WaitingAgentMessage {
            message_id: message.message_id,
            from_session_id: message.from_session_id,
            hops: message.hops,
            text: message.text,
        });
    let mut waiting: Vec<rebon_session_host::WaitingAgentMessage> = Vec::new();
    for message in inbox.chain(queued) {
        if !waiting
            .iter()
            .any(|seen| seen.message_id == message.message_id)
        {
            waiting.push(message);
        }
    }
    waiting
}

/// The record of a waiting inbox item, when it is another session's message.
fn waiting_from_context_item(
    item: crate::session::session_inbox::ContextItem,
) -> Option<rebon_session_host::WaitingAgentMessage> {
    let message_id = item.agent_message_id?;
    let (from_session_id, hops) = item.inbound?;
    Some(rebon_session_host::WaitingAgentMessage {
        message_id,
        from_session_id,
        hops,
        text: item.text,
    })
}

/// What the running turn is working on, for the status sidecar.
///
/// The peer list shows this next to a session's name, so it has to be the same
/// sentence the transcript row carries: the newest tool still in flight, named
/// and summarised the way the tool card names it. A turn that has only produced
/// text has nothing to report — "thinking" tells a reader less than nothing
/// does, and the busy flag already says a turn is running.
pub(super) fn current_streaming_activity(app: &AppState) -> Option<String> {
    app.rebon_tui
        .overlay
        .blocks
        .iter()
        .rev()
        .find_map(|block| match block {
            rebon_tui::StreamingContentBlock::ToolUse(tool)
                if matches!(
                    tool.status,
                    rebon_types::ToolCallStatus::Pending | rebon_types::ToolCallStatus::InProgress
                ) =>
            {
                let name = rebon_render::streaming_tool_display_name(
                    &tool.tool_name,
                    tool.raw_input.as_ref(),
                );
                let summary = tool
                    .raw_input
                    .as_ref()
                    .map(|input| rebon_render::streaming_tool_summary(&tool.tool_name, input))
                    .unwrap_or_default();
                Some(if summary.trim().is_empty() {
                    name.to_string()
                } else {
                    format!("{name} {summary}")
                })
            }
            _ => None,
        })
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
    use crate::session::session_inbox::ContextSource;

    /// A running turn takes an agent message at its next tool round, as a
    /// visible runtime attachment carrying the frame another agent wrote — never
    /// as a queued user submit.
    ///
    /// The attachment marker is what makes the injection a transcript row a
    /// resume can draw again; the frame's `from_handle` is what lets that row name
    /// its sender instead of a session id.
    #[tokio::test]
    async fn an_agent_message_joins_a_running_turn_as_a_visible_attachment() {
        use rebon_core::query::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};

        let mut session = super::super::test_support::make_test_tui_session();
        let control = ForegroundControl::new(&session);
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut active = Some(ActivePrompt::new(rx, rebon_types::PromptCancel::new()));
        let command = ForegroundCommand::AgentMessage {
            message_id: "m-running".into(),
            from_session_id: "sess-sender".into(),
            from_agent: "main".into(),
            from_cwd: None,
            from_handle: Some("repo-qrst".into()),
            from_name: Some("worker".into()),
            summary: Some("handoff".into()),
            text: "please continue".into(),
            hops: 0,
        };

        let applied = control.apply(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            "cmd-agent-1",
            command.clone(),
        );
        assert!(matches!(applied, Ok(None)), "got {applied:?}");
        assert!(
            !session.engine_half.session_loop.inbox().has_context(),
            "a running turn does not wait for the idle trigger"
        );
        assert_eq!(session.engine_half.runtime.mid_turn_queue.pending_len(), 1);

        let messages = session
            .engine_half
            .runtime
            .mid_turn_queue
            .poll(AttachmentPollRequest::new(
                "session",
                "turn",
                1,
                AttachmentPollPhase::Regular,
            ));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, rebon_api::Role::User);
        let text = serde_json::to_string(&messages[0]).unwrap();
        assert!(
            text.contains(
                "<rebon-visible-runtime-attachment uuid=\\\"u-agent-message-m-running\\\">"
            ),
            "the injection is a visible attachment, not a bare user message: {text}"
        );
        assert!(text.contains("<teammate-message"), "{text}");
        assert!(
            text.contains("from_handle=\\\"repo-qrst\\\""),
            "the frame names its sender by handle, which is what the row shows: {text}"
        );
        assert!(text.contains("please continue"), "{text}");
        assert!(
            !text.contains("rebon-queued-user-input"),
            "an agent message must not carry the queued-user marker: {text}"
        );
        assert!(
            app.input.is_empty(),
            "nothing may appear in the prompt input"
        );

        // A retry with the same message id is answered but delivered once; the
        // inbox is shared with the mailbox, which is what makes that hold across
        // both transports.
        let again = control.apply(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            "cmd-agent-2",
            command,
        );
        assert!(matches!(again, Ok(None)), "got {again:?}");
        assert_eq!(
            session.engine_half.runtime.mid_turn_queue.pending_len(),
            0,
            "a duplicate id must not be queued a second time"
        );
    }

    #[tokio::test]
    async fn group_prompt_reaches_model_without_becoming_user_intent_or_user_row() {
        let mut session = super::super::test_support::make_test_tui_session();
        let recorder = Arc::new(RecordingExecutor::default());
        session.set_test_executor(recorder.clone());
        let control = ForegroundControl::new(&session);
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();
        let mut active = None;
        let text = "[Agent group \"ops\"] planner asks (r4): review the patch";
        let outcome = control.apply(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            "cmd-group-1",
            ForegroundCommand::GroupPrompt {
                message: text.into(),
            },
        );
        assert!(matches!(outcome, Ok(None)), "{outcome:?}");
        assert!(active.is_some());
        let request = recorder.first_request().await;
        assert_eq!(request.user_prompt, None);
        assert_eq!(
            request.user_message_uuid.as_deref(),
            Some("u-internal-group-cmd-group-1")
        );
        assert!(request.prompt.iter().any(|block| matches!(
            block,
            rebon_types::ContentBlock::Text(body) if body.text.contains(text)
        )));
        assert!(app
            .rebon_tui
            .transcript
            .rows()
            .iter()
            .any(|row| matches!(row,
                rebon_tui::Message::System(system) if system.subtype == "group_message"
                    && system.content.as_deref() == Some(text)
            )));
        assert!(!app
            .rebon_tui
            .transcript
            .rows()
            .iter()
            .any(|row| matches!(row, rebon_tui::Message::User(_))));
    }

    #[tokio::test]
    async fn group_prompt_queues_as_internal_when_turn_is_running() {
        let mut session = super::super::test_support::make_test_tui_session();
        let recorder = Arc::new(RecordingExecutor::default());
        session.set_test_executor(recorder.clone());
        let control = ForegroundControl::new(&session);
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut active = Some(ActivePrompt::new(rx, rebon_types::PromptCancel::new()));
        let result = control.apply(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            "cmd-group-2",
            ForegroundCommand::GroupPrompt {
                message: "planner asks: review the patch".into(),
            },
        );
        assert!(matches!(result, Ok(None)), "{result:?}");
        assert!(app.queued_commands.is_empty());
        assert_eq!(app.deferred_internal_submit_payloads.len(), 1);
        assert_eq!(
            app.deferred_internal_submit_payloads[0]
                .user_message_uuid
                .as_deref(),
            Some("u-internal-group-cmd-group-2")
        );
        let empty = control.apply(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            "cmd-empty-group",
            ForegroundCommand::GroupPrompt {
                message: "   ".into(),
            },
        );
        assert_eq!(empty.unwrap_err(), "empty group message");
        assert_eq!(app.deferred_internal_submit_payloads.len(), 1);

        active = None;
        super::super::prompt_lifecycle::maybe_spawn_next_queued_prompt(
            &mut app,
            &mut session,
            &handle,
            &mut active,
        );
        assert!(active.is_some());
        assert!(app.deferred_internal_submit_payloads.is_empty());
        let request = recorder.first_request().await;
        assert_eq!(request.user_prompt, None);
        assert_eq!(
            request.user_message_uuid.as_deref(),
            Some("u-internal-group-cmd-group-2")
        );
    }

    /// The requests a turn hands its executor, so a test can read what the
    /// turn was started with.
    #[derive(Default)]
    struct RecordingExecutor {
        requests: std::sync::Mutex<Vec<rebon_agent_core::PromptRequest>>,
    }

    #[async_trait::async_trait]
    impl rebon_agent_core::PromptExecutor for RecordingExecutor {
        async fn execute(
            &self,
            request: rebon_agent_core::PromptRequest,
        ) -> Result<rebon_agent_core::PromptOutcome, rebon_agent_core::PromptExecutorError>
        {
            self.requests.lock().expect("requests lock").push(request);
            Ok(rebon_agent_core::PromptOutcome::end_turn())
        }
    }

    impl RecordingExecutor {
        async fn first_request(&self) -> rebon_agent_core::PromptRequest {
            for _ in 0..200 {
                if let Some(request) = self.requests.lock().expect("requests lock").first() {
                    return request.clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("the turn never reached its executor");
        }
    }

    fn agent_message(message_id: &str, from: &str, text: &str, hops: u8) -> ForegroundCommand {
        ForegroundCommand::AgentMessage {
            message_id: message_id.into(),
            from_session_id: from.into(),
            from_agent: "main".into(),
            from_cwd: None,
            from_handle: None,
            from_name: None,
            summary: None,
            text: text.into(),
            hops,
        }
    }

    /// An idle session hands the message to the idle-context turn rather than
    /// the prompt input: it waits in the model-context queue, which
    /// `admit_idle_context_prompt` drains into the turn it starts. The turn it
    /// starts carries the frame and offers nothing as user intent: another
    /// agent's message is not what a person typed, and its prompt row is named
    /// for the message so the transcript does not say so either.
    #[tokio::test]
    async fn an_agent_message_at_an_idle_prompt_starts_a_context_turn() {
        let mut session = super::super::test_support::make_test_tui_session();
        let recorder = Arc::new(RecordingExecutor::default());
        session.set_test_executor(recorder.clone());
        let control = ForegroundControl::new(&session);
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();
        let mut active = None;

        let applied = control.apply(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            "cmd-agent-idle",
            ForegroundCommand::AgentMessage {
                message_id: "m-idle".into(),
                from_session_id: "sess-sender".into(),
                from_agent: "main".into(),
                from_cwd: None,
                from_handle: None,
                from_name: None,
                summary: None,
                text: "wake up".into(),
                hops: 0,
            },
        );
        assert!(matches!(applied, Ok(None)), "got {applied:?}");
        assert!(
            session.engine_half.runtime.mid_turn_queue.pending_len() == 0,
            "an idle session has no tool round to wait for"
        );
        let pending = session
            .engine_half
            .session_loop
            .inbox()
            .pending_context(ContextSource::Agent);
        assert_eq!(pending.len(), 1);
        assert!(pending[0].starts_with("<teammate-message"));
        assert!(pending[0].contains("wake up"));
        assert!(app.input.is_empty());

        super::super::prompt_lifecycle::maybe_spawn_ready_idle_turn(
            &mut app,
            &session,
            &handle,
            &mut active,
        );
        assert!(active.is_some(), "the message starts the turn it came for");
        assert!(!session.engine_half.session_loop.inbox().has_context());
        let request = recorder.first_request().await;
        assert_eq!(
            request.user_prompt, None,
            "another agent's message must not reach the classifier as user intent"
        );
        assert_eq!(
            request.user_message_uuid.as_deref(),
            Some("u-agent-message-m-idle"),
            "the prompt row is named for the message, which marks it as not the person's words"
        );
        let prompt = serde_json::to_string(&request.prompt).unwrap();
        assert!(prompt.contains("wake up"), "{prompt}");
        assert!(prompt.contains("teammate-message"), "{prompt}");
    }

    /// A message whose chain has gone too deep is still delivered — but not by
    /// starting a turn of its own. The idle trigger must not see it as a reason
    /// to run, and the next turn somebody else starts must carry it.
    #[tokio::test]
    async fn an_agent_message_past_the_hop_bound_waits_for_the_next_turn() {
        let mut session = super::super::test_support::make_test_tui_session();
        let control = ForegroundControl::new(&session);
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();
        let mut active = None;

        let applied = control.apply(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            "cmd-agent-deep",
            ForegroundCommand::AgentMessage {
                message_id: "m-deep".into(),
                from_session_id: "sess-sender".into(),
                from_agent: "main".into(),
                from_cwd: None,
                from_handle: None,
                from_name: None,
                summary: None,
                text: "the fifth hand-off".into(),
                hops: rebon_tool::peer::MAX_AGENT_MESSAGE_HOPS + 1,
            },
        );
        assert!(matches!(applied, Ok(None)), "got {applied:?}");

        assert!(
            session.engine_half.session_loop.inbox().has_context(),
            "an accepted message is never dropped"
        );
        assert!(
            !session
                .engine_half
                .session_loop
                .inbox()
                .has_waking_context(),
            "a chain past the bound may not start a turn"
        );

        // A later message the bounds do allow is a reason to run; the waiting
        // one rides into that turn instead of being lost.
        let allowed = control.apply(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            "cmd-agent-allowed",
            ForegroundCommand::AgentMessage {
                message_id: "m-allowed".into(),
                from_session_id: "sess-other".into(),
                from_agent: "main".into(),
                from_cwd: None,
                from_handle: None,
                from_name: None,
                summary: None,
                text: "a fresh question".into(),
                hops: 0,
            },
        );
        assert!(matches!(allowed, Ok(None)), "got {allowed:?}");
        assert!(session
            .engine_half
            .session_loop
            .inbox()
            .has_waking_context());

        let items = super::super::context_prompts::drain_model_context_items(&session);
        assert_eq!(items.len(), 2, "both messages are delivered");
        assert!(
            items[0].text.contains("the fifth hand-off"),
            "{:?}",
            items[0].text
        );
        assert!(!items[0].may_wake);
        assert!(
            items[1].text.contains("a fresh question"),
            "{:?}",
            items[1].text
        );
        assert!(items[1].may_wake);
        assert_eq!(
            items[0].inbound,
            Some((
                "sess-sender".to_string(),
                rebon_tool::peer::MAX_AGENT_MESSAGE_HOPS + 1
            )),
            "the depth travels with the message, not with the turn"
        );
    }

    /// A message still waiting when the terminal quits is on disk, comes back
    /// the next time the session is opened without starting a turn of its own,
    /// and leaves the file once a turn has carried it.
    #[tokio::test]
    async fn a_waiting_agent_message_survives_the_terminal_exiting() {
        let root = tempfile::tempdir().expect("tempdir");
        let identity = (
            root.path().to_path_buf(),
            "/work/repo".to_string(),
            "sess-waiting".to_string(),
        );
        let path =
            rebon_session_host::waiting_agent_messages_path(&identity.0, &identity.1, &identity.2);

        // First run: a message past the hop bound waits, and is recorded.
        {
            let mut session = super::super::test_support::make_test_tui_session();
            let mut control = ForegroundControl::new(&session);
            control.restore_waiting_agent_messages(&session, &identity);
            let mut app = AppState::new();
            let handle = tokio::runtime::Handle::current();
            let mut active = None;
            let applied = control.apply(
                &mut app,
                &mut session,
                &handle,
                &mut active,
                &mut None,
                UiMode::Screen,
                "cmd-agent-deep",
                agent_message(
                    "m-deep",
                    "sess-sender",
                    "the fifth hand-off",
                    rebon_tool::peer::MAX_AGENT_MESSAGE_HOPS + 1,
                ),
            );
            assert!(matches!(applied, Ok(None)), "got {applied:?}");
            control.record_waiting_agent_messages(&session);
            let saved = rebon_session_host::load_waiting_agent_messages(&path);
            assert_eq!(
                saved
                    .iter()
                    .map(|m| m.message_id.as_str())
                    .collect::<Vec<_>>(),
                ["m-deep"]
            );
            assert!(saved[0].text.contains("the fifth hand-off"));
        }

        // Second run: it is back in the inbox, as a rider.
        let session = super::super::test_support::make_test_tui_session();
        let mut control = ForegroundControl::new(&session);
        control.restore_waiting_agent_messages(&session, &identity);
        let inbox = session.engine_half.session_loop.inbox();
        assert!(inbox.has_context(), "the message came back");
        assert!(
            !inbox.has_waking_context(),
            "reopening a session does not start a turn for it"
        );

        // Restoring again in the same process does not add it twice.
        control.restore_waiting_agent_messages(&session, &identity);
        let items = super::super::context_prompts::drain_model_context_items(&session);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].agent_message_id.as_deref(), Some("m-deep"));
        assert_eq!(
            items[0].inbound,
            Some((
                "sess-sender".to_string(),
                rebon_tool::peer::MAX_AGENT_MESSAGE_HOPS + 1
            ))
        );

        // A turn carried it: nothing waits, and the file goes.
        control.record_waiting_agent_messages(&session);
        assert!(!path.exists());
    }

    /// Apply one agent message past the hop bound, so it waits in the inbox.
    fn leave_a_waiting_message(
        control: &ForegroundControl,
        session: &mut TuiEngineSession,
        message_id: &str,
    ) {
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();
        let applied = control.apply(
            &mut app,
            session,
            &handle,
            &mut None,
            &mut None,
            UiMode::Screen,
            "cmd-agent",
            agent_message(
                message_id,
                "sess-sender",
                "waiting",
                rebon_tool::peer::MAX_AGENT_MESSAGE_HOPS + 1,
            ),
        );
        assert!(matches!(applied, Ok(None)), "got {applied:?}");
    }

    fn waiting_ids(path: &std::path::Path) -> Vec<String> {
        rebon_session_host::load_waiting_agent_messages(path)
            .into_iter()
            .map(|message| message.message_id)
            .collect()
    }

    /// The same session reached again — the listener failed to start, or the
    /// lock came back — does not read the file again: a message a turn read
    /// since the last snapshot must not come back from it.
    #[tokio::test]
    async fn reaching_the_same_session_again_does_not_restore_what_a_turn_read() {
        let root = tempfile::tempdir().expect("tempdir");
        let identity = (
            root.path().to_path_buf(),
            "/work/repo".to_string(),
            "sess-again".to_string(),
        );
        let path =
            rebon_session_host::waiting_agent_messages_path(&identity.0, &identity.1, &identity.2);
        let mut session = super::super::test_support::make_test_tui_session();
        let mut control = ForegroundControl::new(&session);
        control.restore_waiting_agent_messages(&session, &identity);
        leave_a_waiting_message(&control, &mut session, "m-read");
        control.record_waiting_agent_messages(&session);
        assert_eq!(waiting_ids(&path), ["m-read"]);

        // A turn reads it before the next snapshot, and the session is reached
        // again while the file still lists it.
        let items = super::super::context_prompts::drain_model_context_items(&session);
        assert_eq!(items.len(), 1);
        control.restore_waiting_agent_messages(&session, &identity);

        assert!(!session.engine_half.session_loop.inbox().has_context());
        control.record_waiting_agent_messages(&session);
        assert!(!path.exists());
    }

    /// The inbox outlives a session switch. What it holds for the session left
    /// behind goes back to that session's file instead of into the new one's.
    #[tokio::test]
    async fn a_session_switch_hands_waiting_messages_back_to_their_session() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = (
            root.path().to_path_buf(),
            "/work/repo".to_string(),
            "sess-first".to_string(),
        );
        let second = (
            root.path().to_path_buf(),
            "/work/repo".to_string(),
            "sess-second".to_string(),
        );
        let first_path =
            rebon_session_host::waiting_agent_messages_path(&first.0, &first.1, &first.2);
        let second_path =
            rebon_session_host::waiting_agent_messages_path(&second.0, &second.1, &second.2);
        let mut session = super::super::test_support::make_test_tui_session();
        let mut control = ForegroundControl::new(&session);
        control.restore_waiting_agent_messages(&session, &first);
        leave_a_waiting_message(&control, &mut session, "m-recorded");
        control.record_waiting_agent_messages(&session);
        // Arrives after the last snapshot, so only memory has it.
        leave_a_waiting_message(&control, &mut session, "m-unrecorded");

        control.restore_waiting_agent_messages(&session, &second);

        assert_eq!(waiting_ids(&first_path), ["m-recorded", "m-unrecorded"]);
        assert!(
            !session.engine_half.session_loop.inbox().has_context(),
            "the first session's messages do not ride into the second's turn"
        );
        control.record_waiting_agent_messages(&session);
        assert!(!second_path.exists());
    }

    /// A message the listener accepted after the last pass is written out when
    /// the terminal shuts down, beside what was already recorded.
    #[tokio::test]
    async fn a_message_accepted_while_exiting_is_recorded_on_drop() {
        let root = tempfile::tempdir().expect("tempdir");
        let identity = (
            root.path().to_path_buf(),
            "/work/repo".to_string(),
            "sess-exiting".to_string(),
        );
        let path =
            rebon_session_host::waiting_agent_messages_path(&identity.0, &identity.1, &identity.2);
        rebon_session_host::save_waiting_agent_messages(
            &path,
            &[rebon_session_host::WaitingAgentMessage {
                message_id: "m-earlier".into(),
                from_session_id: "sess-a".into(),
                hops: 0,
                text: "<teammate-message>earlier</teammate-message>".into(),
            }],
        )
        .expect("seed");
        let session = super::super::test_support::make_test_tui_session();
        let mut control = ForegroundControl::new(&session);
        control.restore_waiting_agent_messages(&session, &identity);
        let envelope = agent_message("m-late", "sess-b", "late", 0)
            .into_agent_message()
            .expect("an agent message");
        let _ = control.agent_inbox.accept(envelope);

        drop(control);

        let saved = rebon_session_host::load_waiting_agent_messages(&path);
        assert_eq!(
            saved
                .iter()
                .map(|m| m.message_id.as_str())
                .collect::<Vec<_>>(),
            ["m-earlier", "m-late"]
        );
    }

    /// A message that arrived over the reception path and then started an idle
    /// turn makes that turn the one answering its sender: the reply it writes
    /// carries one hop more than the message did.
    #[tokio::test]
    async fn an_idle_turn_started_by_a_received_message_replies_one_hop_deeper() {
        let mut session = super::super::test_support::make_test_tui_session();
        let control = ForegroundControl::new(&session);
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();
        let mut active = None;
        let peer = "sess-hops-received";

        let applied = control.apply(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            "cmd-agent-hops",
            ForegroundCommand::AgentMessage {
                message_id: "m-hops".into(),
                from_session_id: peer.into(),
                from_agent: "main".into(),
                from_cwd: None,
                from_handle: None,
                from_name: None,
                summary: None,
                text: "two of us have spoken already".into(),
                hops: 2,
            },
        );
        assert!(matches!(applied, Ok(None)), "got {applied:?}");

        super::super::prompt_lifecycle::maybe_spawn_ready_idle_turn(
            &mut app,
            &session,
            &handle,
            &mut active,
        );
        assert!(active.is_some(), "the message starts the turn it came for");

        assert_eq!(
            session
                .engine_half
                .runtime
                .mid_turn_queue
                .reply_ledger()
                .reply_hops(peer),
            3,
            "the turn answers a message that had already made two hand-offs"
        );
        assert_eq!(
            session
                .engine_half
                .runtime
                .mid_turn_queue
                .reply_ledger()
                .reply_hops("sess-unrelated"),
            0,
            "a conversation this turn was not answering starts at zero"
        );
        drop(active);
    }

    /// A message delivered while a turn runs and handed back unread when that
    /// turn ends — it ended before its next tool round — goes back through the
    /// wake bounds before it may start a turn. Two sessions answering each
    /// other while one is mid-turn must not get past the hop and rate bounds
    /// that way.
    #[tokio::test]
    async fn an_unread_message_from_a_finished_turn_is_judged_before_it_starts_one() {
        let mut session = super::super::test_support::make_test_tui_session();
        let control = ForegroundControl::new(&session);
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();
        let peer = "sess-unread-peer";
        let ledger = session.engine_half.runtime.mid_turn_queue.reply_ledger();
        for _ in 0..rebon_tool::peer::AGENT_MESSAGE_WAKES_PER_HOUR {
            assert!(ledger
                .admit_wake(peer, 0, rebon_types::wall_clock_ms())
                .is_allowed());
        }

        // The listener sees the turn running, so the message joins it.
        control.agent_inbox.set_busy(true);
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut active = Some(ActivePrompt::new(rx, rebon_types::PromptCancel::new()));
        let applied = control.apply(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            "cmd-agent-unread",
            agent_message("m-unread", peer, "while you were busy", 0),
        );
        assert!(matches!(applied, Ok(None)), "got {applied:?}");
        assert_eq!(session.engine_half.runtime.mid_turn_queue.pending_len(), 1);

        // The turn ends before it reads the message.
        drop(active.take());
        control.agent_inbox.set_busy(false);
        control.apply_pending_agent_messages(&session, &active);

        let inbox = session.engine_half.session_loop.inbox();
        assert!(inbox.has_context(), "the message is still delivered later");
        assert!(
            !inbox.has_waking_context(),
            "a peer whose wakes are spent does not get a turn by writing during one"
        );
        super::super::prompt_lifecycle::maybe_spawn_ready_idle_turn(
            &mut app,
            &session,
            &handle,
            &mut active,
        );
        assert!(active.is_none(), "no turn starts for it");
    }

    /// Within the bounds, the same unread message is judged, admitted, and
    /// starts the turn — which then answers its sender.
    #[tokio::test]
    async fn an_unread_message_within_the_bounds_starts_the_next_turn() {
        let mut session = super::super::test_support::make_test_tui_session();
        let control = ForegroundControl::new(&session);
        let mut app = AppState::new();
        let handle = tokio::runtime::Handle::current();
        let peer = "sess-unread-within";

        control.agent_inbox.set_busy(true);
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut active = Some(ActivePrompt::new(rx, rebon_types::PromptCancel::new()));
        let applied = control.apply(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            "cmd-agent-unread-within",
            agent_message("m-unread", peer, "while you were busy", 1),
        );
        assert!(matches!(applied, Ok(None)), "got {applied:?}");
        assert_eq!(
            session
                .engine_half
                .runtime
                .mid_turn_queue
                .reply_ledger()
                .reply_hops(peer),
            0,
            "queued for the running turn is not read by it"
        );

        drop(active.take());
        control.agent_inbox.set_busy(false);
        control.apply_pending_agent_messages(&session, &active);
        assert!(session
            .engine_half
            .session_loop
            .inbox()
            .has_waking_context());
        super::super::prompt_lifecycle::maybe_spawn_ready_idle_turn(
            &mut app,
            &session,
            &handle,
            &mut active,
        );
        assert!(active.is_some());
        assert_eq!(
            session
                .engine_half
                .runtime
                .mid_turn_queue
                .reply_ledger()
                .reply_hops(peer),
            2
        );
        drop(active);
    }

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
