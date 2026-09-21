use std::time::SystemTime;

use rebon_hooks::HookEventPayload;
use rebon_plugin_tasks::runtime::TaskRegistry;
use rebon_tui::promptinput::clamp_cursor_offset;
use rebon_types::format_system_time_iso_ms;

use crate::tui::app::AppState;
use crate::tui::dispatch::{
    clear_input, next_paste_id_after_restore, requeue_submit_payload_front,
};
use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::UiMode;

use super::live_agent_view::{switch_to_main_agent, with_main_agent_view};
use super::render::note_cancel_race;
use super::rewind::open_rewind_dialog;
use super::status_bar::wall_clock_ms;
use super::{ActivePrompt, WithdrawDestination};

pub(super) fn apply_cancel_or_exit(
    app: &mut AppState,
    session: &TuiEngineSession,
    active_prompt: &mut Option<ActivePrompt>,
    ui_mode: UiMode,
) -> bool {
    if app.agent_view.is_some() {
        // First Ctrl+C in the Agent View is narrowed to the task under the
        // cursor: stop ONLY the selected in-process task, never sibling tasks.
        // When no running task is highlighted, fall back to cancelling the main
        // in-flight turn (see `remote_cancel_active_prompt`) — but still kill
        // nothing else. Either way arm the two-press exit; a second Ctrl+C
        // within 2s exits.
        let stopped_task = stop_selected_agent_view_task(app, session.engine_half.tasks.as_ref());
        let cancelled_prompt = if stopped_task {
            false
        } else {
            remote_cancel_active_prompt(app, session, active_prompt, ui_mode)
        };
        if stopped_task || cancelled_prompt {
            app.last_ctrl_c_exit_press_ms = wall_clock_ms();
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(if stopped_task {
                    "stopped current task; press Ctrl+C again within 2s to exit".to_string()
                } else {
                    "cancelled current session; press Ctrl+C again within 2s to exit".to_string()
                });
            }
            return false;
        }
        // Nothing under the cursor to stop and no in-flight turn. Agent View
        // keeps Ctrl+C scoped to the selected row; its bulk task controls use
        // the dedicated task-management shortcuts instead.
        return confirm_ctrl_c_double_press_exit(app);
    }

    // A second Ctrl+C within the exit window exits BEFORE anything else is
    // cancelled. Several interrupt branches "succeed" on every press without
    // the underlying state ever changing (a remote job whose task snapshots
    // never record the kill, a dead worker whose cancel errors each time), and
    // letting each of those clear the window made the session impossible to
    // exit with Ctrl+C at all.
    if ctrl_c_exit_armed(app, wall_clock_ms()) {
        return resolve_ctrl_c_exit(app);
    }

    let interrupted = apply_interrupt(app, session, active_prompt, ui_mode);
    let stopped_session_tasks = stop_remaining_session_tasks(app, session);
    if interrupted || stopped_session_tasks {
        // Arm the two-press exit instead of clearing it, so the next press
        // within the window exits even when another cancellable target
        // remains. The footer shows "press Ctrl+C again to exit" while armed.
        app.last_esc_press_ms = 0;
        app.last_ctrl_c_exit_press_ms = wall_clock_ms();
        return false;
    }

    if !app.input.is_empty() {
        clear_input(app);
        app.last_ctrl_c_exit_press_ms = 0;
        app.last_esc_press_ms = 0;
        return false;
    }

    confirm_ctrl_c_double_press_exit(app)
}

/// Ctrl+C before the session exists: nothing is running that
/// could be cancelled, so the press clears the composer, or arms the same
/// two-press exit a session would, or exits.
pub(super) fn apply_cancel_or_exit_without_session(app: &mut AppState) -> bool {
    if app.selection.has_selection() {
        app.pending_copy = true;
        return false;
    }
    if ctrl_c_exit_armed(app, wall_clock_ms()) {
        return resolve_ctrl_c_exit(app);
    }
    if !app.input.is_empty() {
        clear_input(app);
        app.last_ctrl_c_exit_press_ms = 0;
        app.last_esc_press_ms = 0;
        return false;
    }
    confirm_ctrl_c_double_press_exit(app)
}

/// Esc before the session exists: the parts of [`apply_interrupt`] that
/// act on the composer alone. There is no turn to withdraw and no rewind
/// dialog to open yet.
pub(super) fn apply_interrupt_without_session(app: &mut AppState) -> bool {
    if app.help_open {
        app.help_open = false;
        app.last_esc_press_ms = 0;
        return true;
    }
    if !app.input.is_empty() && app.input.trim().is_empty() {
        clear_input(app);
        app.last_esc_press_ms = 0;
        return true;
    }
    if app.mode == "bash" && app.input.is_empty() {
        app.mode = String::from("prompt");
        app.last_esc_press_ms = 0;
        return true;
    }
    if app.input.is_empty() {
        return false;
    }
    let now_ms = wall_clock_ms();
    let is_double_press =
        app.last_esc_press_ms != 0 && now_ms.saturating_sub(app.last_esc_press_ms) <= 400;
    app.last_esc_press_ms = now_ms;
    if is_double_press {
        clear_input(app);
        app.last_esc_press_ms = 0;
        return true;
    }
    false
}

/// Arm the two-press Ctrl+C exit on the first press, and resolve the exit on a
/// second press within the 2s window. Shared by the Agent View and the normal
/// session cancel/exit paths.
fn confirm_ctrl_c_double_press_exit(app: &mut AppState) -> bool {
    app.last_esc_press_ms = 0;
    let now_ms = wall_clock_ms();
    if ctrl_c_exit_armed(app, now_ms) {
        return resolve_ctrl_c_exit(app);
    }
    app.last_ctrl_c_exit_press_ms = now_ms;
    false
}

/// Whether a previous Ctrl+C press armed the two-press exit and is still
/// within the confirmation window.
fn ctrl_c_exit_armed(app: &AppState, now_ms: u64) -> bool {
    app.last_ctrl_c_exit_press_ms != 0
        && now_ms.saturating_sub(app.last_ctrl_c_exit_press_ms) <= 2_000
}

fn resolve_ctrl_c_exit(app: &mut AppState) -> bool {
    app.last_ctrl_c_exit_press_ms = 0;

    // Wire rebon-dialog's exit_flow for the goodbye message.
    // established behavior picks a random goodbye; we use the first.
    let rebon_dialog::exit_flow::ExitFlowAction::Exit { message, .. } =
        rebon_dialog::exit_flow::resolve_exit(None, 0);
    tracing::info!("{message}");
    true
}

/// The coordinator task id under the Agent View cursor, but only when the
/// highlighted row is an in-process task (not a background job, a group header,
/// or a "More" row) that is still running or pending. Job-backed rows have their
/// own supervisor lifecycle and are stopped via Ctrl+X, never here.
fn selected_agent_view_task_id(
    app: &AppState,
    tasks: &TaskRegistry,
) -> Option<rebon_plugin_tasks::runtime::TaskId> {
    use rebon_plugin_tasks::runtime::{TaskId, TaskStatus};

    let row = app.agent_view.as_ref()?.selected_row()?;
    if row.source != crate::tui::agent_view::AgentViewRowSource::Task {
        return None;
    }
    let id = TaskId::new(row.id.clone());
    let snapshot = tasks.snapshot(&id)?;
    matches!(snapshot.status, TaskStatus::Running | TaskStatus::Pending).then_some(id)
}

/// Stop ONLY the task highlighted in the Agent View, leaving every sibling
/// session task untouched. Returns true when a running task was stopped.
fn stop_selected_agent_view_task(app: &mut AppState, tasks: &TaskRegistry) -> bool {
    use rebon_plugin_tasks::runtime::stop_task;

    let Some(task_id) = selected_agent_view_task_id(app, tasks) else {
        return false;
    };
    if stop_task(tasks, &task_id).is_err() {
        return false;
    }
    tasks.escalation_registry().cancel_agent(
        task_id.as_str(),
        "selected agent was stopped by Ctrl+C in the Agent View",
    );
    true
}

pub(super) fn apply_interrupt(
    app: &mut AppState,
    session: &TuiEngineSession,
    active_prompt: &mut Option<ActivePrompt>,
    ui_mode: UiMode,
) -> bool {
    if app.help_open {
        app.help_open = false;
        app.last_esc_press_ms = 0;
        return true;
    }

    // Whitespace-only input isn't a real draft: discard it instead of
    // treating it as content worth protecting around interrupts.
    if !app.input.is_empty() && app.input.trim().is_empty() {
        clear_input(app);
        app.last_esc_press_ms = 0;
        return true;
    }

    if app.mode == "bash" && app.input.is_empty() {
        app.mode = String::from("prompt");
        app.last_esc_press_ms = 0;
        return true;
    }

    let mut interrupted = false;

    if let Some(active) = active_prompt.take() {
        if let Err(reason) = run_stop_hook(session, Some("active_prompt_cancel".to_string())) {
            tracing::info!(reason = %reason, "Stop hook blocked active prompt cancellation");
            *active_prompt = Some(active);
            return true;
        }
        if try_withdraw_active_prompt(app, session, active_prompt, active, ui_mode) {
            return true;
        }
        let active = active_prompt
            .take()
            .expect("active prompt restored after failed withdrawal");
        cancel_active_prompt(app, session, active, ui_mode);
        interrupted = true;
    }

    if !interrupted {
        // A foregrounded remote agent is the most specific target: stop just
        // that agent over IPC before falling back to cancelling the whole
        // remote turn.
        interrupted |= interrupt_foregrounded_remote_task(app, session);
    }
    if !interrupted {
        // A parked attachment has no worker to cancel anything in.
        if let Some(remote) = session
            .remote_background_attachment
            .as_ref()
            .filter(|remote| remote.is_live())
        {
            match crate::background::cancel_background_job_turn(&remote.job_id) {
                Ok(cancelled) => interrupted = cancelled,
                Err(err) => {
                    super::inject_system_message(
                        app,
                        "error",
                        &format!("Failed to cancel background turn: {err}"),
                    );
                    return true;
                }
            }
        }
    }
    if !interrupted {
        interrupted |= interrupt_foregrounded_task(app, session.engine_half.tasks.as_ref());
    }

    if interrupted {
        app.last_esc_press_ms = 0;
        return true;
    }

    if app.input.is_empty() {
        let now_ms = wall_clock_ms();
        let double_press_window_ms = 400;
        let is_double_press = app.last_esc_press_ms != 0
            && now_ms.saturating_sub(app.last_esc_press_ms) <= double_press_window_ms;
        app.last_esc_press_ms = now_ms;
        if is_double_press {
            app.rewind_dialog = Some(open_rewind_dialog(app, session));
            app.last_esc_press_ms = 0;
            return true;
        }
        return false;
    }

    let now_ms = wall_clock_ms();
    let double_press_window_ms = 400;
    let is_double_press = app.last_esc_press_ms != 0
        && now_ms.saturating_sub(app.last_esc_press_ms) <= double_press_window_ms;
    app.last_esc_press_ms = now_ms;

    if is_double_press {
        clear_input(app);
        app.last_esc_press_ms = 0;
        return true;
    }

    false
}

fn cancel_active_prompt(
    app: &mut AppState,
    session: &TuiEngineSession,
    mut active: ActivePrompt,
    ui_mode: UiMode,
) {
    if ui_mode == UiMode::Inline && !app.input.is_empty() {
        clear_input(app);
    }
    active.finish_question_escalation_notifications();
    active.finish_task_notification_claim(false);
    active.cancel.cancel();
    if app.foregrounded_task_id.is_some() {
        switch_to_main_agent(app);
    }
    note_cancel_race();
    cancel_commit(app, &session.session_id);
}

/// Hard-cancel the in-flight prompt on behalf of a remote controller (the
/// desktop app's foreground command mailbox). Unlike [`apply_interrupt`], this
/// never tries to withdraw the prompt back into the composer draft and never
/// consults the local input / double-Esc state — a remote `Stop` is an explicit
/// interrupt, not an undo. Returns `true` when a prompt was cancelled.
pub(super) fn remote_cancel_active_prompt(
    app: &mut AppState,
    session: &TuiEngineSession,
    active_prompt: &mut Option<ActivePrompt>,
    ui_mode: UiMode,
) -> bool {
    if let Some(active) = active_prompt.take() {
        cancel_active_prompt(app, session, active, ui_mode);
        true
    } else {
        false
    }
}

fn try_withdraw_active_prompt(
    app: &mut AppState,
    session: &TuiEngineSession,
    active_prompt: &mut Option<ActivePrompt>,
    active: ActivePrompt,
    ui_mode: UiMode,
) -> bool {
    let had_foreground_agent = app.foregrounded_task_id.is_some();
    let withdrawn = with_main_agent_view(app, |app| {
        try_withdraw_active_prompt_from_main(app, session, active_prompt, active, ui_mode)
    });
    if withdrawn && had_foreground_agent {
        switch_to_main_agent(app);
    }
    withdrawn
}

fn try_withdraw_active_prompt_from_main(
    app: &mut AppState,
    session: &TuiEngineSession,
    active_prompt: &mut Option<ActivePrompt>,
    active: ActivePrompt,
    ui_mode: UiMode,
) -> bool {
    if active.reply_started || !app.rebon_tui.overlay.is_empty() {
        *active_prompt = Some(active);
        return false;
    }
    let Some(withdrawable) = active.withdrawable.as_ref() else {
        *active_prompt = Some(active);
        return false;
    };
    if app.rebon_tui.transcript.len() != withdrawable.transcript_len_after {
        *active_prompt = Some(active);
        return false;
    }
    let rows = app.rebon_tui.transcript.rows();
    let added_rows = rows
        .get(withdrawable.transcript_len_before..withdrawable.transcript_len_after)
        .unwrap_or(&[]);
    let user_row_ok = added_rows.iter().any(|row| {
        matches!(
            (row, withdrawable.user_message_uuid.as_deref()),
            (rebon_tui::Message::User(user), Some(uuid)) if user.uuid == uuid
        )
    });
    if !user_row_ok {
        *active_prompt = Some(active);
        return false;
    }

    let withdrawable = active
        .withdrawable
        .clone()
        .expect("withdrawable checked above");
    active.cancel.cancel();
    note_cancel_race();
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::TruncateTranscript {
            len: withdrawable.transcript_len_before,
        },
    );
    app.rebon_tui.overlay.clear();
    match withdrawable.destination {
        WithdrawDestination::LiveInput {
            text,
            cursor_offset,
            image_pastes,
        } => {
            app.input = text;
            app.cursor_offset = clamp_cursor_offset(&app.input, cursor_offset);
            app.pasted_contents = image_pastes;
            app.next_paste_id = next_paste_id_after_restore(&app.pasted_contents);
        }
        WithdrawDestination::QueuedFront { submit, mode } => {
            requeue_submit_payload_front(app, submit, mode);
            app.queued_auto_drain_paused_after_withdrawal = true;
        }
        WithdrawDestination::Discard => {}
    }
    app.last_esc_press_ms = 0;
    app.is_loading = false;
    app.suppress_late_visible_updates_after_withdrawal = true;
    if ui_mode == UiMode::Inline {
        app.pending_inline_viewport_reset = true;
    }
    session
        .engine_half
        .runtime
        .file_history_tracker
        .clear_current_message_id();
    true
}

fn run_stop_hook(session: &TuiEngineSession, stop_reason: Option<String>) -> Result<(), String> {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return Ok(());
    };
    let verdict = handle.block_on(
        session
            .engine_half
            .runtime
            .policy
            .emit(HookEventPayload::Stop { stop_reason }),
    );
    // Gated: a terminal refusal refuses the stop, as a `BlockStop` does.
    match verdict.denial() {
        Some(reason) => Err(reason.to_string()),
        None => rebon_core::hooks::apply_stop_effects(verdict.effects()),
    }
}

fn stop_remote_session_tasks(app: &mut AppState, session: &TuiEngineSession) -> bool {
    let Some(remote) = session
        .remote_background_attachment
        .as_ref()
        .filter(|remote| remote.is_live())
    else {
        return false;
    };
    let task_ids = app
        .remote_background_tasks
        .values()
        .filter(|snapshot| matches!(snapshot.task.status.as_str(), "pending" | "running"))
        .map(|snapshot| snapshot.task.task_id.clone())
        .collect::<Vec<_>>();
    if task_ids.is_empty() {
        return false;
    }

    let task_count = task_ids.len();
    match crate::background::cancel_background_job_tasks(&remote.job_id, task_ids) {
        Ok(cancelled) => {
            if cancelled {
                super::inject_system_message(
                    app,
                    "notice",
                    &format!(
                        "Requested cancellation of {task_count} background task(s) in the attached background job"
                    ),
                );
            }
            cancelled
        }
        Err(err) => {
            super::inject_system_message(
                app,
                "error",
                &format!("Failed to cancel background tasks: {err}"),
            );
            true
        }
    }
}

fn stop_remaining_session_tasks(app: &mut AppState, session: &TuiEngineSession) -> bool {
    let remote_stopped = stop_remote_session_tasks(app, session);
    let stopped = stop_session_tasks(session.engine_half.tasks.as_ref());
    if stopped > 0 {
        super::inject_system_message(
            app,
            "notice",
            &format!("Stopped {stopped} running background task(s)"),
        );
    }
    remote_stopped || stopped > 0
}

pub(super) fn stop_session_tasks(tasks: &TaskRegistry) -> usize {
    use rebon_plugin_tasks::runtime::{stop_task, TaskStatus};

    let running_ids: Vec<_> = tasks
        .snapshots()
        .into_iter()
        .filter(|task| matches!(task.status, TaskStatus::Running | TaskStatus::Pending))
        .map(|task| task.id)
        .collect();

    let mut stopped = 0;
    for id in running_ids {
        if stop_task(tasks, &id).is_ok() {
            tasks.escalation_registry().cancel_agent(
                id.as_str(),
                "session task was stopped during interrupt or exit",
            );
            stopped += 1;
        }
    }

    stopped
}

/// Stop the foregrounded agent when it runs inside the attached remote
/// worker: the in-process registry has no entry for it, so the stop routes
/// over the background IPC channel instead.
fn interrupt_foregrounded_remote_task(app: &mut AppState, session: &TuiEngineSession) -> bool {
    let Some(remote) = session
        .remote_background_attachment
        .as_ref()
        .filter(|remote| remote.is_live())
    else {
        return false;
    };
    let Some(task_id) = app.foregrounded_task_id.clone() else {
        return false;
    };
    let Some(snapshot) = app.remote_background_tasks.get(&task_id) else {
        return false;
    };
    if !matches!(snapshot.task.status.as_str(), "pending" | "running") {
        return false;
    }
    match crate::background::cancel_background_job_tasks(&remote.job_id, vec![task_id]) {
        Ok(cancelled) => cancelled,
        Err(err) => {
            super::inject_system_message(
                app,
                "error",
                &format!("Failed to stop remote agent: {err}"),
            );
            true
        }
    }
}

fn interrupt_foregrounded_task(app: &mut AppState, tasks: &TaskRegistry) -> bool {
    use rebon_plugin_tasks::runtime::{stop_task, TaskId, TaskStatus};

    let Some(task_id) = app.foregrounded_task_id.clone() else {
        return false;
    };
    let id = TaskId::new(task_id.clone());
    let Some(snapshot) = tasks.snapshot(&id) else {
        return false;
    };
    if !matches!(snapshot.status, TaskStatus::Running | TaskStatus::Pending) {
        return false;
    }
    if stop_task(tasks, &id).is_err() {
        return false;
    }
    tasks
        .escalation_registry()
        .cancel_agent(&task_id, "foreground agent was interrupted");
    true
}

fn cancel_commit(app: &mut AppState, session_id: &str) {
    let uuid = format!(
        "a-cancel-{session_id}-{}",
        rebon_types::wall_clock_ms_u128()
    );
    let timestamp = format_system_time_iso_ms(SystemTime::now());
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Cancel {
            commit_uuid: uuid,
            commit_timestamp: timestamp,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use rebon_types::PromptPasteContent;
    use rebon_types::{
        ContentBlock, PromptCancel, SessionUpdate, SessionUpdateParams, TextContent,
    };
    use tokio::sync::oneshot;

    use crate::tui::app::AppState;
    use crate::tui::update::translate_session_update;
    use crate::ui_config::UiMode;

    use super::super::test_support::{
        insert_local_agent_task, insert_terminal_agent_notification, make_test_tui_session,
        push_test_user_message,
    };
    use super::super::{ActivePrompt, PromptResultRx, WithdrawDestination, WithdrawableSubmit};

    fn session_with_app_tasks(app: &AppState) -> TuiEngineSession {
        let mut session = make_test_tui_session();
        session.engine_half.tasks = app.tasks.clone();
        session
    }

    fn active_with_withdrawable(
        rx: PromptResultRx,
        cancel: PromptCancel,
        text: &str,
        before: usize,
        after: usize,
        uuid: &str,
    ) -> ActivePrompt {
        ActivePrompt::new(rx, cancel).with_withdrawable(WithdrawableSubmit {
            destination: WithdrawDestination::LiveInput {
                text: text.into(),
                cursor_offset: text.len(),
                image_pastes: Vec::new(),
            },
            transcript_len_before: before,
            transcript_len_after: after,
            user_message_uuid: Some(uuid.into()),
        })
    }

    #[test]
    fn apply_interrupt_withdraws_user_message_before_reply_and_restores_input() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u1", "hello");
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(active_with_withdrawable(
            rx,
            cancel.clone(),
            "hello",
            0,
            1,
            "u1",
        ));
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));

        assert!(cancel.is_cancelled());
        assert!(active.is_none());
        assert_eq!(app.rebon_tui.transcript.len(), 0);
        assert_eq!(app.input, "hello");
        assert_eq!(app.cursor_offset, "hello".len());
        assert!(app.suppress_late_visible_updates_after_withdrawal);
        assert!(app.rebon_tui.overlay.is_empty());
    }

    #[test]
    fn apply_interrupt_inline_withdrawal_requests_viewport_reset() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u1", "hello");
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(active_with_withdrawable(rx, cancel, "hello", 0, 1, "u1"));
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Inline
        ));

        assert!(app.pending_inline_viewport_reset);
        assert_eq!(app.rebon_tui.transcript.len(), 0);
        assert_eq!(app.input, "hello");
    }

    #[test]
    fn apply_interrupt_withdraws_user_message_and_restores_middle_cursor() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u1", "hello world");
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(ActivePrompt::new(rx, cancel.clone()).with_withdrawable(
            WithdrawableSubmit {
                destination: WithdrawDestination::LiveInput {
                    text: "hello world".into(),
                    cursor_offset: "hello".len(),
                    image_pastes: Vec::new(),
                },
                transcript_len_before: 0,
                transcript_len_after: 1,
                user_message_uuid: Some("u1".into()),
            },
        ));
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));

        assert!(cancel.is_cancelled());
        assert_eq!(app.input, "hello world");
        assert_eq!(app.cursor_offset, "hello".len());
    }

    #[test]
    fn apply_interrupt_snaps_restored_cursor_to_utf8_boundary() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u1", "你world");
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(ActivePrompt::new(rx, cancel).with_withdrawable(
            WithdrawableSubmit {
                destination: WithdrawDestination::LiveInput {
                    text: "你world".into(),
                    cursor_offset: 2,
                    image_pastes: Vec::new(),
                },
                transcript_len_before: 0,
                transcript_len_after: 1,
                user_message_uuid: Some("u1".into()),
            },
        ));
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert_eq!(app.input, "你world");
        assert_eq!(app.cursor_offset, 0);
    }

    #[test]
    fn withdrawn_prompt_suppresses_late_visible_assistant_chunk() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u1", "hello");
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(active_with_withdrawable(rx, cancel, "hello", 0, 1, "u1"));
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        translate_session_update(
            &mut app,
            SessionUpdateParams {
                session_id: "sess-1".into(),
                update: SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text(TextContent {
                        text: "late reply".into(),
                        annotations: None,
                    }),
                },
            },
        );

        assert!(app.rebon_tui.overlay.is_empty());
        assert_eq!(app.rebon_tui.transcript.len(), 0);
    }

    #[test]
    fn apply_interrupt_withdraws_restores_image_pastes_and_next_id() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u1", "see [Pasted image #7]");
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(ActivePrompt::new(rx, cancel.clone()).with_withdrawable(
            WithdrawableSubmit {
                destination: WithdrawDestination::LiveInput {
                    text: "see [Pasted image #7]".into(),
                    cursor_offset: "see [Pasted image #7]".len(),
                    image_pastes: vec![PromptPasteContent {
                        id: 7,
                        kind: "image".into(),
                        content: "abc".into(),
                        media_type: Some("image/png".into()),
                        filename: None,
                        source_path: None,
                    }],
                },
                transcript_len_before: 0,
                transcript_len_after: 1,
                user_message_uuid: Some("u1".into()),
            },
        ));
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));

        assert!(cancel.is_cancelled());
        assert_eq!(app.input, "see [Pasted image #7]");
        assert_eq!(app.pasted_contents.len(), 1);
        assert_eq!(app.pasted_contents[0].id, 7);
        assert_eq!(app.next_paste_id, 8);
    }

    #[test]
    fn apply_cancel_or_exit_withdraws_before_reply_and_does_not_exit() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u1", "hello");
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(active_with_withdrawable(
            rx,
            cancel.clone(),
            "hello",
            0,
            1,
            "u1",
        ));
        let session = session_with_app_tasks(&app);

        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));

        assert!(cancel.is_cancelled());
        assert!(active.is_none());
        assert_eq!(app.rebon_tui.transcript.len(), 0);
        assert_eq!(app.input, "hello");
    }

    #[test]
    fn apply_interrupt_after_visible_reply_keeps_user_row_and_cancels() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u1", "hello");
        app.rebon_tui.overlay.set_streaming_text("partial");
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut prompt = active_with_withdrawable(rx, cancel.clone(), "hello", 0, 1, "u1");
        prompt.reply_started = true;
        let mut active = Some(prompt);
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));

        assert!(cancel.is_cancelled());
        assert!(active.is_none());
        assert_eq!(app.input, "");
        assert_eq!(app.rebon_tui.transcript.len(), 2);
        assert_eq!(app.rebon_tui.transcript.rows()[0].uuid(), Some("u1"));
        assert!(app.rebon_tui.overlay.is_empty());
    }

    #[test]
    fn apply_interrupt_cancels_inflight_prompt_before_exit() {
        let mut app = AppState::new();
        app.rebon_tui.overlay.set_streaming_text("partial");
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(ActivePrompt::new(rx, cancel.clone()));
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(cancel.is_cancelled());
        assert!(active.is_none());
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        assert!(app.rebon_tui.overlay.is_empty());
    }

    #[test]
    fn apply_interrupt_clears_inline_draft_when_cancelling_inflight_prompt() {
        let mut app = AppState::new();
        app.input = "draft".into();
        app.cursor_offset = 5;
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(ActivePrompt::new(rx, cancel.clone()));
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Inline
        ));
        assert!(cancel.is_cancelled());
        assert!(active.is_none());
        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
    }

    #[test]
    fn apply_interrupt_keeps_screen_draft_when_cancelling_inflight_prompt() {
        let mut app = AppState::new();
        app.input = "draft".into();
        app.cursor_offset = 5;
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(ActivePrompt::new(rx, cancel.clone()));
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(cancel.is_cancelled());
        assert!(active.is_none());
        assert_eq!(app.input, "draft");
        assert_eq!(app.cursor_offset, 5);
    }

    #[test]
    fn cancelling_task_notification_prompt_leaves_notifications_retryable() {
        let mut app = AppState::new();
        insert_terminal_agent_notification(&mut app, "agent-cancel", "/tmp/agent-cancel.report.md");
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut session = session_with_app_tasks(&app);
        session.engine_half.task_notification_poller =
            crate::task_notification_poller::TaskNotificationPoller::new(
                app.tasks.as_ref().clone(),
            );
        let task_id = rebon_plugin_tasks::runtime::TaskId::new("agent-cancel");
        let turn_id = format!("{}:notification-cancel", session.session_id);
        let claimed_ids = session
            .engine_half
            .task_notification_poller
            .reserve_task_ids(
                &session.session_id,
                &turn_id,
                std::slice::from_ref(&task_id),
            );
        assert_eq!(claimed_ids, vec![task_id.clone()]);
        let mut active = Some(
            ActivePrompt::with_task_notifications(rx, cancel.clone(), vec![task_id], Vec::new())
                .with_task_notification_claim(
                    session.engine_half.task_notification_poller.clone(),
                    turn_id,
                ),
        );
        assert!(session
            .engine_half
            .task_notification_poller
            .unnotified_notifications_for_session(&session.session_id)
            .is_empty());

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));

        assert!(cancel.is_cancelled());
        assert!(active.is_none());
        assert!(
            !app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-cancel"))
                .expect("task snapshot")
                .notified
        );
        assert_eq!(app.tasks.unnotified_terminal_agent_notifications().len(), 1);
        assert_eq!(
            session
                .engine_half
                .task_notification_poller
                .unnotified_notifications_for_session(&session.session_id)
                .len(),
            1
        );
    }

    #[test]
    fn apply_interrupt_leaves_running_background_tasks_untouched() {
        let mut app = AppState::new();
        let registry = rebon_plugin_tasks::runtime::TaskRegistry::new();
        let cancel = insert_local_agent_task(
            &registry,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(registry);
        let mut active = None;
        let session = session_with_app_tasks(&app);

        assert!(!apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(!cancel.is_cancelled());
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"))
                .expect("task snapshot")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Running
        );
        assert!(!app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(system)
                if system.content.as_deref() == Some("Stopped 1 running background task(s)")
        )));
    }

    #[test]
    fn apply_cancel_or_exit_stops_running_background_tasks() {
        let mut app = AppState::new();
        let registry = rebon_plugin_tasks::runtime::TaskRegistry::new();
        let cancel = insert_local_agent_task(
            &registry,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(registry);
        let mut active = None;
        let session = session_with_app_tasks(&app);

        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(cancel.is_cancelled());
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"))
                .expect("task snapshot")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Killed
        );
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(system)
                if system.content.as_deref() == Some("Stopped 1 running background task(s)")
        )));
    }

    #[test]
    fn ctrl_c_cancels_active_prompt_and_background_tasks_together() {
        let mut app = AppState::new();
        let registry = rebon_plugin_tasks::runtime::TaskRegistry::new();
        let background_cancel = insert_local_agent_task(
            &registry,
            "agent-background",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(registry);
        let (_tx, rx) = oneshot::channel();
        let prompt_cancel = PromptCancel::new();
        let mut active = Some(ActivePrompt::new(rx, prompt_cancel.clone()));
        let session = session_with_app_tasks(&app);

        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(prompt_cancel.is_cancelled());
        assert!(background_cancel.is_cancelled());
        assert!(active.is_none());
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new(
                    "agent-background"
                ))
                .expect("background task")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Killed
        );
    }

    #[test]
    fn ctrl_c_stops_stuck_task_then_allows_session_exit() {
        // In the Agent View, the first Ctrl+C stops ONLY the selected (stuck)
        // task and arms the exit; a sibling running task must survive, and a
        // second press within the window exits.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-stuck",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        insert_local_agent_task(
            &reg,
            "agent-other",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(reg);
        let mut view = crate::tui::agent_view::AgentViewState::open(
            &store,
            store.list_jobs().unwrap(),
            &app.task_snapshots(),
        );
        assert!(view.select_task_row_for_test("agent-stuck"));
        app.agent_view = Some(view);
        let mut active = None;
        let session = session_with_app_tasks(&app);

        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-stuck"))
                .expect("task snapshot")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Killed
        );
        // The non-selected sibling task is left untouched.
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-other"))
                .expect("sibling task snapshot")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Running
        );
        // Second Ctrl+C within the 2s window exits.
        assert!(apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
    }

    #[test]
    fn agent_view_ctrl_c_stops_session_task_and_leaves_supervisor_job_running() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "keep running".into(),
                std::path::PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        job.process.status = crate::background::BackgroundJobStatus::Running;
        store.write_state(&job).unwrap();

        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-mounted",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        insert_local_agent_task(
            &reg,
            "agent-sibling",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(reg);
        let mut view = crate::tui::agent_view::AgentViewState::open(
            &store,
            store.list_jobs().unwrap(),
            &app.task_snapshots(),
        );
        assert!(view.select_task_row_for_test("agent-mounted"));
        app.agent_view = Some(view);
        let mut active = None;
        let session = session_with_app_tasks(&app);

        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen,
        ));
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-mounted"))
                .expect("task snapshot")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Killed
        );
        // Only the selected task is stopped: the sibling task and the
        // supervisor-backed job both keep running.
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-sibling"))
                .expect("sibling task snapshot")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Running
        );
        assert_eq!(
            store
                .read_state(&job.identity.job_id)
                .unwrap()
                .process
                .status,
            crate::background::BackgroundJobStatus::Running
        );
        assert!(apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen,
        ));
    }

    #[test]
    fn agent_view_ctrl_c_with_no_running_task_selected_leaves_siblings_running() {
        // The narrowing guarantee the old `stop_session_tasks` broke: when the
        // highlighted row is not a running task, a running SIBLING task must not
        // be stopped by the first Ctrl+C.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-done",
            rebon_plugin_tasks::runtime::TaskStatus::Completed,
            true,
        );
        insert_local_agent_task(
            &reg,
            "agent-busy",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(reg);
        let mut view = crate::tui::agent_view::AgentViewState::open(
            &store,
            store.list_jobs().unwrap(),
            &app.task_snapshots(),
        );
        assert!(view.select_task_row_for_test("agent-done"));
        app.agent_view = Some(view);
        let mut active = None;
        let session = session_with_app_tasks(&app);

        // First press arms the exit but must stop nothing.
        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-busy"))
                .expect("running sibling snapshot")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Running
        );
        // Second press within the window exits.
        assert!(apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
    }

    #[test]
    fn agent_view_ctrl_c_with_no_task_selected_cancels_main_prompt_but_no_tasks() {
        // With no running task under the cursor, the first Ctrl+C falls back to
        // cancelling the main in-flight turn (point 2), while still leaving
        // sibling session tasks untouched.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-done",
            rebon_plugin_tasks::runtime::TaskStatus::Completed,
            true,
        );
        insert_local_agent_task(
            &reg,
            "agent-busy",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(reg);
        let mut view = crate::tui::agent_view::AgentViewState::open(
            &store,
            store.list_jobs().unwrap(),
            &app.task_snapshots(),
        );
        assert!(view.select_task_row_for_test("agent-done"));
        app.agent_view = Some(view);
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(ActivePrompt::new(rx, cancel.clone()));
        let session = session_with_app_tasks(&app);

        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        // The main in-flight turn is cancelled...
        assert!(cancel.is_cancelled());
        assert!(active.is_none());
        // ...but the running sibling task is left alone.
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-busy"))
                .expect("running sibling snapshot")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Running
        );
    }

    #[test]
    fn apply_interrupt_cancels_only_foreground_agent_when_viewing_live_agent() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        let foreground_cancel = insert_local_agent_task(
            &reg,
            "agent-foreground",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let background_cancel = insert_local_agent_task(
            &reg,
            "agent-background",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(reg);
        app.foregrounded_task_id = Some("agent-foreground".into());
        let mut active = None;
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));

        assert!(foreground_cancel.is_cancelled());
        assert!(!background_cancel.is_cancelled());
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new(
                    "agent-foreground"
                ))
                .expect("foreground task")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Killed
        );
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new(
                    "agent-background"
                ))
                .expect("background task")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Running
        );
    }

    #[test]
    fn apply_interrupt_clears_input_on_second_esc_when_idle() {
        let mut app = AppState::new();
        app.input = "draft".into();
        app.cursor_offset = 5;
        let mut active = None;
        let session = session_with_app_tasks(&app);

        assert!(!apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert_eq!(app.input, "draft");
        assert_eq!(app.cursor_offset, 5);
        assert!(app.last_esc_press_ms > 0);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
        assert_eq!(app.last_esc_press_ms, 0);
    }

    #[test]
    fn apply_interrupt_is_noop_when_idle_and_empty() {
        let mut app = AppState::new();
        let mut active = None;
        let session = session_with_app_tasks(&app);
        assert!(!apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
    }

    #[test]
    fn apply_interrupt_flush_skips_whitespace_input() {
        use rebon_tui::promptinput::{QueuedCommand, QueuedCommandValue};

        let mut app = AppState::new();
        app.mode = "prompt".into();
        app.input = "  \n  ".into();
        app.queued_commands = vec![QueuedCommand {
            mode: "prompt".into(),
            value: QueuedCommandValue::Text("queued".into()),
        }];
        let mut active = None;
        let session = session_with_app_tasks(&app);

        assert!(apply_interrupt(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(app.input.is_empty());
        // Whitespace input NOT enqueued
        assert_eq!(app.queued_commands.len(), 1);
    }

    #[test]
    fn apply_cancel_or_exit_clears_input_on_single_press_when_idle() {
        let mut app = AppState::new();
        app.input = "some draft text".into();
        let mut active = None;
        let session = session_with_app_tasks(&app);

        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(app.input.is_empty());
        assert_eq!(app.last_ctrl_c_exit_press_ms, 0);
    }

    #[test]
    fn apply_cancel_or_exit_requires_second_press_when_idle_and_empty() {
        let mut app = AppState::new();
        let mut active = None;
        let session = session_with_app_tasks(&app);

        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(app.last_ctrl_c_exit_press_ms > 0);

        assert!(apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert_eq!(app.last_ctrl_c_exit_press_ms, 0);
    }

    #[test]
    fn ctrl_c_second_press_exits_after_cancelling_inflight_prompt() {
        let mut app = AppState::new();
        app.rebon_tui.overlay.set_streaming_text("partial");
        let (_tx, rx) = oneshot::channel();
        let cancel = PromptCancel::new();
        let mut active = Some(ActivePrompt::new(rx, cancel.clone()));
        let session = session_with_app_tasks(&app);

        // First press cancels the in-flight prompt and ARMS the two-press
        // exit rather than clearing it.
        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(cancel.is_cancelled());
        assert!(active.is_none());
        assert!(app.last_ctrl_c_exit_press_ms > 0);

        // Second press within the window exits.
        assert!(apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert_eq!(app.last_ctrl_c_exit_press_ms, 0);
    }

    /// A parked session has no worker to cancel anything in: the first
    /// Ctrl+C arms the exit without complaining about a cancel it could
    /// not send, and the second exits.
    #[test]
    fn ctrl_c_on_a_parked_session_arms_the_exit_without_a_worker_to_cancel() {
        let mut app = AppState::new();
        let mut session = session_with_app_tasks(&app);
        session.attached_background_job_id = Some("job-parked-for-ctrl-c-test".into());
        session.remote_background_attachment = Some(
            crate::background::RemoteBackgroundAttachment::without_worker(
                "job-parked-for-ctrl-c-test".into(),
                "sess-parked".into(),
                ".".into(),
                crate::background::BackgroundJobStatus::Stopped,
                0,
            ),
        );
        let mut active = None;

        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(app.last_ctrl_c_exit_press_ms > 0);
        assert!(
            app.rebon_tui.transcript.is_empty(),
            "nothing to cancel, nothing to complain about"
        );
        assert!(apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
    }

    #[test]
    fn ctrl_c_second_press_exits_when_remote_background_cancel_never_settles() {
        // Regression: with a remote attachment whose task snapshots never
        // record the kill (dead worker, task-bridge gap), every Ctrl+C press
        // "cancelled" again (or errored) and cleared the exit window, so the
        // session could never be exited from the keyboard.
        let mut app = AppState::new();
        app.remote_background_tasks.insert(
            "agent-stale".into(),
            rebon_session_host::BackgroundTaskSnapshot {
                task: rebon_session_host::BackgroundTaskDescriptor {
                    task_id: "agent-stale".into(),
                    title: "stale remote agent".into(),
                    kind: "local_agent".into(),
                    status: "running".into(),
                    is_backgrounded: true,
                    start_time_ms: 1,
                    end_time_ms: None,
                    last_progress: None,
                    error: None,
                    prompt: None,
                    parent_tool_call_id: None,
                    agent_id: Some("agent-stale".into()),
                    agent_name: None,
                    agent_type: None,
                    model: None,
                    token_count: None,
                    tool_use_count: None,
                    result: None,
                },
                updated_at_ms: 1,
                log_preview: Vec::new(),
                transcript: Vec::new(),
            },
        );
        let mut session = session_with_app_tasks(&app);
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                "job-missing-for-ctrl-c-exit-test".into(),
                "sess-remote".into(),
                ".".into(),
                crate::background::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: 0,
                    port: 1,
                    token: "tok".into(),
                },
            ));
        let mut active = None;

        // First press hits the remote cancel path (which here errors against
        // a job that does not exist) — it must still arm the exit window.
        assert!(!apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
        assert!(app.last_ctrl_c_exit_press_ms > 0);

        // The stale snapshot still claims a running task, so the cancel path
        // would fire again — the second press must exit anyway.
        assert!(apply_cancel_or_exit(
            &mut app,
            &session,
            &mut active,
            UiMode::Screen
        ));
    }
}
