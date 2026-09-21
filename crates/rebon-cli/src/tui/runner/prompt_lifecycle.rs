//! Prompt-turn state machine. Owns the "advance a turn" lifecycle:
//! polling the in-flight `ActivePrompt` to completion, draining
//! channels around finalize so the overlay is whole when it commits,
//! converting outcomes into transcript rows (success, failure, auth
//! error), and pulling the next queued or task-notification prompt
//! into flight. The checked local-turn admission API and its private raw
//! executor dispatch live here too — everything that touches an `ActivePrompt`.

use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;

use rebon_render::normalize_system_api_error_text;
use rebon_types::PromptPasteContent;
use rebon_types::{
    format_system_time_iso_ms, ContentBlock, ExecutionPolicy, ImageContent, PromptCancel,
    TextContent, ToolCallContent, ToolCallStatus,
};

use crate::session::submit_payload::{ensure_internal_submit_payload_user_uuid, SubmitPayload};
use crate::session::ultraplan_run::UltraplanPhase;
use crate::tui::app::{AppState, PromptCompletionStatus};
use crate::tui::dispatch::{
    apply_internal_submit, commit_internal_submit_payload_to_transcript,
    commit_submit_payload_to_transcript, pop_next_queued_submit_with_mode,
};
use crate::tui::permission_modal::PendingPermission;
use crate::tui::wiring::TuiEngineSession;

use super::onboarding_hooks::fire_onboarding_opened;
use super::permission_flow::drain_pending_permissions;
use super::render::drain_pending_updates;
use super::ultraplan::{
    consume_pending_ultraplan_transition, maybe_prepare_implicit_ultraplan_submit,
};
use super::{
    commit_goal_continuation_feedback, drain_main_agent_updates, drain_model_context_prompts,
    maybe_prepare_goal_continuation, reconcile_mid_turn_consumed_queued_submits,
    repin_transcript_to_bottom, sync_follow_tail_after_updates, sync_foreground_agent_view,
    with_main_agent_view, ActivePrompt, LocalTurnSource, PromptResultRx, WithdrawDestination,
    WithdrawableSubmit,
};
use crate::session::commands::effort::resolve_thinking_from_effort;
#[cfg(test)]
use crate::session::commands::effort::ThinkingOverrides;

pub(super) fn maybe_update_loading_state(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    task_notification_retry_after: &mut Option<Instant>,
) {
    let active_prompt_elapsed = active_prompt
        .as_ref()
        .map(|prompt| prompt.started_at.elapsed());
    let prompt_result = poll_active_prompt(app, active_prompt, task_notification_retry_after);
    let mut completed_elapsed = None;
    let mut completed_stop_reason = None;
    let mut completed_usage = rebon_types::Usage::default();
    let mut completed_usage_model: Option<(String, String)> = None;
    let completed_runtime: Option<Arc<crate::session::runtime::SessionRuntime>>;
    let mut failure: Option<String> = None;
    match prompt_result {
        PromptPollResult::Pending => {
            if active_prompt.is_some() {
                app.prompt_completion_status = None;
            }
            // A mirrored session's overlay is the owner's turn in flight,
            // fed by its stream; there is no prompt future here to hold
            // it because the turn runs in another process. Clearing it as
            // an orphan wiped the streamed text every frame — the screen
            // showed one frame's worth of deltas at a time, and the full
            // reply only appeared when the transcript was rebuilt from
            // disk half a second later. That was the flicker.
            let turn_runs_elsewhere = session.remote_background_attachment.is_some()
                || session.pending_hosted_session.is_some();
            let has_orphaned_main_overlay = active_prompt.is_none()
                && !turn_runs_elsewhere
                && if app.foregrounded_task_id.is_some() {
                    app.main_agent_view
                        .as_ref()
                        .is_some_and(|main| !main.tui.overlay.is_empty())
                } else {
                    !app.rebon_tui.overlay.is_empty()
                };
            if has_orphaned_main_overlay {
                with_main_agent_view(app, |app| {
                    tracing::warn!(
                        target: "stream_dbg",
                        overlay_blocks = app.rebon_tui.overlay.blocks.len(),
                        "tui: overlay.clear (Pending+no active_prompt orphan-cleanup)"
                    );
                    // Clean up orphaned overlay content after cancellation.
                    // When Ctrl+C fires during streaming, cancel_commit commits
                    // the partial text and clears the overlay. But late-arriving
                    // stream events from the update channel can re-populate the
                    // overlay on subsequent frames. Since there's no active
                    // prompt to finalize, this content would otherwise remain
                    // as orphaned overlay text.
                    app.rebon_tui.overlay.clear();
                });
            }
            if active_prompt.is_none()
                && !turn_runs_elsewhere
                && app.resume_dialog.is_none()
                && !app.deferred_internal_submit_payloads.is_empty()
                && local_turn_rejection(app, session, None).is_none()
            {
                maybe_spawn_next_queued_prompt(app, session, handle, active_prompt);
            }
            return;
        }
        PromptPollResult::Succeeded {
            stop_reason,
            usage,
            usage_model,
            user_message_uuid,
            runtime,
            ..
        } => {
            app.prompt_completion_status = Some(PromptCompletionStatus::Succeeded);
            completed_stop_reason = Some(stop_reason);
            completed_usage = usage;
            completed_usage_model = usage_model;
            if user_message_uuid.is_some() {
                completed_elapsed = active_prompt_elapsed;
            }
            completed_runtime = runtime;
        }
        PromptPollResult::Failed { error, runtime, .. } => {
            app.prompt_completion_status = Some(PromptCompletionStatus::Failed);
            completed_runtime = runtime;
            failure = Some(error);
        }
    };

    let failed = failure.is_some();
    // Drain ALL remaining events BEFORE finalizing so the overlay
    // is complete when it gets promoted to the transcript. This
    // closes the race where ToolCallUpdate (carrying the Edit diff)
    // was already in the mpsc channel but hadn't been drained yet
    // when finalize_turn was called — causing the committed
    // AssistantToolUseBlock.tool_call_content to be None and the
    // diff to be lost.
    drain_main_agent_updates(app, &mut session.engine_half.update_rx);
    let restore_foreground_agent_after_main_finalize = app.foregrounded_task_id.is_some();
    let mut foreground_after_completion = None;
    if restore_foreground_agent_after_main_finalize {
        let main = app.main_agent_view.take().unwrap_or_default();
        foreground_after_completion = Some(app.replace_active_transcript_view(main));
    }
    tracing::info!(
        target: "stream_dbg",
        overlay_blocks = app.rebon_tui.overlay.blocks.len(),
        transcript_rows = app.rebon_tui.transcript.rows().len(),
        failed,
        "tui: finalize_turn (turn ended)"
    );
    if failure.is_some() {
        mark_inflight_tools_failed(app);
    }
    let turn_runtime = completed_runtime
        .as_ref()
        .unwrap_or(&session.engine_half.runtime);
    finalize_turn(app, &turn_runtime.session_id);
    if let Some(elapsed) = completed_elapsed {
        super::transcript_messages::inject_worked_message(app, elapsed);
    }
    turn_runtime.file_history_tracker.clear_current_message_id();
    // Clear any residual overlay state (e.g. events that arrived
    // between the drain and the finalize on the same tick).
    app.rebon_tui.overlay.clear();
    if let Some(err) = failure {
        commit_prompt_failure_message(app, session, handle, &err);
    } else {
        let (provider, model) = completed_usage_model.unwrap_or_else(|| {
            (
                session.model.provider_name.clone(),
                session.model.name.clone(),
            )
        });
        // The terminal's half of the one write per turn; the other is the
        // worker's, in `finish_background_turn`.
        app.usage_mut().add_turn(provider, model, completed_usage);
        app.streaming_token_count = 0;
        if completed_stop_reason
            .as_ref()
            .is_some_and(|reason| *reason == rebon_types::StopReason::MaxTokens)
        {
            super::inject_system_message(
                app,
                "warning",
                "The model reached its output token limit before completing the response. Send \"continue\" to resume, or increase the output token budget.",
            );
        }
    }
    consume_pending_ultraplan_transition(app, session);
    if !failed
        && app
            .ultraplan_status
            .as_ref()
            .is_some_and(|status| status.phase == UltraplanPhase::Executing)
    {
        app.ultraplan_status = None;
    }
    if let Some(foreground) = foreground_after_completion {
        app.main_agent_view = Some(app.replace_active_transcript_view(foreground));
    }
    if restore_foreground_agent_after_main_finalize {
        sync_foreground_agent_view(app, session.engine_half.tasks.as_ref());
    }
    let goal_action = if !failed && !restore_foreground_agent_after_main_finalize {
        completed_stop_reason.map(|reason| {
            maybe_prepare_goal_continuation(app, session, handle, Some(format!("{reason:?}")))
        })
    } else {
        None
    };
    if let Some(action) = goal_action.as_ref() {
        commit_goal_continuation_feedback(app, action);
    }
    maybe_spawn_next_queued_prompt(app, session, handle, active_prompt);
    if active_prompt.is_some() {
        app.prompt_completion_status = None;
    }
}

pub(super) fn prepare_for_new_prompt_after_withdrawal(
    app: &mut AppState,
    update_rx: &mut tokio::sync::mpsc::UnboundedReceiver<rebon_types::SessionUpdateParams>,
) {
    if app.suppress_late_visible_updates_after_withdrawal {
        drain_pending_updates(app, update_rx);
        app.rebon_tui.overlay.clear();
        app.suppress_late_visible_updates_after_withdrawal = false;
    }
}

pub(super) fn live_input_withdrawable_submit_from_payload(
    submit: &SubmitPayload,
    cursor_offset: usize,
    transcript_len_before: usize,
    transcript_len_after: usize,
) -> WithdrawableSubmit {
    WithdrawableSubmit {
        destination: WithdrawDestination::LiveInput {
            text: submit.text.clone(),
            cursor_offset,
            image_pastes: submit.image_pastes.clone(),
        },
        transcript_len_before,
        transcript_len_after,
        user_message_uuid: submit.user_message_uuid.clone(),
    }
}

fn queued_withdrawable_submit_from_payload(
    submit: &SubmitPayload,
    mode: String,
    transcript_len_before: usize,
    transcript_len_after: usize,
) -> WithdrawableSubmit {
    WithdrawableSubmit {
        destination: WithdrawDestination::QueuedFront {
            submit: submit.clone(),
            mode,
        },
        transcript_len_before,
        transcript_len_after,
        user_message_uuid: submit.user_message_uuid.clone(),
    }
}

fn internal_discard_withdrawable_submit_from_payload(
    submit: &SubmitPayload,
    transcript_len_before: usize,
    transcript_len_after: usize,
) -> WithdrawableSubmit {
    WithdrawableSubmit {
        destination: WithdrawDestination::Discard,
        transcript_len_before,
        transcript_len_after,
        user_message_uuid: submit.user_message_uuid.clone(),
    }
}

/// Check whether the active prompt has completed. Does NOT
/// call `finalize_turn` — the caller is responsible for draining all
/// pending updates first and then finalizing.
pub(super) enum PromptPollResult {
    Pending,
    Succeeded {
        stop_reason: rebon_types::StopReason,
        usage: rebon_types::Usage,
        usage_model: Option<(String, String)>,
        user_message_uuid: Option<String>,
        runtime: Option<Arc<crate::session::runtime::SessionRuntime>>,
    },
    Failed {
        error: String,
        runtime: Option<Arc<crate::session::runtime::SessionRuntime>>,
    },
}

pub(super) fn is_auth_prompt_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("401 unauthorized")
        || lower.contains("http error: 401")
        || lower.contains("model client unauthorized")
        || lower.contains("unauthorized")
}

fn prompt_failure_display_text(err: &str) -> String {
    normalize_system_api_error_text(err)
}

fn mark_inflight_tools_failed(app: &mut AppState) {
    let error_content = ToolCallContent::Content(rebon_types::RegularContent {
        content: ContentBlock::Text(TextContent {
            text: "回合中断，未收到工具执行结果；这不代表工具执行失败。".into(),
            annotations: None,
        }),
    });
    for block in &mut app.rebon_tui.overlay.blocks {
        let rebon_tui::StreamingContentBlock::ToolUse(tool) = block else {
            continue;
        };
        if !matches!(
            tool.status,
            ToolCallStatus::Completed | ToolCallStatus::Failed
        ) {
            tool.status = ToolCallStatus::Failed;
            match &mut tool.content {
                Some(content) => content.push(error_content.clone()),
                None => tool.content = Some(vec![error_content.clone()]),
            }
        }
    }
}

pub(super) fn commit_prompt_failure_message(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    err: &str,
) {
    let auth_failure = is_auth_prompt_failure(err);
    let login_hint = if auth_failure {
        app.onboarding_dialog =
            Some(rebon_plugin_onboarding::OnboardingDialogState::open_for_login_pane());
        if let Some(dialog) = app.onboarding_dialog.as_ref() {
            fire_onboarding_opened(session, handle, dialog, "auth_failure");
        }
        Some("Sign in again in the pane above, or run /onboarding.")
    } else {
        None
    };
    let mut content = prompt_failure_display_text(err);
    if let Some(login_hint) = login_hint {
        if !content.is_empty() {
            if content.ends_with('.') || content.ends_with('!') || content.ends_with('?') {
                content.push(' ');
            } else {
                content.push_str(". ");
            }
        }
        content.push_str(login_hint);
    }
    let uuid = format!("s-api-error-{}", rebon_types::wall_clock_ms_u128());
    let timestamp = format_system_time_iso_ms(SystemTime::now());
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_tui::Message::System(rebon_tui::SystemMessage {
            uuid,
            timestamp,
            subtype: "api_error".into(),
            content: Some(content),
            level: Some(rebon_tui::SystemLevel::Error),
            is_meta: None,
        })),
    );
    app.follow_transcript_tail = true;
}

pub(super) fn poll_active_prompt(
    _app: &mut AppState,
    active_prompt: &mut Option<ActivePrompt>,
    task_notification_retry_after: &mut Option<Instant>,
) -> PromptPollResult {
    let Some(active) = active_prompt.as_mut() else {
        return PromptPollResult::Pending;
    };

    match active.rx.try_recv() {
        Ok(Ok(outcome)) => {
            tracing::info!(stop_reason = ?outcome.stop_reason, "rebon-cli: prompt turn completed");
            let mut active = active_prompt.take().expect("active prompt present");
            let user_message_uuid = active.user_message_uuid.take();
            let had_pending_notifications = !active.pending_task_notification_ids.is_empty()
                || !active.pending_question_escalation_ids.is_empty();
            active.finish_task_notification_claim(true);
            if had_pending_notifications {
                *task_notification_retry_after = None;
            }
            active.finish_question_escalation_notifications();
            let stop_reason = outcome.stop_reason;
            let usage = outcome.usage;
            let usage_model = active.usage_model.take();
            let runtime = active.runtime.take();
            PromptPollResult::Succeeded {
                stop_reason,
                usage,
                usage_model,
                user_message_uuid,
                runtime,
            }
        }
        Ok(Err(err)) => {
            let err = err.to_string();
            tracing::warn!(error = %err, "rebon-cli: prompt turn failed");
            let mut active = active_prompt.take().expect("active prompt present");
            let runtime = active.runtime.take();
            active.finish_task_notification_claim(false);
            if !active.pending_task_notification_ids.is_empty()
                || !active.pending_question_escalation_ids.is_empty()
            {
                *task_notification_retry_after = Some(Instant::now() + Duration::from_secs(10));
                tracing::warn!(
                    task_count = active.pending_task_notification_ids.len(),
                    escalation_count = active.pending_question_escalation_ids.len(),
                    "notification turn failed; leaving notifications unacked for retry"
                );
            }
            PromptPollResult::Failed {
                error: err,
                runtime,
            }
        }
        Err(TryRecvError::Empty) => PromptPollResult::Pending,
        Err(TryRecvError::Closed) => {
            let err = prompt_result_channel_closed_error().to_string();
            tracing::warn!(error = %err, "rebon-cli: prompt oneshot closed without a result");
            let mut active = active_prompt.take().expect("active prompt present");
            let runtime = active.runtime.take();
            active.finish_task_notification_claim(false);
            if !active.pending_task_notification_ids.is_empty()
                || !active.pending_question_escalation_ids.is_empty()
            {
                *task_notification_retry_after = Some(Instant::now() + Duration::from_secs(10));
            }
            PromptPollResult::Failed {
                error: err,
                runtime,
            }
        }
    }
}

fn spawn_prompt_executor_task(
    handle: &Handle,
    runtime: Arc<crate::session::runtime::SessionRuntime>,
    executor: Arc<dyn rebon_agent_core::PromptExecutor>,
    request: rebon_agent_core::PromptRequest,
    turn_kind: &'static str,
) -> PromptResultRx {
    let (tx, rx) = oneshot::channel();
    let turn_guard = runtime.mid_turn_queue.begin_turn();
    let worker = handle.spawn(async move {
        let _turn_guard = turn_guard;
        let _runtime_guard = runtime;
        executor.execute(request).await
    });
    handle.spawn(async move {
        let result = match worker.await {
            Ok(result) => result,
            Err(err) => Err(prompt_executor_task_join_error(turn_kind, err)),
        };
        let _ = tx.send(result);
    });
    rx
}

fn prompt_executor_task_join_error(
    turn_kind: &'static str,
    err: tokio::task::JoinError,
) -> rebon_agent_core::PromptExecutorError {
    if err.is_panic() {
        let join_error = err.to_string();
        let panic = panic_payload_message(err.into_panic());
        tracing::error!(
            turn_kind,
            panic = %panic,
            join_error = %join_error,
            "rebon-cli: prompt executor task panicked"
        );
        return rebon_agent_core::PromptExecutorError::Execution(format!(
            "{turn_kind} prompt executor task panicked: {panic}"
        ));
    }

    let message = if err.is_cancelled() {
        format!("{turn_kind} prompt executor task was aborted before reporting a result")
    } else {
        format!("{turn_kind} prompt executor task failed before reporting a result: {err}")
    };
    tracing::error!(turn_kind, error = %err, "rebon-cli: prompt executor task failed");
    rebon_agent_core::PromptExecutorError::Execution(message)
}

pub(super) fn prompt_result_channel_closed_error() -> rebon_agent_core::PromptExecutorError {
    rebon_agent_core::PromptExecutorError::Execution(
        "prompt executor result channel closed; task may have been aborted or the runtime shut down before reporting a result".to_string(),
    )
}

fn panic_payload_message(payload: Box<dyn Any + Send + 'static>) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

pub(super) fn finalize_turn(app: &mut AppState, session_id: &str) {
    let uuid = format!("a-final-{session_id}-{}", rebon_types::wall_clock_ms_u128());
    let timestamp = format_system_time_iso_ms(SystemTime::now());
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::FinalizeTurn {
            commit_uuid: uuid,
            commit_timestamp: timestamp,
        },
    );
}

pub(super) fn maybe_spawn_next_queued_prompt(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
) {
    if active_prompt.is_some() {
        return;
    }
    if app.foregrounded_task_id.is_some() && app.main_agent_view.is_some() {
        with_main_agent_view(app, |app| {
            maybe_spawn_next_queued_prompt_in_main_view(app, session, handle, active_prompt);
        });
    } else {
        maybe_spawn_next_queued_prompt_in_main_view(app, session, handle, active_prompt);
    }
}

fn maybe_spawn_next_queued_prompt_in_main_view(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
) {
    reconcile_mid_turn_consumed_queued_submits(app);

    if !app.deferred_goal_submit_payloads.is_empty() {
        let mut submit = app.deferred_goal_submit_payloads.remove(0);
        prepare_for_new_prompt_after_withdrawal(app, &mut session.engine_half.update_rx);
        let transcript_len_before_submit = app.rebon_tui.transcript.len();
        commit_submit_payload_to_transcript(app, &mut submit, &session.session_id);
        repin_transcript_to_bottom(app);
        let withdrawable = internal_discard_withdrawable_submit_from_payload(
            &submit,
            transcript_len_before_submit,
            app.rebon_tui.transcript.len(),
        );
        if let Some(active) = admit_active_prompt(
            app,
            session,
            handle,
            active_prompt.as_ref(),
            LocalTurnSource::AutomaticFollowUp,
            submit,
        ) {
            *active_prompt = Some(active.with_withdrawable(withdrawable));
            app.is_loading = true;
        }
        return;
    }

    if !app.deferred_internal_submit_payloads.is_empty() {
        let mut submit = app.deferred_internal_submit_payloads.remove(0);
        prepare_for_new_prompt_after_withdrawal(app, &mut session.engine_half.update_rx);
        let transcript_len_before_submit = app.rebon_tui.transcript.len();
        commit_internal_submit_payload_to_transcript(app, &mut submit, &session.session_id);
        repin_transcript_to_bottom(app);
        let withdrawable = internal_discard_withdrawable_submit_from_payload(
            &submit,
            transcript_len_before_submit,
            app.rebon_tui.transcript.len(),
        );
        if let Some(active) = admit_active_prompt(
            app,
            session,
            handle,
            active_prompt.as_ref(),
            LocalTurnSource::AutomaticFollowUp,
            submit,
        ) {
            *active_prompt = Some(active.with_withdrawable(withdrawable));
            app.is_loading = true;
        }
        return;
    }

    if app.queued_auto_drain_paused_after_withdrawal {
        return;
    }

    let Some((mut submit, mode)) = pop_next_queued_submit_with_mode(app) else {
        return;
    };
    prepare_for_new_prompt_after_withdrawal(app, &mut session.engine_half.update_rx);
    maybe_prepare_implicit_ultraplan_submit(app, session, &mut submit);
    let transcript_len_before_submit = app.rebon_tui.transcript.len();
    commit_submit_payload_to_transcript(app, &mut submit, &session.session_id);
    repin_transcript_to_bottom(app);
    let withdrawable = queued_withdrawable_submit_from_payload(
        &submit,
        mode,
        transcript_len_before_submit,
        app.rebon_tui.transcript.len(),
    );
    if let Some(active) = admit_active_prompt(
        app,
        session,
        handle,
        active_prompt.as_ref(),
        LocalTurnSource::QueuedFollowUp,
        submit,
    ) {
        *active_prompt = Some(active.with_withdrawable(withdrawable));
        app.is_loading = true;
    }
}

pub(super) fn maybe_spawn_task_notification_prompt(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
) -> bool {
    let notifications = session
        .engine_half
        .task_notification_poller
        .unnotified_notifications_for_session(&session.session_id);
    let escalation_notifications = session
        .engine_half
        .tasks
        .unnotified_question_escalation_notifications();
    if notifications.is_empty() && escalation_notifications.is_empty() {
        return false;
    }

    let candidate_ids = notifications
        .iter()
        .map(|notification| notification.task_id.clone())
        .collect::<Vec<_>>();
    let pending_question_escalation_ids = escalation_notifications
        .iter()
        .map(|notification| notification.escalation_id.clone())
        .collect::<Vec<_>>();
    let mut submit = SubmitPayload {
        text: String::new(),
        model_text: None,
        user_message_uuid: None,
        image_pastes: Vec::new(),
        directory_attachments: Vec::new(),
        execution_policy: None,
        skill_invocations: Vec::new(),
    };
    let user_message_uuid =
        ensure_internal_submit_payload_user_uuid(&mut submit, &session.session_id);
    let turn_id = format!("{}:{user_message_uuid}", session.session_id);
    let pending_ids = session
        .engine_half
        .task_notification_poller
        .reserve_task_ids(&session.session_id, &turn_id, &candidate_ids);
    let claimed_notifications = notifications
        .into_iter()
        .filter(|notification| pending_ids.contains(&notification.task_id))
        .collect::<Vec<_>>();
    if claimed_notifications.is_empty() && escalation_notifications.is_empty() {
        return false;
    }

    let mut messages = claimed_notifications
        .iter()
        .map(|notification| notification.message.as_str())
        .collect::<Vec<_>>();
    messages.extend(
        escalation_notifications
            .iter()
            .map(|notification| notification.message.as_str()),
    );
    submit.text = messages.join("\n\n");
    let report_paths = claimed_notifications
        .iter()
        .filter_map(|notification| notification.output_file.clone())
        .collect::<Vec<_>>();
    let spawned = with_main_agent_view(app, |app| {
        let mut submit = apply_internal_submit(app, submit, &session.session_id)
            .expect("notification submit must not be empty");
        maybe_prepare_implicit_ultraplan_submit(app, session, &mut submit);
        repin_transcript_to_bottom(app);
        if let Some(active) = admit_active_prompt_with_task_notifications(
            app,
            session,
            handle,
            active_prompt.as_ref(),
            submit,
            pending_ids,
            pending_question_escalation_ids,
            report_paths,
            turn_id,
        ) {
            *active_prompt = Some(active);
            app.is_loading = true;
            true
        } else {
            false
        }
    });
    spawned
}

/// An active goal runs unattended across many turns, so its turns opt
/// into the relaxed auto-mode script review profile. Turns that already
/// carry an ultraplan planning policy are left alone: those phases are
/// read-only with the shell denied, and
/// `execution_policy_continuity_is_explicit_for_goal_and_ultrawork_constructors`
/// pins that they never carry continuity.
fn stamp_goal_script_continuity(app: &AppState, submit: &mut SubmitPayload) {
    if !app.goal.as_ref().is_some_and(|goal| goal.is_active()) {
        return;
    }
    match submit.execution_policy.as_mut() {
        Some(policy) if policy.accepts_auto_mode_script_continuity() => {
            policy.auto_mode_script_continuity = true;
        }
        Some(_) => {}
        None => submit.execution_policy = Some(ExecutionPolicy::goal()),
    }
}

pub(super) fn admit_active_prompt(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    active_prompt: Option<&ActivePrompt>,
    source: LocalTurnSource,
    mut submit: SubmitPayload,
) -> Option<ActivePrompt> {
    stamp_goal_script_continuity(app, &mut submit);
    match admit_local_turn(
        app,
        session,
        handle,
        active_prompt,
        source,
        LocalTurnRequest::Prompt {
            submit,
            pending_task_notification_ids: Vec::new(),
            pending_question_escalation_ids: Vec::new(),
            coordinator_report_paths: Vec::new(),
        },
    ) {
        Ok(active) => Some(active),
        Err(reason) => {
            report_local_turn_rejection(app, &reason);
            None
        }
    }
}

fn admit_active_prompt_with_task_notifications(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    active_prompt: Option<&ActivePrompt>,
    mut submit: SubmitPayload,
    pending_task_notification_ids: Vec<rebon_plugin_tasks::runtime::TaskId>,
    pending_question_escalation_ids: Vec<rebon_tool::EscalationId>,
    coordinator_report_paths: Vec<String>,
    task_notification_turn_id: String,
) -> Option<ActivePrompt> {
    stamp_goal_script_continuity(app, &mut submit);
    match admit_local_turn(
        app,
        session,
        handle,
        active_prompt,
        LocalTurnSource::TaskNotification,
        LocalTurnRequest::Prompt {
            submit,
            pending_task_notification_ids,
            pending_question_escalation_ids,
            coordinator_report_paths,
        },
    ) {
        Ok(active) => Some(
            active
                .with_task_notification_claim(
                    session.engine_half.task_notification_poller.clone(),
                    task_notification_turn_id,
                )
                .with_question_escalation_registry(session.engine_half.tasks.escalation_registry()),
        ),
        Err(reason) => {
            session
                .engine_half
                .task_notification_poller
                .finish_claim(&task_notification_turn_id, false);
            report_local_turn_rejection(app, &reason);
            None
        }
    }
}

pub(super) fn admit_permission_replay_turn(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    active_prompt: Option<&ActivePrompt>,
    source: LocalTurnSource,
    replay_requests: Vec<rebon_agent_core::DenialReplayRequest>,
) -> Option<ActivePrompt> {
    match admit_local_turn(
        app,
        session,
        handle,
        active_prompt,
        source,
        LocalTurnRequest::PermissionReplay { replay_requests },
    ) {
        Ok(active) => Some(active),
        Err(reason) => {
            report_local_turn_rejection(app, &reason);
            None
        }
    }
}

pub(super) fn admit_idle_context_prompt(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    active_prompt: Option<&ActivePrompt>,
) -> Option<ActivePrompt> {
    match admit_local_turn(
        app,
        session,
        handle,
        active_prompt,
        LocalTurnSource::IdleContext,
        LocalTurnRequest::IdleContext,
    ) {
        Ok(active) => Some(active),
        Err(reason) => {
            report_local_turn_rejection(app, &reason);
            None
        }
    }
}

enum LocalTurnRequest {
    Prompt {
        submit: SubmitPayload,
        pending_task_notification_ids: Vec<rebon_plugin_tasks::runtime::TaskId>,
        pending_question_escalation_ids: Vec<rebon_tool::EscalationId>,
        coordinator_report_paths: Vec<String>,
    },
    IdleContext,
    PermissionReplay {
        replay_requests: Vec<rebon_agent_core::DenialReplayRequest>,
    },
}

fn admit_local_turn(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    active_prompt: Option<&ActivePrompt>,
    source: LocalTurnSource,
    request: LocalTurnRequest,
) -> Result<ActivePrompt, String> {
    if let Some(reason) = local_turn_rejection(app, session, active_prompt) {
        return Err(reason);
    }

    // Persist completed shell feedback before this turn acquires its replay.
    super::task_runtime::drain_inline_shell_commands(app, session, true);

    // This is the only production snapshot of the installed immutable runtime
    // for a new foreground turn. Everything below, including the executor task,
    // steer pump, cleanup, and permission retry, uses this exact Arc.
    let runtime = session.engine_half.runtime.clone();
    let cancel = PromptCancel::new();
    let (request, user_message_uuid, pending_task_ids, pending_escalation_ids) = match request {
        LocalTurnRequest::Prompt {
            submit,
            pending_task_notification_ids,
            pending_question_escalation_ids,
            coordinator_report_paths,
        } => {
            let injected_context = drain_model_context_prompts(app);
            let user_message_uuid = submit.user_message_uuid.clone();
            runtime.file_history_tracker.clear_current_message_id();
            if let Some(message_id) = user_message_uuid.clone() {
                runtime
                    .file_history_tracker
                    .set_current_message_id(message_id);
            }
            let skill_invocations = submit
                .skill_invocations
                .iter()
                .map(|invocation| rebon_agent_core::SkillInvocationRequest {
                    skill: invocation.skill.clone(),
                    args: invocation.args.clone(),
                })
                .collect();
            let thinking_overrides =
                resolve_thinking_from_effort(app.effort_level, app.effort_provider_kind);
            let user_prompt = matches!(
                source,
                LocalTurnSource::DirectUserSubmit
                    | LocalTurnSource::Command
                    | LocalTurnSource::QueuedFollowUp
            )
            .then(|| submit.prompt_text().to_owned());
            let prompt = prompt_blocks(
                submit.prompt_text().to_string(),
                submit.image_pastes,
                injected_context,
            );
            (
                rebon_agent_core::PromptRequest {
                    session_id: runtime.session_id.clone(),
                    cwd: runtime.cwd.clone(),
                    prompt,
                    user_prompt,
                    effort_is_session_default: true,
                    update_publisher: Some(runtime.update_publisher.clone()),
                    permission_publisher: None,
                    mcp_servers: Vec::new(),
                    cancel: cancel.clone(),
                    thinking_budget: thinking_overrides.thinking_budget,
                    max_tokens: thinking_overrides.max_tokens,
                    reasoning_effort_ordinal: thinking_overrides.reasoning_effort_ordinal,
                    additional_working_directories: session.startup.add_dirs.clone(),
                    coordinator_mode: Some(session.engine_half.coordinator_mode_handle.get()),
                    coordinator_report_paths,
                    user_message_uuid: user_message_uuid.clone(),
                    background_agent_system: None,
                    background_agent_tool_filter: None,
                    execution_policy: submit.execution_policy,
                    replay_requests: Vec::new(),
                    skill_invocations,
                },
                user_message_uuid,
                pending_task_notification_ids,
                pending_question_escalation_ids,
            )
        }
        LocalTurnRequest::IdleContext => {
            let injected_context = drain_model_context_prompts(app);
            (
                rebon_agent_core::PromptRequest {
                    session_id: runtime.session_id.clone(),
                    cwd: runtime.cwd.clone(),
                    prompt: prompt_blocks(String::new(), Vec::new(), injected_context),
                    user_prompt: None,
                    effort_is_session_default: false,
                    update_publisher: Some(runtime.update_publisher.clone()),
                    permission_publisher: None,
                    mcp_servers: Vec::new(),
                    cancel: cancel.clone(),
                    thinking_budget: None,
                    max_tokens: None,
                    reasoning_effort_ordinal: None,
                    additional_working_directories: session.startup.add_dirs.clone(),
                    coordinator_mode: Some(session.engine_half.coordinator_mode_handle.get()),
                    coordinator_report_paths: Vec::new(),
                    user_message_uuid: None,
                    background_agent_system: None,
                    background_agent_tool_filter: None,
                    execution_policy: None,
                    replay_requests: Vec::new(),
                    skill_invocations: Vec::new(),
                },
                None,
                Vec::new(),
                Vec::new(),
            )
        }
        LocalTurnRequest::PermissionReplay { replay_requests } => (
            rebon_agent_core::PromptRequest {
                session_id: runtime.session_id.clone(),
                cwd: runtime.cwd.clone(),
                prompt: Vec::new(),
                user_prompt: None,
                effort_is_session_default: false,
                update_publisher: Some(runtime.update_publisher.clone()),
                permission_publisher: None,
                mcp_servers: Vec::new(),
                cancel: cancel.clone(),
                thinking_budget: None,
                max_tokens: None,
                reasoning_effort_ordinal: None,
                additional_working_directories: session.startup.add_dirs.clone(),
                coordinator_mode: Some(session.engine_half.coordinator_mode_handle.get()),
                coordinator_report_paths: Vec::new(),
                user_message_uuid: None,
                background_agent_system: None,
                background_agent_tool_filter: None,
                execution_policy: None,
                replay_requests,
                skill_invocations: Vec::new(),
            },
            None,
            Vec::new(),
            Vec::new(),
        ),
    };

    let rx = dispatch_admitted_local_turn(handle, runtime.clone(), request, source);
    if matches!(
        source,
        LocalTurnSource::DirectUserSubmit | LocalTurnSource::Command
    ) || matches!(
        source,
        LocalTurnSource::QueuedFollowUp
            | LocalTurnSource::AutomaticFollowUp
            | LocalTurnSource::AgentViewAttachment
    ) {
        if let Some(status) = app.ultraplan_status.as_mut() {
            if status.phase == UltraplanPhase::PlanModeActive {
                status.phase = UltraplanPhase::Orchestrating;
            }
        }
    }

    let active = if pending_task_ids.is_empty() && pending_escalation_ids.is_empty() {
        ActivePrompt::new(rx, cancel)
    } else {
        ActivePrompt::with_task_notifications(rx, cancel, pending_task_ids, pending_escalation_ids)
    };
    let steer_pump = app
        .mid_turn_queued_submit_poller
        .as_ref()
        .map(|_| runtime.mid_turn_queue.clone())
        .and_then(|poller| {
            super::steer_pump::spawn(
                handle,
                runtime.session_agents.clone(),
                poller,
                runtime.update_publisher.clone(),
                runtime.session_id.clone(),
            )
        });
    Ok(active
        .with_runtime(runtime)
        .with_source(source)
        .with_user_message_uuid(user_message_uuid)
        .with_usage_model(&session.model.provider_name, &session.model.name)
        .with_steer_pump(steer_pump))
}

pub(super) fn local_turn_rejection(
    app: &AppState,
    session: &TuiEngineSession,
    active_prompt: Option<&ActivePrompt>,
) -> Option<String> {
    if active_prompt.is_some() {
        return Some("Cannot start another local turn while a prompt is running.".to_string());
    }
    if let Some(remote) = session.remote_background_attachment.as_ref() {
        return Some(if remote.is_live() {
            format!(
                "Session {} is attached to background worker {}; send the prompt to that worker instead of starting a local turn.",
                session.session_id, remote.job_id
            )
        } else {
            format!(
                "Session {} belongs to background worker {}, which is stopped; a prompt gives it a new worker instead of starting a local turn.",
                session.session_id, remote.job_id
            )
        });
    }
    if let Some(pending) = session
        .pending_hosted_session
        .as_ref()
        .filter(|pending| pending.kind.session_is_the_jobs())
    {
        return Some(format!(
            "Session {} is moving into background worker {}; wait for the handover to finish.",
            session.session_id, pending.job_id
        ));
    }
    if session.session_id != session.engine_half.runtime.session_id
        || !rebon_session::same_cwd(&session.cwd, &session.engine_half.runtime.cwd)
    {
        return Some(
            "The session runtime is being replaced; retry after it is installed.".to_string(),
        );
    }
    if app
        .mid_turn_queued_submit_poller
        .as_ref()
        .is_some_and(|poller| !Arc::ptr_eq(poller, &session.engine_half.runtime.mid_turn_queue))
    {
        return Some(
            "The session attachments are still bound to the previous runtime; retry after replacement finishes."
                .to_string(),
        );
    }
    if session
        .engine_half
        .projection_invalid
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Some("The session was detached after its transcript projection was lost; reload or resume it before sending another prompt.".to_string());
    }
    None
}

fn report_local_turn_rejection(app: &mut AppState, reason: &str) {
    super::inject_system_message(app, "error", reason);
    app.follow_transcript_tail = true;
}

fn prompt_blocks(
    text: String,
    image_pastes: Vec<PromptPasteContent>,
    injected_context: Vec<String>,
) -> Vec<ContentBlock> {
    let mut prompt_blocks = Vec::new();
    for xml in injected_context {
        prompt_blocks.push(ContentBlock::Text(TextContent {
            text: xml,
            annotations: None,
        }));
    }
    if !text.is_empty() {
        prompt_blocks.push(ContentBlock::Text(TextContent {
            text,
            annotations: None,
        }));
    }
    for image in image_pastes {
        prompt_blocks.push(ContentBlock::Image(ImageContent {
            mime_type: image
                .media_type
                .unwrap_or_else(|| String::from("image/png")),
            data: image.content,
            uri: None,
            annotations: None,
        }));
    }
    prompt_blocks
}

fn dispatch_admitted_local_turn(
    handle: &Handle,
    runtime: Arc<crate::session::runtime::SessionRuntime>,
    request: rebon_agent_core::PromptRequest,
    source: LocalTurnSource,
) -> PromptResultRx {
    let executor = runtime.executor.clone();
    let label = match source {
        LocalTurnSource::PermissionRetry | LocalTurnSource::ForegroundPermissionRetry => {
            "permission replay"
        }
        _ => "main",
    };
    spawn_prompt_executor_task(handle, runtime, executor, request, label)
}

#[cfg(test)]
fn dispatch_local_turn_for_test(
    session: &TuiEngineSession,
    handle: &Handle,
    text: String,
    image_pastes: Vec<PromptPasteContent>,
    injected_context: Vec<String>,
    cancel: PromptCancel,
    overrides: ThinkingOverrides,
    execution_policy: Option<ExecutionPolicy>,
    user_message_uuid: Option<String>,
    coordinator_report_paths: Vec<String>,
    skill_invocations: Vec<rebon_agent_core::SkillInvocationRequest>,
) -> PromptResultRx {
    let runtime = session.engine_half.runtime.clone();
    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: runtime.session_id.clone(),
        cwd: runtime.cwd.clone(),
        prompt: prompt_blocks(text, image_pastes, injected_context),
        update_publisher: Some(runtime.update_publisher.clone()),
        permission_publisher: None,
        mcp_servers: Vec::new(),
        cancel,
        thinking_budget: overrides.thinking_budget,
        max_tokens: overrides.max_tokens,
        reasoning_effort_ordinal: overrides.reasoning_effort_ordinal,
        additional_working_directories: session.startup.add_dirs.clone(),
        coordinator_mode: Some(session.engine_half.coordinator_mode_handle.get()),
        coordinator_report_paths,
        user_message_uuid,
        background_agent_system: None,
        background_agent_tool_filter: None,
        execution_policy,
        replay_requests: Vec::new(),
        skill_invocations,
    };
    dispatch_admitted_local_turn(handle, runtime, request, LocalTurnSource::TestOnly)
}

pub(super) fn sync_current_session_title(app: &mut AppState, session: &TuiEngineSession) {
    let Some(record) = session.server_state.get_session(&session.session_id) else {
        return;
    };
    app.session_title = record.title;
}

/// Re-resolve the runtime for a provider/model change an approved
/// `ProfileSwitch` already wrote to disk.
///
/// Deferred to here because the permission callback that applied the profile
/// holds the session immutably and has no tokio handle. Left in place if the
/// session has no runtime to resolve on — the next pass tries again rather
/// than dropping the change on the floor.
fn drain_pending_profile_runtime_refresh(app: &mut AppState, session: &mut TuiEngineSession) {
    if app.pending_profile_runtime_refresh.is_none() {
        return;
    }
    let Some(handle) = session.runtime_handle() else {
        return;
    };
    let update = app.pending_profile_runtime_refresh.take();
    super::runtime_refresh::refresh_runtime_model(app, session, update, &handle);
}

pub(super) fn drain_ui_channels(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    active_prompt: &mut Option<ActivePrompt>,
) {
    let before_overlay_empty = if app.foregrounded_task_id.is_some() {
        app.main_agent_view
            .as_ref()
            .map(|state| state.tui.overlay.is_empty())
            .unwrap_or(true)
    } else {
        app.rebon_tui.overlay.is_empty()
    };
    let before_rows = if app.foregrounded_task_id.is_some() {
        app.main_agent_view
            .as_ref()
            .map(|state| state.tui.transcript.len())
            .unwrap_or(0)
    } else {
        app.rebon_tui.transcript.len()
    };
    drain_main_agent_updates(app, &mut session.engine_half.update_rx);
    sync_current_session_title(app, session);
    let after_overlay_empty = if app.foregrounded_task_id.is_some() {
        app.main_agent_view
            .as_ref()
            .map(|state| state.tui.overlay.is_empty())
            .unwrap_or(true)
    } else {
        app.rebon_tui.overlay.is_empty()
    };
    let after_rows = if app.foregrounded_task_id.is_some() {
        app.main_agent_view
            .as_ref()
            .map(|state| state.tui.transcript.len())
            .unwrap_or(0)
    } else {
        app.rebon_tui.transcript.len()
    };
    if let Some(active) = active_prompt.as_mut() {
        if !after_overlay_empty || after_rows > before_rows {
            active.reply_started = true;
        } else if !before_overlay_empty {
            active.reply_started = true;
        }
    }
    drain_pending_permissions(app, session, pending_permission);
    drain_pending_profile_runtime_refresh(app, session);
    sync_follow_tail_after_updates(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{HashMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use crate::session::submit_payload::{DirectoryAttachment, SubmitPayload};
    use crate::session::ultraplan_run::UltraplanStatus;
    use crate::tui::dispatch::{
        enqueue_submit_payload, pop_queued_command_into_input, queued_text,
    };
    use crate::tui::update::translate_session_update;
    use crate::ui_config::UiMode;
    use rebon_agent_core::{
        PromptExecutorError, PromptOutcome, PromptRequest, SessionUpdatePublisher,
    };
    use rebon_core::query::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};
    use rebon_types::{
        ContentBlock, ExecutionPolicy, PolicyMode, PromptCancel, SessionUpdate,
        SessionUpdateParams, TextContent, ToolCallStatus, ToolKind, UltraplanContext,
    };
    use serde_json::json;
    use tokio::runtime::Builder;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
    use tokio::sync::oneshot;

    use super::super::interrupt_flow::{apply_cancel_or_exit, apply_interrupt};
    use super::super::test_support::{
        insert_local_agent_task, insert_terminal_agent_notification, make_test_tui_session,
        make_ultraplan_test_tui_session,
    };
    use crate::session::commands::ultraplan_prompt::build_ultraplan_prompt;

    fn make_immediate_handle() -> (tokio::runtime::Runtime, Handle) {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let handle = runtime.handle().clone();
        (runtime, handle)
    }

    fn admission_test_submit(text: &str) -> SubmitPayload {
        SubmitPayload {
            text: text.to_string(),
            model_text: None,
            user_message_uuid: None,
            image_pastes: Vec::new(),
            directory_attachments: Vec::new(),
            execution_policy: None,
            skill_invocations: Vec::new(),
        }
    }

    #[test]
    fn local_turn_source_matrix_uses_one_checked_production_seam() {
        fn production_part(source: &str) -> &str {
            source
                .split("\n#[cfg(test)]\nmod tests")
                .next()
                .expect("production source")
        }

        let prompt_lifecycle = include_str!("prompt_lifecycle.rs");
        let production_prompt_lifecycle = production_part(prompt_lifecycle);
        let production_sources = [
            production_prompt_lifecycle,
            production_part(include_str!("submit.rs")),
            production_part(include_str!("foreground_mailbox.rs")),
            production_part(include_str!("session_detach_attach.rs")),
            production_part(include_str!("event_loop_entry.rs")),
        ]
        .join("\n");
        for variant in [
            "DirectUserSubmit",
            "Command",
            "QueuedFollowUp",
            "AutomaticFollowUp",
            "IdleContext",
            "TaskNotification",
            "PermissionRetry",
            "ForegroundPermissionRetry",
            "AgentViewAttachment",
        ] {
            let marker = format!("LocalTurnSource::{variant}");
            assert!(
                production_sources.matches(&marker).count() >= 1,
                "missing checked admission route for {variant}"
            );
        }
        for source in [
            production_part(include_str!("submit.rs")),
            production_part(include_str!("foreground_mailbox.rs")),
            production_part(include_str!("session_detach_attach.rs")),
            production_part(include_str!("event_loop_entry.rs")),
        ] {
            assert!(!source.contains("spawn_prompt_executor_task"));
            assert!(!source.contains("dispatch_admitted_local_turn"));
            assert!(!source.contains(".execute(request)"));
        }
        let raw_dispatch_marker = concat!("spawn_prompt_executor", "_task(");
        assert_eq!(
            production_prompt_lifecycle
                .matches(raw_dispatch_marker)
                .count(),
            2,
            "the raw executor task must have only its definition and the private admitted call"
        );
        assert!(prompt_lifecycle.contains("fn dispatch_local_turn_for_test("));
        let test_seam = prompt_lifecycle
            .find("fn dispatch_local_turn_for_test(")
            .expect("test seam");
        let cfg_prefix = &prompt_lifecycle[test_seam.saturating_sub(40)..test_seam];
        assert!(cfg_prefix.contains("#[cfg(test)]"));
    }

    #[test]
    fn checked_admission_snapshots_one_runtime_and_detached_turn_keeps_it() {
        let (tokio_runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        session.set_test_cwd("/old-runtime");
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let old_runtime = session.engine_half.runtime.clone();
        let old_session_id = session.session_id.clone();
        let mut app = AppState::new();

        let active = admit_active_prompt(
            &mut app,
            &session,
            &handle,
            None,
            LocalTurnSource::DirectUserSubmit,
            admission_test_submit("bound once"),
        )
        .expect("turn admitted");
        assert_eq!(active.source, LocalTurnSource::DirectUserSubmit);
        assert!(Arc::ptr_eq(
            active.runtime.as_ref().expect("captured runtime"),
            &old_runtime
        ));

        session.attached_background_job_id = Some("bg-old".into());
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                "bg-remote".into(),
                old_session_id.clone(),
                "/old-runtime".into(),
                crate::background::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "token".into(),
                },
            ));
        session.pending_hosted_session = Some(crate::background::PendingHostedSession::handover(
            "bg-pending".into(),
        ));
        assert!(session
            .swap_runtime("blocked-runtime", "/blocked", true)
            .is_err());
        assert_eq!(session.session_id, old_session_id);
        assert!(Arc::ptr_eq(&session.engine_half.runtime, &old_runtime));
        assert_eq!(
            session.attached_background_job_id.as_deref(),
            Some("bg-old")
        );
        assert!(session.remote_background_attachment.is_some());
        assert!(session.pending_hosted_session.is_some());

        assert!(session
            .swap_runtime("replacement-runtime", "/replacement", false)
            .expect("detached replacement"));
        assert!(!Arc::ptr_eq(&session.engine_half.runtime, &old_runtime));
        assert!(!Arc::ptr_eq(
            &session.engine_half.runtime.mid_turn_queue,
            &old_runtime.mid_turn_queue
        ));
        assert!(session.attached_background_job_id.is_none());
        assert!(session.remote_background_attachment.is_none());
        assert!(session.pending_hosted_session.is_none());
        assert!(Arc::ptr_eq(
            active.runtime.as_ref().expect("old runtime retained"),
            &old_runtime
        ));
        tokio_runtime.block_on(tokio::task::yield_now());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].session_id, old_session_id);
        assert_eq!(requests[0].cwd, PathBuf::from("/old-runtime"));

        drop(active);
        tokio_runtime.shutdown_background();
    }

    #[test]
    fn rejected_and_no_op_runtime_adoption_preserve_session_attachment_state() {
        let mut session = make_test_tui_session();
        let runtime = session.engine_half.runtime.clone();
        let poller = session.engine_half.runtime.mid_turn_queue.clone();
        session.attached_background_job_id = Some("bg-same".into());
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                "bg-same".into(),
                session.session_id.clone(),
                session.cwd.clone(),
                crate::background::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "token".into(),
                },
            ));
        session.pending_hosted_session = Some(crate::background::PendingHostedSession::dispatch(
            "bg-same".into(),
        ));
        let (_blocked_update_tx, blocked_update_rx) =
            rebon_agent_core::ChannelSessionUpdatePublisher::new();
        let (_blocked_permission_tx, blocked_permission_rx) = unbounded_channel();
        assert!(session
            .restore_runtime(
                runtime.clone(),
                blocked_update_rx,
                blocked_permission_rx,
                true,
            )
            .is_err());
        assert!(Arc::ptr_eq(&session.engine_half.runtime, &runtime));
        assert_eq!(
            session.attached_background_job_id.as_deref(),
            Some("bg-same")
        );
        assert!(session.remote_background_attachment.is_some());
        assert!(session.pending_hosted_session.is_some());

        let (_update_tx, update_rx) = rebon_agent_core::ChannelSessionUpdatePublisher::new();
        let (_permission_tx, permission_rx) = unbounded_channel();
        assert!(!session
            .restore_runtime(runtime.clone(), update_rx, permission_rx, false)
            .expect("same runtime adoption"));
        assert!(Arc::ptr_eq(&session.engine_half.runtime, &runtime));
        assert!(Arc::ptr_eq(
            &session.engine_half.runtime.mid_turn_queue,
            &poller
        ));
        assert_eq!(
            session.attached_background_job_id.as_deref(),
            Some("bg-same")
        );
        assert!(session.remote_background_attachment.is_some());
        assert!(session.pending_hosted_session.is_some());
    }

    #[test]
    fn local_turn_admission_rejects_every_unavailable_session_state() {
        let mut app = AppState::new();
        let (_tx, rx) = oneshot::channel();
        let active = ActivePrompt::new(rx, PromptCancel::new());
        let session = make_test_tui_session();
        assert!(local_turn_rejection(&app, &session, Some(&active))
            .is_some_and(|reason| reason.contains("prompt is running")));

        let mut session = make_test_tui_session();
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                "bg-remote".into(),
                session.session_id.clone(),
                session.cwd.clone(),
                crate::background::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "token".into(),
                },
            ));
        assert!(local_turn_rejection(&app, &session, None)
            .is_some_and(|reason| reason.contains("background worker")));

        let mut session = make_test_tui_session();
        session.pending_hosted_session = Some(crate::background::PendingHostedSession::handover(
            "bg-handover".into(),
        ));
        assert!(local_turn_rejection(&app, &session, None)
            .is_some_and(|reason| reason.contains("moving into background")));

        let mut session = make_test_tui_session();
        session.remote_background_attachment = Some(
            crate::background::RemoteBackgroundAttachment::without_worker(
                "bg-parked".into(),
                session.session_id.clone(),
                session.cwd.clone(),
                crate::background::BackgroundJobStatus::Stopped,
                0,
            ),
        );
        assert!(local_turn_rejection(&app, &session, None)
            .is_some_and(|reason| reason.contains("which is stopped")));

        let mut session = make_test_tui_session();
        session.session_id = "replacement-visible-before-runtime".into();
        assert!(local_turn_rejection(&app, &session, None)
            .is_some_and(|reason| reason.contains("being replaced")));

        let mut session = make_test_tui_session();
        app.mid_turn_queued_submit_poller =
            Some(session.engine_half.runtime.mid_turn_queue.clone());
        session
            .swap_runtime("replacement-with-stale-poller", "/replacement", false)
            .expect("replace test runtime");
        assert!(local_turn_rejection(&app, &session, None)
            .is_some_and(|reason| reason.contains("previous runtime")));
        app.mid_turn_queued_submit_poller = None;

        let session = make_test_tui_session();
        session
            .engine_half
            .projection_invalid
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(local_turn_rejection(&app, &session, None)
            .is_some_and(|reason| reason.contains("projection was lost")));
    }

    #[test]
    fn current_session_title_syncs_from_server_state() {
        let mut app = AppState::new();
        app.session_title = Some("Previous session".into());
        let session = make_test_tui_session();

        sync_current_session_title(&mut app, &session);
        assert!(app.session_title.is_none());

        session
            .server_state
            .set_session_title(&session.session_id, "Generated title".into());
        sync_current_session_title(&mut app, &session);
        assert_eq!(app.session_title.as_deref(), Some("Generated title"));
    }

    #[test]
    fn completed_prompt_accumulates_provider_billed_totals() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        session.engine_half.update_rx = rebon_agent_core::ChannelSessionUpdatePublisher::new().1;
        let mut outcome = PromptOutcome::end_turn();
        outcome.usage = rebon_types::Usage {
            input_tokens: 23_000,
            output_tokens: 1_000,
            total_input_tokens: 203_000,
            total_output_tokens: 4_500,
            ..Default::default()
        };
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(outcome)).unwrap();
        let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));
        let mut retry_after = None;

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        let usage = app.usage();
        assert_eq!(usage.last_turn.total_input_tokens, 203_000);
        assert_eq!(usage.last_turn.total_output_tokens, 4_500);
        assert_eq!(usage.total.input_tokens, 23_000);
        assert_eq!(usage.total.output_tokens, 1_000);
        assert_eq!(usage.total.total_input_tokens, 203_000);
        assert_eq!(usage.total.total_output_tokens, 4_500);
        runtime.shutdown_background();
    }

    #[test]
    fn completing_an_old_turn_after_runtime_swap_closes_only_its_file_history() {
        let (tokio_runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let old = session.engine_half.runtime.clone();
        old.file_history_tracker.set_current_message_id("old-turn");
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(PromptOutcome::end_turn())).unwrap();
        let mut active = Some(
            ActivePrompt::new(rx, PromptCancel::new())
                .with_runtime(old.clone())
                .with_user_message_uuid(Some("old-turn".into())),
        );
        session.swap_runtime("new-session", "/new", false).unwrap();
        let new = session.engine_half.runtime.clone();
        new.file_history_tracker.set_current_message_id("new-turn");
        let mut retry_after = None;

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        assert_eq!(
            rebon_agent_core::file_history::FileHistoryTracker::is_armed(&old.file_history_tracker),
            Some(false)
        );
        assert_eq!(
            rebon_agent_core::file_history::FileHistoryTracker::is_armed(&new.file_history_tracker),
            Some(true),
            "old-turn completion must not close the new session's active boundary"
        );
        tokio_runtime.shutdown_background();
    }

    /// A mirrored session's overlay is the owner's turn in flight, fed by its
    /// stream, and there is no prompt future in this process to vouch for
    /// it. Treating it as an orphan cleared it every frame: the screen
    /// showed one frame's worth of deltas at a time, and the full reply
    /// only appeared when the transcript was rebuilt from disk. A local
    /// session with no prompt still gets the clean-up, which is what
    /// keeps a Ctrl+C from leaving late deltas on screen.
    #[test]
    fn a_mirrored_sessions_overlay_is_not_an_orphan() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        session.remote_background_attachment = Some(
            crate::background::RemoteBackgroundAttachment::without_worker(
                "bg-mirror".into(),
                session.session_id.clone(),
                ".".into(),
                rebon_session_host::BackgroundJobStatus::Running,
                0,
            ),
        );
        app.rebon_tui.overlay.set_streaming_text("流上来的半句");
        let mut active = None;
        let mut retry_after = None;

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("流上来的半句"),
            "the owner's turn in flight is not an orphan"
        );

        session.remote_background_attachment = None;
        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );
        assert!(
            app.rebon_tui.overlay.is_empty(),
            "a local session with no prompt still clears late deltas"
        );
        runtime.shutdown_background();
    }

    #[test]
    fn max_tokens_completion_commits_visible_warning() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        session.engine_half.update_rx = rebon_agent_core::ChannelSessionUpdatePublisher::new().1;
        let mut outcome = PromptOutcome::end_turn();
        outcome.stop_reason = rebon_types::StopReason::MaxTokens;
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(outcome)).unwrap();
        let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));
        let mut retry_after = None;

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        assert_eq!(
            app.prompt_completion_status,
            Some(PromptCompletionStatus::Succeeded)
        );
        let Some(rebon_tui::Message::System(warning)) = app.rebon_tui.transcript.rows().last()
        else {
            panic!("expected MaxTokens warning");
        };
        assert_eq!(warning.subtype, "warning");
        assert_eq!(warning.level, Some(rebon_tui::SystemLevel::Warning));
        assert!(warning
            .content
            .as_deref()
            .is_some_and(|content| content.contains("output token limit")));

        let area = ratatui::layout::Rect::new(0, 0, 120, 6);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        let mut cache = rebon_tui::TranscriptMeasureCache::new();
        rebon_tui::render_transcript_cached_with_running_hints(
            &app.rebon_tui,
            area,
            &mut buffer,
            &rebon_tui::RenderTheme::plain(),
            0,
            rebon_tui::ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
            false,
            rebon_tui::TranscriptRenderExtras::empty(),
        );
        let rendered = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .filter_map(|position| buffer.cell(position))
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("output token limit"), "{rendered}");
        runtime.shutdown_background();
    }

    #[test]
    fn main_prompt_completion_finalizes_main_view_and_keeps_foreground_local_agent_visible() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let reg = std::sync::Arc::new(reg);
        app.tasks = reg.clone();
        app.main_agent_view = Some(crate::tui::app::StoredTranscriptView::default());
        app.foregrounded_task_id = Some("agent-1".into());
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(PromptOutcome::end_turn())).unwrap();
        let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));
        let mut retry_after = None;
        let mut session = make_test_tui_session();
        session.engine_half.tasks = reg;
        session.engine_half.update_rx = rebon_agent_core::ChannelSessionUpdatePublisher::new().1;
        let (runtime, handle) = make_immediate_handle();

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        assert!(active.is_none());
        assert_eq!(
            app.prompt_completion_status,
            Some(PromptCompletionStatus::Succeeded)
        );
        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-1"));
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::User(user)
                if user.message.content.iter().any(|block| matches!(
                    block,
                    rebon_tui::UserContentBlock::Text(text) if text.text == "do work"
                ))
        )));
        let main = app.main_agent_view.as_ref().expect("main view");
        assert_eq!(main.tui.transcript.len(), 0);
        assert!(main.tui.overlay.is_empty());
        runtime.shutdown_background();
    }

    #[test]
    fn queued_prompt_after_main_completion_stays_in_main_view_while_child_foregrounded() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let registry = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &registry,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let registry = Arc::new(registry);
        app.tasks = registry.clone();
        app.main_agent_view = Some(crate::tui::app::StoredTranscriptView::default());
        app.foregrounded_task_id = Some("agent-1".into());
        sync_foreground_agent_view(&mut app, registry.as_ref());
        let child_rows = app.rebon_tui.transcript.rows().to_vec();
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "queued main turn".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );

        let mut session = make_test_tui_session();
        session.engine_half.tasks = registry;
        session.engine_half.update_rx = rebon_agent_core::ChannelSessionUpdatePublisher::new().1;
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(PromptOutcome::end_turn())).unwrap();
        let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));
        let mut retry_after = None;

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );
        runtime.block_on(tokio::task::yield_now());

        assert!(active.is_some());
        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-1"));
        assert_eq!(app.rebon_tui.transcript.rows(), child_rows.as_slice());
        let main = app.main_agent_view.as_ref().expect("saved main view");
        assert_eq!(main.tui.transcript.len(), 1);
        assert!(main.tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::User(user)
                if user.message.content.iter().any(|block| matches!(
                    block,
                    rebon_tui::UserContentBlock::Text(text) if text.text == "queued main turn"
                ))
        )));
        let withdrawable = active
            .as_ref()
            .and_then(|active| active.withdrawable.as_ref())
            .expect("queued prompt is withdrawable");
        assert_eq!(withdrawable.transcript_len_before, 0);
        assert_eq!(withdrawable.transcript_len_after, 1);
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(prompt_text(&requests[0]), "queued main turn");

        drop(active.take());
        runtime.shutdown_background();
    }

    #[test]
    fn context_reset_during_foreground_prompt_completion_keeps_fresh_main_active() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u-child", "child transcript");
        app.rebon_tui.overlay.set_streaming_text("child overlay");
        let mut main = crate::tui::app::StoredTranscriptView::default();
        main.tui.overlay.set_streaming_text("stale main overlay");
        app.main_agent_view = Some(main);
        app.foregrounded_task_id = Some("agent-1".into());

        let (publisher, update_rx) = rebon_agent_core::ChannelSessionUpdatePublisher::new();
        let mut session = make_test_tui_session();
        session.engine_half.update_publisher = Arc::new(publisher.clone());
        session.engine_half.update_rx = update_rx;
        let (runtime, handle) = make_immediate_handle();
        runtime.block_on(publisher.publish_owned(SessionUpdateParams {
            session_id: session.session_id.clone(),
            update: SessionUpdate::ContextReset { plan: None },
        }));
        let (tx, rx) = oneshot::channel();
        tx.send(Err(PromptExecutorError::Execution(
            "prompt failed after reset".into(),
        )))
        .unwrap();
        let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));
        let mut retry_after = None;

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        assert!(active.is_none());
        assert!(app.foregrounded_task_id.is_none());
        assert!(app.main_agent_view.is_none());
        assert!(app.local_agent_views.is_empty());
        assert!(app.rebon_tui.overlay.is_empty());
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        assert!(matches!(
            app.rebon_tui.transcript.rows().last(),
            Some(rebon_tui::Message::System(system))
                if system.subtype == "api_error"
                    && system.content.as_deref() == Some("prompt failed after reset")
        ));
        runtime.shutdown_background();
    }

    #[test]
    fn orphaned_main_overlay_cleanup_preserves_foreground_child_overlay() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u-child", "child transcript");
        app.rebon_tui.overlay.set_streaming_text("child overlay");
        let child_overlay_blocks = app.rebon_tui.overlay.blocks.len();
        let mut main = crate::tui::app::StoredTranscriptView::default();
        main.tui.overlay.set_streaming_text("orphaned main overlay");
        app.main_agent_view = Some(main);
        app.foregrounded_task_id = Some("agent-1".into());
        let mut active = None;
        let mut retry_after = None;
        let mut session = make_test_tui_session();
        let (runtime, handle) = make_immediate_handle();

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-1"));
        assert_eq!(app.rebon_tui.overlay.blocks.len(), child_overlay_blocks);
        assert!(!app.rebon_tui.overlay.is_empty());
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        assert!(app
            .main_agent_view
            .as_ref()
            .is_some_and(|main| main.tui.overlay.is_empty()));
        runtime.shutdown_background();
    }

    #[test]
    fn active_prompt_clears_previous_terminal_title_completion_status() {
        let mut app = AppState::new();
        app.prompt_completion_status = Some(PromptCompletionStatus::Failed);
        let (_tx, rx) = oneshot::channel();
        let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));
        let mut retry_after = None;
        let mut session = make_test_tui_session();
        let (runtime, handle) = make_immediate_handle();

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        assert!(active.is_some());
        assert_eq!(app.prompt_completion_status, None);
        runtime.shutdown_background();
    }

    #[test]
    fn finalize_turn_commits_streaming_text_on_success_path() {
        let mut app = AppState::new();
        app.rebon_tui.overlay.set_streaming_text("assistant reply");
        let session = make_test_tui_session();
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(PromptOutcome::end_turn())).unwrap();
        let mut active = Some(
            ActivePrompt::new(rx, PromptCancel::new())
                .with_user_message_uuid(Some("u-completed".into()))
                .with_usage_model("provider-before-switch", "model-before-switch"),
        );

        let mut retry_after = None;
        let result = poll_active_prompt(&mut app, &mut active, &mut retry_after);

        assert!(matches!(
            result,
            PromptPollResult::Succeeded {
                stop_reason: rebon_types::StopReason::EndTurn,
                usage,
                usage_model: Some((ref provider, ref model)),
                user_message_uuid: Some(ref uuid),
                ..
            } if usage == rebon_types::Usage::default()
                && provider == "provider-before-switch"
                && model == "model-before-switch"
                && uuid == "u-completed"
        ));
        assert!(active.is_none());

        // poll_active_prompt no longer finalizes directly. The caller drains
        // events first and then commits the overlay; this mirrors that order.
        finalize_turn(&mut app, &session.session_id);
        app.rebon_tui.overlay.clear();

        assert_eq!(app.rebon_tui.transcript.len(), 1);
        assert!(app.rebon_tui.overlay.is_empty());
    }

    #[test]
    fn successful_user_turn_appends_worked_row_after_the_reply() {
        let mut app = AppState::new();
        app.rebon_tui.overlay.set_streaming_text("assistant reply");
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(PromptOutcome::end_turn())).unwrap();
        let mut prompt = ActivePrompt::new(rx, PromptCancel::new())
            .with_user_message_uuid(Some("u-worked".into()));
        prompt.started_at = Instant::now() - Duration::from_secs(3_723);
        let mut active = Some(prompt);
        let mut retry_after = None;
        let mut session = make_test_tui_session();
        let (runtime, handle) = make_immediate_handle();

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        assert!(active.is_none());
        let rows = app.rebon_tui.transcript.rows();
        assert!(matches!(
            rows.first(),
            Some(rebon_tui::Message::Assistant(_))
        ));
        assert!(matches!(
            rows.last(),
            Some(rebon_tui::Message::System(system))
                if system.subtype == "turn_duration"
                    && system.content.as_deref() == Some("Worked 1h 02m 03s")
        ));
        runtime.shutdown_background();
    }

    #[test]
    fn failed_user_turn_does_not_append_worked_row() {
        let mut app = AppState::new();
        let (tx, rx) = oneshot::channel();
        tx.send(Err(PromptExecutorError::Execution("failed".into())))
            .unwrap();
        let mut prompt = ActivePrompt::new(rx, PromptCancel::new())
            .with_user_message_uuid(Some("u-failed".into()));
        prompt.started_at = Instant::now() - Duration::from_secs(62);
        let mut active = Some(prompt);
        let mut retry_after = None;
        let mut session = make_test_tui_session();
        let (runtime, handle) = make_immediate_handle();

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        assert!(app.rebon_tui.transcript.rows().iter().all(|row| !matches!(
            row,
            rebon_tui::Message::System(system) if system.subtype == "turn_duration"
        )));
        runtime.shutdown_background();
    }

    #[test]
    fn successful_execute_turn_clears_ultraplan_widget_status() {
        let mut app = AppState::new();
        app.ultraplan_status = Some(UltraplanStatus {
            run_id: "ultraplan-test".into(),
            phase: UltraplanPhase::Executing,
            task_title: "ship it".into(),
            started_at_ms: Some(1),
            worker_count: None,
            context: None,
            round: 1,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        });
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(PromptOutcome::end_turn())).unwrap();
        let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));
        let mut session = make_test_tui_session();
        let (runtime, handle) = make_immediate_handle();
        let mut retry_after = None;

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        assert!(active.is_none());
        assert!(app.ultraplan_status.is_none());
        runtime.shutdown_background();
    }

    #[test]
    fn successful_execute_turn_clears_ultraplan_after_pending_context_reset() {
        let mut app = AppState::new();
        app.ultraplan_status = Some(UltraplanStatus {
            run_id: "ultraplan-test".into(),
            phase: UltraplanPhase::Executing,
            task_title: "ship it".into(),
            started_at_ms: Some(1),
            worker_count: None,
            context: None,
            round: 1,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        });
        let (publisher, update_rx) = rebon_agent_core::ChannelSessionUpdatePublisher::new();
        let mut session = make_test_tui_session();
        session.engine_half.update_publisher = Arc::new(publisher.clone());
        session.engine_half.update_rx = update_rx;
        let (runtime, handle) = make_immediate_handle();
        runtime.block_on(publisher.publish_owned(SessionUpdateParams {
            session_id: session.session_id.clone(),
            update: SessionUpdate::ContextReset { plan: None },
        }));
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(PromptOutcome::end_turn())).unwrap();
        let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));
        let mut retry_after = None;

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        assert!(active.is_none());
        assert!(app.ultraplan_status.is_none());
        runtime.shutdown_background();
    }

    #[test]
    fn prompt_executor_error_commits_visible_api_error() {
        let mut app = AppState::new();
        let (tx, rx) = oneshot::channel();
        tx.send(Err(PromptExecutorError::Execution(
            "model stream start failed: boom".into(),
        )))
        .unwrap();
        let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));

        let mut retry_after = None;
        let result = poll_active_prompt(&mut app, &mut active, &mut retry_after);

        assert!(matches!(result, PromptPollResult::Failed { .. }));
        assert!(active.is_none());
        if let PromptPollResult::Failed { error, .. } = result {
            finalize_turn(&mut app, "sess-err");
            app.rebon_tui.overlay.clear();
            let session = make_test_tui_session();
            let (runtime, handle) = make_immediate_handle();
            commit_prompt_failure_message(&mut app, &session, &handle, &error);
            runtime.shutdown_background();
        }

        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 1);
        let rebon_tui::Message::System(sys) = rows.last().unwrap() else {
            panic!("expected system message");
        };
        assert_eq!(sys.subtype, "api_error");
        assert_eq!(sys.level, Some(rebon_tui::SystemLevel::Error));
        let content = sys.content.as_deref().unwrap();
        assert!(!content.contains("Prompt turn failed"), "{content}");
        assert_eq!(content, "boom");
        assert!(app.onboarding_dialog.is_none());
    }

    #[test]
    fn prompt_failure_display_text_strips_known_wrapper_prefixes() {
        assert_eq!(
            prompt_failure_display_text("Prompt turn failed: prompt executor failed: model stream error: model client bad request: 400: ws error (websocket_connection_limit_reached): Responses websocket connection limit reached (60 minutes)."),
            "Responses websocket connection limit reached (60 minutes)."
        );
    }

    #[test]
    fn prompt_executor_panic_returns_diagnostic_error() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        session.set_test_executor(Arc::new(PanicPromptExecutor));

        let rx = dispatch_local_turn_for_test(
            &session,
            &handle,
            "trigger panic".into(),
            Vec::new(),
            Vec::new(),
            PromptCancel::new(),
            ThinkingOverrides {
                thinking_budget: None,
                max_tokens: None,
                reasoning_effort_ordinal: None,
            },
            None,
            None,
            Vec::new(),
            Vec::new(),
        );

        let result = runtime.block_on(rx).expect("supervisor sends result");
        let Err(err) = result else {
            panic!("expected executor panic to become an error");
        };
        let err = err.to_string();
        assert!(err.contains("main prompt executor task panicked"), "{err}");
        assert!(err.contains("synthetic executor panic"), "{err}");
        runtime.shutdown_background();
    }

    #[test]
    fn prompt_result_closed_channel_returns_diagnostic_error() {
        let mut app = AppState::new();
        let (tx, rx) = oneshot::channel();
        drop(tx);
        let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));
        let mut retry_after = None;

        let result = poll_active_prompt(&mut app, &mut active, &mut retry_after);

        let PromptPollResult::Failed { error, .. } = result else {
            panic!("expected closed result channel to fail prompt");
        };
        assert!(active.is_none());
        assert!(
            error.contains("prompt executor result channel closed"),
            "{error}"
        );
    }

    #[test]
    fn prompt_executor_error_marks_inflight_tools_failed_before_commit() {
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::StartToolUse {
                call_id: "tool-edit".into(),
                tool_name: "Edit".into(),
                kind: ToolKind::Edit,
                initial_status: ToolCallStatus::InProgress,
                initial_title: None,
                raw_input: Some(HashMap::from([("file_path".into(), json!("src/lib.rs"))])),
                content: None,
                locations: None,
                raw_output: None,
            },
        );
        let (tx, rx) = oneshot::channel();
        tx.send(Err(PromptExecutorError::Execution(
            "model stream error: retries exhausted".into(),
        )))
        .unwrap();
        let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));
        let mut session = make_test_tui_session();
        let (runtime, handle) = make_immediate_handle();
        let mut retry_after = None;

        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active,
            &mut retry_after,
        );

        assert!(active.is_none());
        assert_eq!(
            app.prompt_completion_status,
            Some(PromptCompletionStatus::Failed)
        );
        assert!(app.rebon_tui.overlay.is_empty());
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 2);
        match &rows[0] {
            rebon_tui::Message::Assistant(assistant) => match &assistant.message.content[0] {
                rebon_tui::AssistantContentBlock::ToolUse(tool) => {
                    assert_eq!(tool.status, Some(ToolCallStatus::Failed));
                    let content = tool.tool_call_content.as_ref().expect("failure content");
                    assert!(content.iter().any(|item| matches!(
                        item,
                        ToolCallContent::Content(regular)
                            if matches!(&regular.content, ContentBlock::Text(text) if text.text == "回合中断，未收到工具执行结果；这不代表工具执行失败。")
                    )));
                }
                other => panic!("expected failed tool use, got {other:?}"),
            },
            other => panic!("expected assistant tool row, got {other:?}"),
        }
        assert!(matches!(rows[1], rebon_tui::Message::System(_)));
        runtime.shutdown_background();
    }

    #[test]
    fn run_code_transport_failure_stays_separate_from_tool_results() {
        for status in [
            ToolCallStatus::Pending,
            ToolCallStatus::InProgress,
            ToolCallStatus::Completed,
            ToolCallStatus::Failed,
        ] {
            for auth_failure in [false, true] {
                let error = if auth_failure {
                    "model client unauthorized: 401 Unauthorized"
                } else {
                    "websocket error: WebSocket protocol error: Connection reset without closing handshake"
                };
                let mut app = AppState::new();
                rebon_tui::reducer(
                    &mut app.rebon_tui,
                    rebon_tui::Action::StartToolUse {
                        call_id: "code-transport".into(),
                        tool_name: "run_code".into(),
                        kind: ToolKind::Execute,
                        initial_status: status,
                        initial_title: None,
                        raw_input: Some(HashMap::from([(
                            "description".into(),
                            json!("Inspect tasks"),
                        )])),
                        content: None,
                        locations: None,
                        raw_output: None,
                    },
                );
                let (tx, rx) = oneshot::channel();
                tx.send(Err(PromptExecutorError::Execution(error.into())))
                    .expect("prompt receiver alive");
                let mut active = Some(ActivePrompt::new(rx, PromptCancel::new()));
                let mut session = make_test_tui_session();
                let (runtime, handle) = make_immediate_handle();
                let mut retry_after = None;
                maybe_update_loading_state(
                    &mut app,
                    &mut session,
                    &handle,
                    &mut active,
                    &mut retry_after,
                );
                assert!(active.is_none());
                assert_eq!(
                    app.prompt_completion_status,
                    Some(PromptCompletionStatus::Failed)
                );
                let terminal = matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed);
                let rows = app.rebon_tui.transcript.rows();
                let system_errors = rows.iter().filter(|row| matches!(row, rebon_tui::Message::System(system) if system.subtype == "api_error")).count();
                assert_eq!(system_errors, 1, "{status:?} auth={auth_failure}: {rows:?}");
                assert!(rows.iter().any(|row| matches!(row, rebon_tui::Message::System(system) if system.subtype == "api_error" && system.content.as_deref().is_some_and(|text| text.contains(&prompt_failure_display_text(error))))));
                let rebon_tui::Message::Assistant(assistant) = &rows[0] else {
                    panic!("expected tool row")
                };
                let rebon_tui::AssistantContentBlock::ToolUse(tool) = &assistant.message.content[0]
                else {
                    panic!("expected code tool")
                };
                if terminal {
                    assert_eq!(tool.status, Some(status));
                    assert!(tool.tool_call_content.as_ref().is_none_or(Vec::is_empty));
                } else {
                    assert_eq!(tool.status, Some(ToolCallStatus::Failed));
                    let content = tool.tool_call_content.as_ref().expect("failure reason");
                    assert_eq!(content.len(), 1);
                    assert!(
                        matches!(&content[0], ToolCallContent::Content(regular) if matches!(&regular.content, ContentBlock::Text(text) if text.text == "回合中断，未收到工具执行结果；这不代表工具执行失败。"))
                    );
                }
                assert_eq!(app.onboarding_dialog.is_some(), auth_failure);
                runtime.shutdown_background();
            }
        }
    }

    #[test]
    fn auth_prompt_executor_error_opens_the_login_pane_and_points_at_onboarding() {
        let mut app = AppState::new();
        let session = make_test_tui_session();
        let (runtime, handle) = make_immediate_handle();
        commit_prompt_failure_message(
            &mut app,
            &session,
            &handle,
            "prompt executor failed: model client unauthorized: ws connect: HTTP error: 401 Unauthorized",
        );
        runtime.shutdown_background();

        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::System(sys) = rows.last().unwrap() else {
            panic!("expected system message");
        };
        assert_eq!(sys.subtype, "api_error");
        assert_eq!(sys.level, Some(rebon_tui::SystemLevel::Error));
        let content = sys.content.as_deref().unwrap();
        assert!(content.contains("401 Unauthorized"), "{content}");
        assert!(!content.contains("prompt executor failed"), "{content}");
        assert!(!content.contains("model client unauthorized"), "{content}");
        // `/login` no longer exists as a command; the pane opens by itself
        // and the hint names the wizard that still can sign in.
        assert!(!content.contains("/login"), "{content}");
        assert!(content.contains("/onboarding"), "{content}");
        assert!(app.onboarding_dialog.is_some());
    }

    #[test]
    fn spawn_prompt_turn_characterization_builds_request_fields_in_stable_order() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        session.set_test_cwd("/tmp/rebon-characterization");
        session.engine_half.coordinator_mode_handle.set(true);
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let cancel = PromptCancel::new();
        let execution_policy = ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
            "run-characterize",
            "planmodeactive",
            PolicyMode::Observe,
        ));
        let image_paste = rebon_types::PromptPasteContent {
            id: 42,
            kind: "image".into(),
            content: "base64-image-payload".into(),
            media_type: Some("image/jpeg".into()),
            filename: Some("screen.jpg".into()),
            source_path: None,
        };

        let _rx = dispatch_local_turn_for_test(
            &session,
            &handle,
            "user prompt body".into(),
            vec![image_paste],
            vec![
                "<channel>server context</channel>".into(),
                "<teammate>note</teammate>".into(),
            ],
            cancel,
            ThinkingOverrides {
                thinking_budget: Some(1234),
                max_tokens: Some(5678),
                reasoning_effort_ordinal: Some(2),
            },
            Some(execution_policy.clone()),
            Some("user-message-uuid-1".into()),
            vec!["C:/Users/example/.rebon/tasks/agent-1.report.md".into()],
            vec![rebon_agent_core::SkillInvocationRequest {
                skill: "project-context-skill".into(),
                args: Some("--check".into()),
            }],
        );
        runtime.block_on(tokio::task::yield_now());

        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.session_id, session.session_id);
        assert_eq!(request.cwd, PathBuf::from("/tmp/rebon-characterization"));
        assert!(request.update_publisher.is_some());
        assert!(request.permission_publisher.is_none());
        assert!(request.mcp_servers.is_empty());
        assert_eq!(request.thinking_budget, Some(1234));
        assert_eq!(request.max_tokens, Some(5678));
        assert_eq!(request.reasoning_effort_ordinal, Some(2));
        assert_eq!(request.coordinator_mode, Some(true));
        assert_eq!(
            request.coordinator_report_paths,
            vec!["C:/Users/example/.rebon/tasks/agent-1.report.md".to_string()]
        );
        assert_eq!(
            request.user_message_uuid.as_deref(),
            Some("user-message-uuid-1")
        );
        assert_eq!(request.execution_policy.as_ref(), Some(&execution_policy));
        assert!(request.replay_requests.is_empty());
        assert_eq!(request.skill_invocations.len(), 1);
        assert_eq!(request.skill_invocations[0].skill, "project-context-skill");
        assert_eq!(
            request.skill_invocations[0].args.as_deref(),
            Some("--check")
        );
        assert_eq!(request.prompt.len(), 4);
        assert!(matches!(
            &request.prompt[0],
            ContentBlock::Text(text) if text.text == "<channel>server context</channel>" && text.annotations.is_none()
        ));
        assert!(matches!(
            &request.prompt[1],
            ContentBlock::Text(text) if text.text == "<teammate>note</teammate>" && text.annotations.is_none()
        ));
        assert!(matches!(
            &request.prompt[2],
            ContentBlock::Text(text) if text.text == "user prompt body" && text.annotations.is_none()
        ));
        assert!(matches!(
            &request.prompt[3],
            ContentBlock::Image(image)
                if image.mime_type == "image/jpeg"
                    && image.data == "base64-image-payload"
                    && image.uri.is_none()
                    && image.annotations.is_none()
        ));

        runtime.shutdown_background();
    }

    fn prompt_text(request: &PromptRequest) -> &str {
        match &request.prompt[0] {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("expected text prompt, got {other:?}"),
        }
    }

    #[test]
    fn spawn_prompt_turn_characterization_keeps_context_and_image_without_user_text() {
        let (runtime, handle) = make_immediate_handle();
        let mut session = make_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());

        let _rx = dispatch_local_turn_for_test(
            &session,
            &handle,
            String::new(),
            vec![rebon_types::PromptPasteContent {
                id: 7,
                kind: "image".into(),
                content: "png-data".into(),
                media_type: None,
                filename: None,
                source_path: None,
            }],
            vec!["<injected>only context</injected>".into()],
            PromptCancel::new(),
            ThinkingOverrides {
                thinking_budget: None,
                max_tokens: None,
                reasoning_effort_ordinal: None,
            },
            None,
            None,
            Vec::new(),
            Vec::new(),
        );
        runtime.block_on(tokio::task::yield_now());

        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].prompt.len(), 2);
        assert!(matches!(
            &requests[0].prompt[0],
            ContentBlock::Text(text) if text.text == "<injected>only context</injected>"
        ));
        assert!(matches!(
            &requests[0].prompt[1],
            ContentBlock::Image(image) if image.mime_type == "image/png" && image.data == "png-data"
        ));
        assert_eq!(requests[0].user_message_uuid, None);
        assert!(requests[0].coordinator_report_paths.is_empty());
        assert!(requests[0].execution_policy.is_none());
        assert!(requests[0].skill_invocations.is_empty());

        runtime.shutdown_background();
    }

    #[test]
    fn task_notification_prompt_carries_output_file_allowlist_path() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let output_file = "/home/user/.rebon/tasks/agent-d6851c254d18.report.md";
        insert_terminal_agent_notification(&mut app, "agent-allowlist", output_file);
        let mut active_prompt = None;

        maybe_spawn_task_notification_prompt(&mut app, &session, &handle, &mut active_prompt);
        assert!(session
            .engine_half
            .task_notification_poller
            .poll(AttachmentPollRequest::new(
                &session.session_id,
                &session.session_id,
                1,
                AttachmentPollPhase::Regular,
            ))
            .is_empty());
        runtime.block_on(tokio::task::yield_now());

        let active = active_prompt.as_ref().expect("notification prompt spawned");
        assert_eq!(
            active.pending_task_notification_ids.as_slice(),
            &[rebon_plugin_tasks::runtime::TaskId::new("agent-allowlist")]
        );
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].coordinator_report_paths,
            vec![output_file.to_string()]
        );
        let prompt_text = match &requests[0].prompt[0] {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("expected notification text prompt, got {other:?}"),
        };
        assert!(prompt_text.contains(&format!("<output-file>{output_file}</output-file>")));

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn task_notification_prompt_inherits_active_goal_continuity_policy() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        app.goal = Some(crate::goal::GoalState::new_now("ship it"));
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        insert_terminal_agent_notification(
            &mut app,
            "agent-goal-notify",
            "/home/user/.rebon/tasks/agent-goal.report.md",
        );
        let mut active_prompt = None;

        maybe_spawn_task_notification_prompt(&mut app, &session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let policy = requests[0]
            .execution_policy
            .as_ref()
            .expect("active goal continuity policy");
        assert!(policy.auto_mode_script_continuity);
        assert!(policy.ultraplan.is_none());

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    fn continuity_stamping_submit(execution_policy: Option<ExecutionPolicy>) -> SubmitPayload {
        SubmitPayload {
            text: "keep going".into(),
            model_text: None,
            user_message_uuid: None,
            image_pastes: Vec::new(),
            directory_attachments: Vec::new(),
            execution_policy,
            skill_invocations: Vec::new(),
        }
    }

    #[test]
    fn goal_continuity_stamping_leaves_ultraplan_planning_turns_untouched() {
        let mut app = AppState::new();
        app.goal = Some(crate::goal::GoalState::new_now("ship it"));
        let planning = ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
            "run-1",
            "reviewing",
            PolicyMode::Enforce,
        ));

        let mut submit = continuity_stamping_submit(Some(planning.clone()));
        stamp_goal_script_continuity(&app, &mut submit);

        assert_eq!(submit.execution_policy.as_ref(), Some(&planning));
        assert!(
            !submit
                .execution_policy
                .expect("planning policy")
                .auto_mode_script_continuity
        );
    }

    #[test]
    fn goal_continuity_stamping_covers_bare_and_execution_phase_turns() {
        let mut app = AppState::new();
        app.goal = Some(crate::goal::GoalState::new_now("ship it"));

        let mut bare = continuity_stamping_submit(None);
        stamp_goal_script_continuity(&app, &mut bare);
        let bare_policy = bare.execution_policy.expect("goal policy");
        assert!(bare_policy.auto_mode_script_continuity);
        assert!(bare_policy.ultraplan.is_none());

        let mut executing = continuity_stamping_submit(Some(ExecutionPolicy::ultraplan(
            UltraplanContext::ultrawork_execution_controller_turn("run-1", PolicyMode::Enforce),
        )));
        stamp_goal_script_continuity(&app, &mut executing);
        assert!(
            executing
                .execution_policy
                .expect("ultrawork policy")
                .auto_mode_script_continuity
        );
    }

    #[test]
    fn goal_continuity_stamping_is_inert_without_an_active_goal() {
        let app = AppState::new();
        let mut submit = continuity_stamping_submit(None);

        stamp_goal_script_continuity(&app, &mut submit);

        assert!(submit.execution_policy.is_none());
    }

    #[test]
    fn task_notification_prompt_inherits_active_ultraplan_policy() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let run_id = "run-notification-policy";
        let context = UltraplanContext::planning_turn(run_id, "reviewing", PolicyMode::Enforce);
        app.ultraplan_status = Some(UltraplanStatus {
            run_id: run_id.into(),
            phase: UltraplanPhase::Reviewing,
            task_title: "test ultraplan".into(),
            started_at_ms: Some(1),
            worker_count: None,
            context: Some(context),
            round: 1,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        });
        insert_terminal_agent_notification(
            &mut app,
            "agent-ultraplan-notify",
            "/home/user/.rebon/tasks/agent-ultraplan.report.md",
        );
        let mut active_prompt = None;

        maybe_spawn_task_notification_prompt(&mut app, &session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let policy = requests[0]
            .execution_policy
            .as_ref()
            .and_then(|policy| policy.ultraplan.as_ref())
            .expect("ultraplan policy");
        assert_eq!(policy.run_id, run_id);
        assert!(policy.allowed_tools.iter().any(|tool| tool == "PlanLedger"));

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn failed_task_notification_prompt_retries_with_output_file_allowlist_path() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        recorder.push_outcome(Err(PromptExecutorError::Execution("transient".into())));
        session.set_test_executor(recorder.clone());
        let output_file = if cfg!(windows) {
            r"C:\Users\example\.rebon\tasks\agent-d6851c254d18.report.md"
        } else {
            "/home/user/.rebon/tasks/agent-d6851c254d18.report.md"
        };
        insert_terminal_agent_notification(&mut app, "agent-retry", output_file);
        let mut active_prompt = None;
        let mut retry_after = None;

        maybe_spawn_task_notification_prompt(&mut app, &session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());
        let first_requests = recorder.take_requests();
        assert_eq!(first_requests.len(), 1);
        assert_eq!(
            first_requests[0].coordinator_report_paths,
            vec![output_file.to_string()]
        );

        assert!(matches!(
            poll_active_prompt(&mut app, &mut active_prompt, &mut retry_after),
            PromptPollResult::Failed { .. }
        ));
        assert!(active_prompt.is_none());
        assert!(retry_after.is_some_and(|deadline| deadline > Instant::now()));
        assert!(
            !app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-retry"))
                .expect("task snapshot")
                .notified
        );

        maybe_spawn_task_notification_prompt(&mut app, &session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());
        let retry_requests = recorder.take_requests();
        assert_eq!(retry_requests.len(), 1);
        assert_eq!(
            retry_requests[0].coordinator_report_paths,
            vec![output_file.to_string()]
        );
        let prompt_text = match &retry_requests[0].prompt[0] {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("expected retried notification text prompt, got {other:?}"),
        };
        assert!(prompt_text.contains(&format!("<output-file>{output_file}</output-file>")));

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn question_escalation_prompt_injects_xml_and_acks_after_success() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let client = app
            .tasks
            .escalation_registry()
            .worker_client("agent-question", Some("question worker".into()));
        let waiting = runtime.spawn(async move {
            client
                .escalate("Which path should I take?".into(), None, Some("ctx".into()))
                .await
        });
        runtime.block_on(tokio::task::yield_now());
        let mut active_prompt = None;
        let mut retry_after = None;

        maybe_spawn_task_notification_prompt(&mut app, &session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        let active = active_prompt.as_ref().expect("escalation prompt spawned");
        assert!(active.pending_task_notification_ids.is_empty());
        assert_eq!(active.pending_question_escalation_ids.len(), 1);
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].coordinator_report_paths.is_empty());
        let prompt_text = match &requests[0].prompt[0] {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("expected question escalation text prompt, got {other:?}"),
        };
        assert!(prompt_text.contains("<question-escalation>"));
        assert!(prompt_text.contains("<agent-id>agent-question</agent-id>"));
        assert!(prompt_text.contains("Which path should I take?"));
        assert_eq!(
            app.tasks
                .unnotified_question_escalation_notifications()
                .len(),
            1
        );

        assert!(matches!(
            poll_active_prompt(&mut app, &mut active_prompt, &mut retry_after),
            PromptPollResult::Succeeded { .. }
        ));
        assert!(app
            .tasks
            .unnotified_question_escalation_notifications()
            .is_empty());

        app.tasks
            .escalation_registry()
            .cancel_all("test cleanup after delivered question escalation");
        let err = runtime.block_on(waiting).unwrap().unwrap_err();
        assert!(err.contains("cancelled before it was answered"));
        assert!(err.contains("esc-agent-question-1"));
        runtime.shutdown_background();
    }

    #[test]
    fn question_escalation_prompt_failure_leaves_unacked_for_retry_without_output_allowlist() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        recorder.push_outcome(Err(PromptExecutorError::Execution("transient".into())));
        session.set_test_executor(recorder.clone());
        let client = app
            .tasks
            .escalation_registry()
            .worker_client("agent-retry-question", None);
        let waiting =
            runtime.spawn(async move { client.escalate("Retry this?".into(), None, None).await });
        runtime.block_on(tokio::task::yield_now());
        let mut active_prompt = None;
        let mut retry_after = None;

        maybe_spawn_task_notification_prompt(&mut app, &session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());
        let first_requests = recorder.take_requests();
        assert_eq!(first_requests.len(), 1);
        assert!(first_requests[0].coordinator_report_paths.is_empty());

        assert!(matches!(
            poll_active_prompt(&mut app, &mut active_prompt, &mut retry_after),
            PromptPollResult::Failed { .. }
        ));
        assert!(active_prompt.is_none());
        assert!(retry_after.is_some_and(|deadline| deadline > Instant::now()));
        assert_eq!(
            app.tasks
                .unnotified_question_escalation_notifications()
                .len(),
            1
        );

        maybe_spawn_task_notification_prompt(&mut app, &session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());
        let retry_requests = recorder.take_requests();
        assert_eq!(retry_requests.len(), 1);
        assert!(retry_requests[0].coordinator_report_paths.is_empty());
        let prompt_text = match &retry_requests[0].prompt[0] {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("expected retried question escalation text prompt, got {other:?}"),
        };
        assert!(prompt_text.contains("<question-escalation>"));
        assert!(prompt_text.contains("Retry this?"));

        drop(active_prompt.take());
        app.tasks
            .escalation_registry()
            .cancel_all("test cleanup after retried question escalation");
        let err = runtime.block_on(waiting).unwrap().unwrap_err();
        assert!(err.contains("cancelled before it was answered"));
        assert!(err.contains("esc-agent-retry-question-1"));
        runtime.shutdown_background();
    }

    #[test]
    fn task_notification_prompt_uses_main_view_while_local_agent_foregrounded() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let foreground_reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &foreground_reg,
            "agent-foreground",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let foreground_snapshot = foreground_reg
            .snapshot(&rebon_plugin_tasks::runtime::TaskId::new(
                "agent-foreground",
            ))
            .expect("foreground task");
        insert_terminal_agent_notification(
            &mut app,
            "agent-notify",
            "/home/user/.rebon/tasks/agent-notify.report.md",
        );
        app.tasks.insert(
            rebon_plugin_tasks::runtime::TaskId::new("agent-foreground"),
            foreground_snapshot,
            PromptCancel::new(),
        );
        app.main_agent_view = Some(crate::tui::app::StoredTranscriptView::default());
        app.foregrounded_task_id = Some("agent-foreground".into());
        sync_foreground_agent_view(&mut app, session.engine_half.tasks.as_ref());
        let mut active_prompt = None;

        maybe_spawn_task_notification_prompt(&mut app, &session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        assert_eq!(
            app.foregrounded_task_id.as_deref(),
            Some("agent-foreground")
        );
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::User(user)
                if user.message.content.iter().any(|block| matches!(
                    block,
                    rebon_tui::UserContentBlock::Text(text) if text.text == "do work"
                ))
        )));
        assert!(!app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::User(user)
                if user.message.content.iter().any(|block| matches!(
                    block,
                    rebon_tui::UserContentBlock::Text(text) if text.text.contains("<task-notification>")
                ))
        )));
        let main = app.main_agent_view.as_ref().expect("main view");
        assert!(main.tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::User(user)
                if user.message.content.iter().any(|block| matches!(
                    block,
                    rebon_tui::UserContentBlock::Text(text) if text.text.contains("<task-notification>")
                ))
        )));

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn task_notification_prompt_completion_finalizes_main_view_while_local_agent_foregrounded() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let foreground_reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &foreground_reg,
            "agent-foreground",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let foreground_snapshot = foreground_reg
            .snapshot(&rebon_plugin_tasks::runtime::TaskId::new(
                "agent-foreground",
            ))
            .expect("foreground task");
        insert_terminal_agent_notification(
            &mut app,
            "agent-notify",
            "/home/user/.rebon/tasks/agent-notify.report.md",
        );
        app.tasks.insert(
            rebon_plugin_tasks::runtime::TaskId::new("agent-foreground"),
            foreground_snapshot,
            PromptCancel::new(),
        );
        let (tx, update_rx): (_, UnboundedReceiver<SessionUpdateParams>) = unbounded_channel();
        send_text_chunk(&tx, &session.session_id, "notification ack");
        session.engine_half.update_rx = update_rx;
        app.main_agent_view = Some(crate::tui::app::StoredTranscriptView::default());
        app.foregrounded_task_id = Some("agent-foreground".into());
        sync_foreground_agent_view(&mut app, session.engine_half.tasks.as_ref());
        let mut active_prompt = None;
        let mut retry_after = None;

        maybe_spawn_task_notification_prompt(&mut app, &session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());
        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut retry_after,
        );

        assert!(active_prompt.is_none());
        assert_eq!(
            app.foregrounded_task_id.as_deref(),
            Some("agent-foreground")
        );
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::User(user)
                if user.message.content.iter().any(|block| matches!(
                    block,
                    rebon_tui::UserContentBlock::Text(text) if text.text == "do work"
                ))
        )));
        assert!(!app
            .rebon_tui
            .transcript
            .rows()
            .iter()
            .any(|row| matches!(row, rebon_tui::Message::Assistant(_))));
        let main = app.main_agent_view.as_ref().expect("main view");
        assert!(main.tui.overlay.is_empty());
        assert!(main.tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::User(user)
                if user.message.content.iter().any(|block| matches!(
                    block,
                    rebon_tui::UserContentBlock::Text(text) if text.text.contains("<task-notification>")
                ))
        )));
        assert!(main.tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::Assistant(assistant)
                if assistant.message.content.iter().any(|block| matches!(
                    block,
                    rebon_tui::AssistantContentBlock::Text(text) if text.text == "notification ack"
                ))
        )));
        assert!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-notify"))
                .expect("task snapshot")
                .notified
        );
        runtime.shutdown_background();
    }

    struct PanicPromptExecutor;

    #[async_trait::async_trait]
    impl rebon_agent_core::PromptExecutor for PanicPromptExecutor {
        async fn execute(
            &self,
            _request: PromptRequest,
        ) -> Result<PromptOutcome, PromptExecutorError> {
            panic!("synthetic executor panic")
        }
    }

    #[derive(Default)]
    struct RecordingPromptExecutor {
        requests: Mutex<Vec<PromptRequest>>,
        outcomes: Mutex<VecDeque<Result<PromptOutcome, PromptExecutorError>>>,
    }

    #[async_trait::async_trait]
    impl rebon_agent_core::PromptExecutor for RecordingPromptExecutor {
        async fn execute(
            &self,
            request: PromptRequest,
        ) -> Result<PromptOutcome, PromptExecutorError> {
            self.requests.lock().expect("requests lock").push(request);
            self.outcomes
                .lock()
                .expect("outcomes lock")
                .pop_front()
                .unwrap_or_else(|| Ok(PromptOutcome::end_turn()))
        }
    }

    impl RecordingPromptExecutor {
        fn push_outcome(&self, outcome: Result<PromptOutcome, PromptExecutorError>) {
            self.outcomes
                .lock()
                .expect("outcomes lock")
                .push_back(outcome);
        }

        fn take_requests(&self) -> Vec<PromptRequest> {
            std::mem::take(&mut *self.requests.lock().expect("requests lock"))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn inline_shell_completion_automatically_starts_agent_reply() {
        let root = tempfile::tempdir().unwrap();
        let mut session = make_test_tui_session();
        Arc::get_mut(&mut session.engine_half.runtime)
            .unwrap()
            .projects_root = root.path().into();
        session.engine_half.engine = Arc::new(rebon_core::Engine::new());
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut app = AppState::new();
        let handle = Handle::current();
        super::super::task_runtime::spawn_inline_shell_command(
            &mut app, &session, &handle, "fixture", "!fixture",
        );
        let path =
            rebon_session::transcript_file_path(root.path(), &session.cwd, &session.session_id);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !path.exists() {
                super::super::task_runtime::drain_inline_shell_commands(&mut app, &session, true);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shell result persisted");
        let mut active_prompt = None;
        maybe_update_loading_state(
            &mut app,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
        );
        assert!(
            active_prompt.is_some(),
            "shell completion must start an agent reply without user input"
        );
        (&mut active_prompt.as_mut().unwrap().rx)
            .await
            .unwrap()
            .unwrap();
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert!(matches!(&requests[0].prompt[0], ContentBlock::Text(text)
            if text.text.contains("Respond to") && text.text.contains("!fixture")));
        assert!(app.deferred_internal_submit_payloads.is_empty());
        let transcript = rebon_session::load_transcript_from_file(&path)
            .unwrap()
            .unwrap();
        assert_eq!(transcript.messages.len(), 1);
        let replay = rebon_core::query::transcript_to_api_messages(&transcript.messages);
        assert!(serde_json::to_string(&replay)
            .unwrap()
            .contains("Command failed:"));
    }

    #[test]
    fn idle_internal_reply_waits_for_admissible_runtime() {
        for blocked in ["replacement", "projection", "handover", "active"] {
            let (runtime, handle) = make_immediate_handle();
            let mut session = make_test_tui_session();
            let mut app = AppState::new();
            app.deferred_internal_submit_payloads
                .push(admission_test_submit("shell reply"));
            let (tx, rx) = oneshot::channel();
            let mut active = None;
            match blocked {
                "replacement" => session.session_id = "not-installed".into(),
                "projection" => session
                    .engine_half
                    .projection_invalid
                    .store(true, std::sync::atomic::Ordering::Release),
                "handover" => {
                    session.pending_hosted_session = Some(
                        crate::background::PendingHostedSession::handover("bg".into()),
                    )
                }
                "active" => active = Some(ActivePrompt::new(rx, PromptCancel::new())),
                _ => unreachable!(),
            }
            maybe_update_loading_state(&mut app, &mut session, &handle, &mut active, &mut None);
            assert_eq!(app.deferred_internal_submit_payloads.len(), 1, "{blocked}");
            assert_eq!(active.is_some(), blocked == "active");
            assert!(app.rebon_tui.transcript.is_empty());
            drop(tx);
            drop(active);
            runtime.shutdown_background();
        }
    }

    #[test]
    fn maybe_spawn_next_queued_prompt_runs_deferred_internal_payload_first() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        app.deferred_internal_submit_payloads.push(SubmitPayload {
            text: "internal continuation".into(),
            model_text: None,
            user_message_uuid: None,
            image_pastes: Vec::new(),
            directory_attachments: Vec::new(),
            execution_policy: None,
            skill_invocations: Vec::new(),
        });
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "visible queued".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        assert!(app.deferred_internal_submit_payloads.is_empty());
        assert_eq!(app.queued_submit_payloads.len(), 1);
        assert_eq!(queued_text(&app.queued_commands[0]), Some("visible queued"));
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            &requests[0].prompt[0],
            ContentBlock::Text(text) if text.text == "internal continuation"
        ));

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn maybe_spawn_next_queued_prompt_drains_deferred_goal_payload_with_hidden_model_text() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        app.goal = Some(crate::goal::GoalState::new_now("ship it"));
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        app.deferred_goal_submit_payloads.push(SubmitPayload {
            text: "visible next step".into(),
            model_text: Some("hidden model summary with prior transcript".into()),
            user_message_uuid: None,
            image_pastes: Vec::new(),
            directory_attachments: Vec::new(),
            execution_policy: None,
            skill_invocations: Vec::new(),
        });
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "editable queued".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        assert!(app.deferred_goal_submit_payloads.is_empty());
        assert_eq!(app.queued_submit_payloads.len(), 1);
        assert_eq!(
            queued_text(&app.queued_commands[0]),
            Some("editable queued")
        );
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 1);
        let rebon_tui::Message::User(user) = &rows[0] else {
            panic!("expected deferred goal user message, got {:?}", rows[0]);
        };
        match &user.message.content[0] {
            rebon_tui::UserContentBlock::Text(text) => assert_eq!(text.text, "visible next step"),
            other => panic!("expected text block, got {other:?}"),
        }
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            prompt_text(&requests[0]),
            "hidden model summary with prior transcript"
        );
        assert!(!prompt_text(&requests[0]).contains("visible next step"));
        assert_eq!(
            requests[0].user_message_uuid.as_deref(),
            Some(user.uuid.as_str())
        );
        let policy = requests[0]
            .execution_policy
            .as_ref()
            .expect("active goal continuity policy");
        assert!(policy.auto_mode_script_continuity);
        assert!(policy.ultraplan.is_none());
        let withdrawable = active_prompt
            .as_ref()
            .and_then(|active| active.withdrawable.as_ref())
            .expect("deferred goal active prompt is withdrawable");
        match &withdrawable.destination {
            WithdrawDestination::Discard => {}
            other => panic!("expected discard destination, got {other:?}"),
        }
        assert_eq!(withdrawable.transcript_len_before, 0);
        assert_eq!(withdrawable.transcript_len_after, 1);

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn maybe_spawn_next_queued_prompt_preserves_wrapped_ultraplan_run_id_and_status() {
        let _guard = crate::test_env::lock_env();
        let prev_trigger = std::env::var_os("REBON_ULTRAPLAN_IMPLICIT_TRIGGER");
        unsafe {
            std::env::set_var("REBON_ULTRAPLAN_IMPLICIT_TRIGGER", "1");
        }

        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        app.ultraplan_status = Some(UltraplanStatus {
            run_id: "original-run".into(),
            phase: UltraplanPhase::PlanModeActive,
            task_title: "original task".into(),
            started_at_ms: Some(123),
            worker_count: None,
            context: None,
            round: 1,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        });
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "ultraplan original task".into(),
                model_text: Some(build_ultraplan_prompt(
                    "original task",
                    "original-run",
                    None,
                )),
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: Some(ExecutionPolicy::ultraplan(
                    UltraplanContext::planning_turn(
                        "original-run",
                        "planmodeactive",
                        PolicyMode::Enforce,
                    ),
                )),
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        let status = app.ultraplan_status.as_ref().expect("ultraplan status");
        assert_eq!(status.run_id, "original-run");
        assert_eq!(status.task_title, "original task");
        assert_eq!(status.started_at_ms, Some(123));
        assert_eq!(status.phase, UltraplanPhase::Orchestrating);
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let prompt_text = match &requests[0].prompt[0] {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("expected text prompt, got {other:?}"),
        };
        assert!(prompt_text.contains("ULTRAPLAN_ID: original-run"));
        assert!(!prompt_text.contains("ULTRAPLAN_ID: ultraplan-"));
        let ctx = requests[0]
            .execution_policy
            .as_ref()
            .and_then(|policy| policy.ultraplan.as_ref())
            .expect("ultraplan context");
        assert_eq!(ctx.run_id, "original-run");

        drop(active_prompt.take());
        runtime.shutdown_background();
        unsafe {
            match prev_trigger {
                Some(value) => std::env::set_var("REBON_ULTRAPLAN_IMPLICIT_TRIGGER", value),
                None => std::env::remove_var("REBON_ULTRAPLAN_IMPLICIT_TRIGGER"),
            }
        }
    }

    #[test]
    fn maybe_spawn_next_queued_prompt_wraps_unwrapped_implicit_ultraplan_payload() {
        let _guard = crate::test_env::lock_env();
        let prev_trigger = std::env::var_os("REBON_ULTRAPLAN_IMPLICIT_TRIGGER");
        unsafe {
            std::env::set_var("REBON_ULTRAPLAN_IMPLICIT_TRIGGER", "1");
        }

        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_ultraplan_test_tui_session();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "please ultraplan this migration".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        let status = app.ultraplan_status.as_ref().expect("ultraplan status");
        assert!(status.run_id.starts_with("ultraplan-"));
        assert_eq!(status.task_title, "please ultraplan this migration");
        assert_eq!(status.phase, UltraplanPhase::Orchestrating);
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let prompt_text = match &requests[0].prompt[0] {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("expected text prompt, got {other:?}"),
        };
        assert!(prompt_text.starts_with("You are starting REBON LOCAL ULTRAPLAN"));
        assert!(prompt_text.contains(&format!("ULTRAPLAN_ID: {}", status.run_id)));
        let ctx = requests[0]
            .execution_policy
            .as_ref()
            .and_then(|policy| policy.ultraplan.as_ref())
            .expect("ultraplan context");
        assert_eq!(ctx.run_id, status.run_id);

        drop(active_prompt.take());
        runtime.shutdown_background();
        unsafe {
            match prev_trigger {
                Some(value) => std::env::set_var("REBON_ULTRAPLAN_IMPLICIT_TRIGGER", value),
                None => std::env::remove_var("REBON_ULTRAPLAN_IMPLICIT_TRIGGER"),
            }
        }
    }

    #[test]
    fn maybe_spawn_next_queued_prompt_skips_mid_turn_consumed_payloads() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let poller = session.engine_half.runtime.mid_turn_queue.clone();
        app.mid_turn_queued_submit_poller = Some(poller.clone());
        app.is_loading = true;
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "mid turn".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        assert_eq!(
            poller
                .poll(AttachmentPollRequest::new(
                    &session.session_id,
                    "test-turn",
                    1,
                    AttachmentPollPhase::Regular,
                ))
                .len(),
            1
        );
        app.is_loading = false;
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);

        assert!(active_prompt.is_none());
        assert!(recorder.take_requests().is_empty());
        assert!(app.queued_commands.is_empty());
        assert!(app.queued_submit_payloads.is_empty());
        runtime.shutdown_background();
    }

    #[test]
    fn maybe_spawn_next_queued_prompt_keeps_unpolled_payload_for_fallback() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.mid_turn_queued_submit_poller =
            Some(session.engine_half.runtime.mid_turn_queue.clone());
        app.is_loading = true;
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "fallback".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: vec![DirectoryAttachment {
                    path: "docs".into(),
                    display_path: "@docs".into(),
                    content: "README.md\n".into(),
                }],
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        app.is_loading = false;
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert!(app.queued_commands.is_empty());
        assert!(app.queued_submit_payloads.is_empty());
        assert_eq!(session.engine_half.runtime.mid_turn_queue.pending_len(), 0);
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 2);
        let rebon_tui::Message::User(user) = &rows[0] else {
            panic!("expected fallback user message, got {:?}", rows[0]);
        };
        assert_eq!(
            requests[0].user_message_uuid.as_deref(),
            Some(user.uuid.as_str())
        );
        match &user.message.content[0] {
            rebon_tui::UserContentBlock::Text(text) => assert_eq!(text.text, "fallback"),
            other => panic!("expected text block, got {other:?}"),
        }
        assert!(matches!(rows[1], rebon_tui::Message::Attachment(_)));
        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn maybe_spawn_next_queued_prompt_commits_user_message_to_transcript() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "queued work".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        assert!(app.queued_commands.is_empty());
        assert!(app.queued_submit_payloads.is_empty());
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 1);
        let rebon_tui::Message::User(user) = &rows[0] else {
            panic!("expected queued user message, got {:?}", rows[0]);
        };
        assert!(user
            .uuid
            .starts_with(&format!("u-user-{}-", session.session_id)));
        match &user.message.content[0] {
            rebon_tui::UserContentBlock::Text(text) => assert_eq!(text.text, "queued work"),
            other => panic!("expected text block, got {other:?}"),
        }
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].user_message_uuid.as_deref(),
            Some(user.uuid.as_str())
        );
        let withdrawable = active_prompt
            .as_ref()
            .and_then(|active| active.withdrawable.as_ref())
            .expect("queued active prompt is withdrawable");
        match &withdrawable.destination {
            WithdrawDestination::QueuedFront { submit, .. } => {
                assert_eq!(submit.text, "queued work")
            }
            other => panic!("expected queued destination, got {other:?}"),
        }
        assert_eq!(withdrawable.transcript_len_before, 0);
        assert_eq!(withdrawable.transcript_len_after, 1);
        assert_eq!(
            withdrawable.user_message_uuid.as_deref(),
            Some(user.uuid.as_str())
        );

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn queued_auto_drained_prompt_withdraws_on_esc_before_visible_reply() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        app.input = "draft stays".into();
        app.cursor_offset = 5;
        app.pasted_contents.push(PromptPasteContent {
            id: 2,
            kind: "text".into(),
            content: "draft paste".into(),
            media_type: None,
            filename: None,
            source_path: None,
        });
        app.next_paste_id = 3;
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder);
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "queued with [Pasted image #7]".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: vec![PromptPasteContent {
                    id: 7,
                    kind: "image".into(),
                    content: "abc".into(),
                    media_type: Some("image/png".into()),
                    filename: None,
                    source_path: None,
                }],
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());
        assert_eq!(app.rebon_tui.transcript.len(), 1);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active_prompt,
            UiMode::Screen
        ));

        assert!(active_prompt.is_none());
        assert_eq!(app.rebon_tui.transcript.len(), 0);
        assert_eq!(app.input, "draft stays");
        assert_eq!(app.cursor_offset, 5);
        assert_eq!(app.pasted_contents.len(), 1);
        assert_eq!(app.pasted_contents[0].id, 2);
        assert_eq!(app.next_paste_id, 3);
        assert_eq!(app.queued_commands.len(), 1);
        assert_eq!(app.queued_submit_payloads.len(), 1);
        assert_eq!(
            app.queued_submit_payloads[0].text,
            "queued with [Pasted image #7]"
        );
        assert_eq!(app.queued_submit_payloads[0].image_pastes.len(), 1);
        assert_eq!(app.queued_submit_payloads[0].image_pastes[0].id, 7);
        assert!(app.queued_auto_drain_paused_after_withdrawal);
        assert!(app.suppress_late_visible_updates_after_withdrawal);
        runtime.shutdown_background();
    }

    #[test]
    fn queued_auto_drained_prompt_withdraws_on_ctrl_c_and_does_not_exit() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder);
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "queued ctrl c".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active_prompt,
            UiMode::Screen
        ));

        assert!(active_prompt.is_none());
        assert_eq!(app.rebon_tui.transcript.len(), 0);
        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
        assert_eq!(app.queued_submit_payloads.len(), 1);
        assert_eq!(app.queued_submit_payloads[0].text, "queued ctrl c");
        assert!(app.queued_auto_drain_paused_after_withdrawal);
        runtime.shutdown_background();
    }

    #[test]
    fn queued_auto_drained_prompt_after_visible_reply_cancels_instead_of_withdrawing() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder);
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "queued visible".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());
        app.rebon_tui.overlay.set_streaming_text("partial");
        if let Some(active) = active_prompt.as_mut() {
            active.reply_started = true;
        }

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active_prompt,
            UiMode::Screen
        ));

        assert!(active_prompt.is_none());
        assert_eq!(app.input, "");
        assert!(app.queued_submit_payloads.is_empty());
        assert!(!app.queued_auto_drain_paused_after_withdrawal);
        assert_eq!(app.rebon_tui.transcript.len(), 2);
        let rows = app.rebon_tui.transcript.rows();
        assert!(
            matches!(&rows[0], rebon_tui::Message::User(user) if user.uuid.starts_with(&format!("u-user-{}-", session.session_id)))
        );
        assert!(app.rebon_tui.overlay.is_empty());
        runtime.shutdown_background();
    }

    #[test]
    fn queued_auto_drain_after_withdrawal_drains_stale_updates_and_allows_next_visible_chunk() {
        let (runtime, handle) = make_immediate_handle();
        let (tx, update_rx): (_, UnboundedReceiver<SessionUpdateParams>) = unbounded_channel();
        send_text_chunk(&tx, "sess-1", "stale A reply");
        let mut app = AppState::new();
        app.suppress_late_visible_updates_after_withdrawal = true;
        app.rebon_tui.overlay.set_streaming_text("stale overlay");
        let mut session = make_test_tui_session();
        session.engine_half.update_rx = update_rx;
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder);
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "queued B".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(active_prompt.is_some());
        assert!(!app.suppress_late_visible_updates_after_withdrawal);
        assert!(app.rebon_tui.overlay.is_empty());
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        let rows = app.rebon_tui.transcript.rows();
        assert!(
            matches!(&rows[0], rebon_tui::Message::User(user) if user.message.content.iter().any(|block| matches!(block, rebon_tui::UserContentBlock::Text(text) if text.text == "queued B")))
        );

        translate_session_update(
            &mut app,
            SessionUpdateParams {
                session_id: "sess-1".into(),
                update: SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text(TextContent {
                        text: "B visible".into(),
                        annotations: None,
                    }),
                },
            },
        );

        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("B visible")
        );
        assert_eq!(app.rebon_tui.transcript.len(), 1);

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    #[test]
    fn queued_auto_drained_prompt_with_directory_attachment_withdraws_attachment_tail_only() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u-prev", "previous row");
        let mut session = make_test_tui_session();
        app.tasks = session.engine_half.tasks.clone();
        let recorder = Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder);
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "queued dir @docs".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: vec![DirectoryAttachment {
                    path: "docs".into(),
                    display_path: "@docs".into(),
                    content: "README.md\n".into(),
                }],
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());
        assert_eq!(app.rebon_tui.transcript.len(), 3);
        assert!(matches!(
            app.rebon_tui.transcript.rows()[0],
            rebon_tui::Message::User(_)
        ));
        assert!(matches!(
            app.rebon_tui.transcript.rows()[1],
            rebon_tui::Message::User(_)
        ));
        assert!(matches!(
            app.rebon_tui.transcript.rows()[2],
            rebon_tui::Message::Attachment(_)
        ));

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active_prompt,
            UiMode::Screen
        ));

        assert!(active_prompt.is_none());
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        let rows = app.rebon_tui.transcript.rows();
        assert!(matches!(&rows[0], rebon_tui::Message::User(user) if user.uuid == "u-prev"));
        assert_eq!(app.input, "");
        assert_eq!(app.cursor_offset, 0);
        assert!(app.pasted_contents.is_empty());
        assert_eq!(app.queued_submit_payloads.len(), 1);
        assert_eq!(app.queued_submit_payloads[0].text, "queued dir @docs");
        assert_eq!(app.queued_submit_payloads[0].directory_attachments.len(), 1);
        assert_eq!(
            app.queued_submit_payloads[0].directory_attachments[0].display_path,
            "@docs"
        );
        assert!(app.queued_auto_drain_paused_after_withdrawal);
        assert!(app.suppress_late_visible_updates_after_withdrawal);

        runtime.shutdown_background();
    }

    #[test]
    fn queued_withdrawal_pauses_auto_drain_until_up_arrow_retrieval() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        session.set_test_executor(Arc::new(RecordingPromptExecutor::default()));
        for text in ["queued A", "queued B"] {
            enqueue_submit_payload(
                &mut app,
                SubmitPayload {
                    text: text.into(),
                    model_text: None,
                    user_message_uuid: None,
                    image_pastes: Vec::new(),
                    directory_attachments: Vec::new(),
                    execution_policy: None,
                    skill_invocations: Vec::new(),
                },
            );
        }
        let mut active_prompt = None;
        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active_prompt,
            UiMode::Screen
        ));
        assert!(app.queued_auto_drain_paused_after_withdrawal);
        assert_eq!(
            app.queued_submit_payloads
                .iter()
                .map(|submit| submit.text.as_str())
                .collect::<Vec<_>>(),
            vec!["queued A", "queued B"]
        );

        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "queued C".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        assert!(app.queued_auto_drain_paused_after_withdrawal);
        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);

        assert!(active_prompt.is_none());
        assert_eq!(app.rebon_tui.transcript.len(), 0);
        assert_eq!(
            app.queued_submit_payloads
                .iter()
                .map(|submit| submit.text.as_str())
                .collect::<Vec<_>>(),
            vec!["queued A", "queued B", "queued C"]
        );
        pop_queued_command_into_input(&mut app);
        assert_eq!(app.input, "queued C");
        assert_eq!(app.cursor_offset, "queued C".len());
        assert_eq!(
            app.queued_submit_payloads
                .iter()
                .map(|submit| submit.text.as_str())
                .collect::<Vec<_>>(),
            vec!["queued A", "queued B"]
        );
        assert!(!app.queued_auto_drain_paused_after_withdrawal);

        runtime.shutdown_background();
    }

    #[test]
    fn queued_withdrawal_requeues_with_original_mode_and_payload_alignment() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        session.set_test_executor(Arc::new(RecordingPromptExecutor::default()));
        app.mode = "original-mode".into();
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "queued A".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        app.mode = "b-mode".into();
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "queued B".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let mut active_prompt = None;
        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());
        app.mode = "current-mode-at-withdraw".into();

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active_prompt,
            UiMode::Screen
        ));

        assert_eq!(app.queued_commands.len(), 2);
        assert_eq!(app.queued_submit_payloads.len(), 2);
        assert_eq!(app.queued_commands[0].mode, "original-mode");
        assert_eq!(app.queued_submit_payloads[0].text, "queued A");
        assert!(matches!(
            &app.queued_commands[0].value,
            rebon_tui::promptinput::QueuedCommandValue::Text(text) if text == "queued A"
        ));
        assert_eq!(app.queued_commands[1].mode, "b-mode");
        assert_eq!(app.queued_submit_payloads[1].text, "queued B");
        assert!(matches!(
            &app.queued_commands[1].value,
            rebon_tui::promptinput::QueuedCommandValue::Text(text) if text == "queued B"
        ));

        runtime.shutdown_background();
    }

    #[test]
    fn queued_withdrawal_requeues_active_submit_at_front_preserving_order() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        session.set_test_executor(Arc::new(RecordingPromptExecutor::default()));
        for text in ["queued A", "queued B"] {
            enqueue_submit_payload(
                &mut app,
                SubmitPayload {
                    text: text.into(),
                    model_text: None,
                    user_message_uuid: None,
                    image_pastes: Vec::new(),
                    directory_attachments: Vec::new(),
                    execution_policy: None,
                    skill_invocations: Vec::new(),
                },
            );
        }
        let mut active_prompt = None;
        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);
        runtime.block_on(tokio::task::yield_now());
        assert_eq!(app.queued_submit_payloads[0].text, "queued B");

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active_prompt,
            UiMode::Screen
        ));

        let queued_texts = app
            .queued_submit_payloads
            .iter()
            .map(|submit| submit.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(queued_texts, vec!["queued A", "queued B"]);
        runtime.shutdown_background();
    }

    #[test]
    fn maybe_spawn_next_queued_prompt_does_not_drain_while_active_prompt_exists() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "queued work".into(),
                model_text: Some("queued model payload".into()),
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let (_tx, rx) = oneshot::channel();
        let active_cancel = PromptCancel::new();
        let mut active_prompt = Some(ActivePrompt::with_task_notifications(
            rx,
            active_cancel.clone(),
            vec![rebon_plugin_tasks::runtime::TaskId::new("sentinel-active")],
            Vec::new(),
        ));

        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active_prompt);

        assert_eq!(app.queued_commands.len(), 1);
        assert_eq!(app.queued_submit_payloads.len(), 1);
        assert_eq!(app.queued_submit_payloads[0].text, "queued work");
        let active = active_prompt.as_ref().expect("active prompt preserved");
        assert_eq!(
            active.pending_task_notification_ids.as_slice(),
            &[rebon_plugin_tasks::runtime::TaskId::new("sentinel-active")]
        );
        assert_eq!(active.cancel.is_cancelled(), active_cancel.is_cancelled());

        drop(active_prompt.take());
        runtime.shutdown_background();
    }

    fn push_test_user_message(app: &mut AppState, uuid: &str, text: &str) {
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_tui::Message::User(rebon_tui::UserMessage {
                uuid: uuid.to_string(),
                timestamp: format!("2026-04-14T00:00:00.{}Z", uuid.trim_start_matches('u')),
                message: rebon_tui::UserMessageInner {
                    role: rebon_tui::UserRole::User,
                    content: vec![rebon_tui::UserContentBlock::Text(
                        rebon_tui::UserTextBlock {
                            text: text.to_string(),
                        },
                    )],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            })),
        );
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
        .expect("send failed");
    }
}
