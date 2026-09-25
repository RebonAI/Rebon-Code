//! Apply [`rebon_tui::promptinput`] plan results to [`AppState`].
//!
//! The event loop in [`crate::tui::runner`] produces three kinds
//! of work on each keystroke:
//!
//! 1. A pure gesture decision from [`plan_input_event`]
//!    (`InputEventPlan`) — mostly mode-reset / help-close side
//!    effects and a small `InputEventPrimaryAction` enum.
//! 2. An optional buffer decision from [`plan_input_change`]
//!    (`InputChangePlan`) — either a `ToggleHelp` short-circuit or
//!    a `NormalizedInputChange` that tells us whether to replace
//!    the buffer and where to put the cursor.
//! 3. Local cursor / exit gestures that the pure planners do not
//!    own because they depend on render-surface grapheme state
//!    (Left / Right / Home / End) or on terminal lifecycle
//!    (Ctrl-C / Esc).
//!
//! This module owns the translation of those results into mutations
//! of [`AppState`]. Keeping it in its own module means the runner
//! stays a thin "poll → translate → dispatch → render" loop, and
//! the mutation logic is covered by unit tests that don't need
//! crossterm or ratatui initialised.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rebon_tui::promptinput::prompt_surface::parse_references;
use rebon_tui::promptinput::{
    clamp_cursor_offset, get_mode_from_input, get_value_from_input, process_queued_commands,
    InputChangePlan, InputEventPlan, InputEventPrimaryAction, NormalizedInputChange,
    QueuedCommandValue,
};
use rebon_tui::{
    reducer, Action, AttachmentRow, Message, UserContentBlock, UserImageBlock, UserMessage,
    UserMessageInner, UserRole, UserTextBlock,
};
use rebon_types::format_system_time_iso_ms;
use rebon_types::FileMentionQuery;
use rebon_types::PromptPasteContent;

use crate::file_scanner::canonical_file_mention_location;
use crate::session::input_history::HistoryEntry;
use crate::session::submit_payload::{
    ensure_internal_submit_payload_user_uuid, ensure_submit_payload_user_uuid, DirectoryAttachment,
    SubmitPayload,
};
use crate::tui::app::AppState;
use crate::tui::event::TextEdit;
use rebon_tui::promptinput::QueuedCommand;

/// Apply an [`InputEventPlan`] (no text-edit side effects) to
/// [`AppState`].
pub fn apply_event_plan(app: &mut AppState, plan: &InputEventPlan) {
    // option_meta_hint → status-bar toast for macOS Option key.
    if let Some(hint) = &plan.option_meta_hint {
        app.option_meta_hint_toast = Some(hint.clone());
    }

    // reset_prompt_mode → reset to "prompt" (backspace/esc/delete
    // at cursor position 0).
    if plan.reset_prompt_mode {
        app.mode = String::from("prompt");
    }

    // close_help → close the help overlay.
    if plan.close_help {
        app.help_open = false;
    }

    match plan.primary_action {
        InputEventPrimaryAction::None => {}
        InputEventPrimaryAction::TypeToExitFooter {
            ref next_input,
            next_cursor_offset,
        } => {
            // typing a printable char while a footer pill is
            // selected inserts the char into the buffer and clears
            // the footer selection.
            app.input = next_input.clone();
            app.cursor_offset = clamp_cursor_offset(&app.input, next_cursor_offset);
            app.slash_picker = None;
            app.at_mention_picker = None;
        }
        InputEventPrimaryAction::AbortSpeculation => {
            // abort active speculation — reset to idle state.
            app.speculation_active = false;
        }
        InputEventPrimaryAction::DismissSideQuestion => {
            // dismiss the "/btw" side-question modal.
            app.side_question_visible = false;
        }
        InputEventPrimaryAction::DeferToFooterSelection => {
            // Esc while a footer pill is selected clears the
            // selection.
            app.slash_picker = None;
            app.at_mention_picker = None;
        }
        InputEventPrimaryAction::PopQueuedCommand => {
            pop_queued_command_into_input(app);
        }
        InputEventPrimaryAction::FlushQueue => {
            flush_queue(app);
        }
        InputEventPrimaryAction::TriggerDoublePressEscFromEmpty => {
            // double-press Esc from empty prompt opens the
            // message selector. Track timestamp; the runner checks
            // the window (400ms) and opens the selector on the
            // second press.
            let now_ms = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            app.last_esc_press_ms = now_ms;
        }
    }
}

/// Apply the text-edit pair (event plan + change plan + intended
/// cursor) produced by [`crate::tui::event::translate_key`] to
/// [`AppState`].
///
/// Returns `true` if the buffer or cursor changed, so the runner
/// can optionally track a "dirty" flag in the future. No caller
/// reads it today — the next frame always re-renders.
pub fn apply_text_edit(
    app: &mut AppState,
    edit: &TextEdit,
    event_plan: &InputEventPlan,
    change_plan: &InputChangePlan,
) -> bool {
    // Gesture side effects from the same keystroke run first so
    // any mode/help resets apply before the buffer mutation.
    apply_event_plan(app, event_plan);

    match change_plan {
        InputChangePlan::ToggleHelp => {
            // Help toggles are explicit gestures, not text edits, so do not
            // run the normal input-change path.
            app.help_open = !app.help_open;
            true
        }
        InputChangePlan::Apply(normalized) => apply_normalized_change(app, edit, normalized),
    }
}

/// Apply the `Apply` branch of an [`InputChangePlan`] to [`AppState`].
fn apply_normalized_change(
    app: &mut AppState,
    edit: &TextEdit,
    plan: &NormalizedInputChange,
) -> bool {
    let mut changed = false;

    // 1. Replace buffer if the planner requested it. When
    //    `replace_input_with` is None the planner is signalling
    //    "no buffer mutation this tick" — e.g. the single-char
    //    mode prefix branch that only changes mode. In that case
    //    the buffer stays at its current value and the cursor
    //    stays where it was.
    if let Some(new_input) = &plan.replace_input_with {
        if app.input != *new_input {
            app.input = new_input.clone();
            changed = true;
        }

        // 2. Apply the cursor. Priority:
        //    a. Explicit `next_cursor_offset` from the plan (mode
        //       stripping, tab expansion, etc.).
        //    b. `edit.intended_cursor` from the translate layer
        //       (normal insert / backspace / delete).
        //    c. Clamp to end of the new buffer.
        let raw = plan.next_cursor_offset.unwrap_or(edit.intended_cursor);
        let clamped = clamp_cursor_offset(&app.input, raw);
        if app.cursor_offset != clamped {
            app.cursor_offset = clamped;
            changed = true;
        }
    }

    // 3. Mode change. `HistoryMode` is an enum from rebon_tui::promptinput;
    //    its string form is what `PromptInputRuntimeInput::mode`
    //    expects. We use Debug for now because `Display` is not
    //    implemented on `HistoryMode`; wiring real mode navigation
    //    can swap this for a dedicated helper.
    if let Some(mode) = plan.next_mode {
        let new_mode = format!("{mode:?}").to_lowercase();
        if app.mode != new_mode {
            app.mode = new_mode;
            changed = true;
        }
    }

    if plan.push_previous_to_buffer {
        app.undo_stack.push((
            edit.change_input.current_input.clone(),
            edit.change_input.cursor_offset,
        ));
    }

    // Clear pickers when the planner signals `clear_footer_selection`.
    if plan.clear_footer_selection {
        app.slash_picker = None;
        app.at_mention_picker = None;
    }

    // NormalizedInputChange side effects from the input-change path.
    if plan.close_help {
        app.help_open = false;
    }
    if plan.dismiss_stash_hint {
        app.stash_hint_dismissed = true;
    }
    if plan.abort_prompt_suggestion {
        app.prompt_suggestion_active = false;
    }
    if plan.abort_speculation {
        app.speculation_active = false;
    }

    changed
}

/// Restore the most recent input snapshot from the undo stack.
pub fn pop_undo(app: &mut AppState) -> bool {
    let Some((input, cursor_offset)) = app.undo_stack.pop() else {
        return false;
    };
    let clamped = clamp_cursor_offset(&input, cursor_offset);
    let changed = app.input != input || app.cursor_offset != clamped;
    app.input = input;
    app.cursor_offset = clamped;
    changed
}

/// Queue one prompt while another turn is in flight.
#[cfg(test)]
pub fn enqueue_submit(app: &mut AppState, text: String) {
    enqueue_submit_payload(
        app,
        SubmitPayload {
            text,
            model_text: None,
            user_message_uuid: None,
            image_pastes: Vec::new(),
            directory_attachments: Vec::new(),
            execution_policy: None,
            skill_invocations: Vec::new(),
        },
    );
}

pub fn enqueue_submit_payload(app: &mut AppState, submit: SubmitPayload) {
    let mode = app.mode.clone();
    enqueue_submit_payload_in_mode(app, submit, mode);
}

/// Queue `submit` under an explicit input mode, for a message nobody typed
/// at the prompt — the prompt's current mode says nothing about it.
pub fn enqueue_submit_payload_in_mode(app: &mut AppState, submit: SubmitPayload, mode: String) {
    if app.queued_commands.is_empty() && app.queued_submit_payloads.is_empty() {
        app.queued_auto_drain_paused_after_withdrawal = false;
    }
    app.queued_commands.push(QueuedCommand {
        mode,
        value: QueuedCommandValue::Text(submit.text.clone()),
    });
    let mut submit = submit;
    if app.is_loading {
        if let Some(poller) = app.mid_turn_queued_submit_poller.as_ref() {
            let _ = poller.enqueue_submit(&mut submit);
        }
    }
    app.queued_submit_payloads.push(submit);
    // Increment hint counter so "Press up to edit queued messages"
    // is shown only the first few times.
    app.queued_command_up_hint_count = app.queued_command_up_hint_count.saturating_add(1);
}

pub fn requeue_submit_payload_front(app: &mut AppState, submit: SubmitPayload, mode: String) {
    app.queued_commands.insert(
        0,
        QueuedCommand {
            mode,
            value: QueuedCommandValue::Text(submit.text.clone()),
        },
    );
    app.queued_submit_payloads.insert(0, submit);
    app.queued_command_up_hint_count = app.queued_command_up_hint_count.saturating_add(1);
}

pub fn take_submit_payload(app: &mut AppState, raw_text: &str) -> Option<SubmitPayload> {
    take_submit_payload_with_images(app, raw_text, Vec::new())
}

pub fn take_submit_payload_with_images(
    app: &mut AppState,
    raw_text: &str,
    images: Vec<PromptPasteContent>,
) -> Option<SubmitPayload> {
    let raw_text = text_with_mode_prefix(raw_text, &app.mode);
    let mut submit =
        build_submit_payload(&raw_text, &app.pasted_contents, &app.cwd, &app.file_index)?;
    submit.image_pastes.extend(images);
    clear_input(app);
    app.mode = String::from("prompt");
    app.pasted_contents.clear();
    app.next_paste_id = 1;
    Some(submit)
}

fn text_with_mode_prefix(raw_text: &str, mode: &str) -> String {
    if mode == "bash" && !raw_text.trim().is_empty() && !raw_text.starts_with('!') {
        format!("!{raw_text}")
    } else {
        raw_text.to_string()
    }
}

fn build_submit_payload(
    raw_text: &str,
    pasted_contents: &[PromptPasteContent],
    cwd: &str,
    _file_index: &crate::file_scanner::FileIndex,
) -> Option<SubmitPayload> {
    let trimmed = raw_text.trim_end().to_string();

    let referenced_ids: std::collections::HashSet<u32> = parse_references(&trimmed)
        .into_iter()
        .map(|reference| reference.id)
        .collect();
    // If the user submits with no text at all, treat every pasted
    // image as implicitly attached. Otherwise require a chip
    // reference so deleting the chip drops the image.
    let text_is_empty = trimmed.is_empty();
    let active_pasted_contents: Vec<PromptPasteContent> = pasted_contents
        .iter()
        .filter(|content| {
            content.kind != "image" || text_is_empty || referenced_ids.contains(&content.id)
        })
        .cloned()
        .collect();

    let expanded = expand_paste_references(&trimmed, &active_pasted_contents);
    let directory_attachments = collect_directory_attachments(&expanded, cwd);
    let model_text = if directory_attachments.is_empty() {
        None
    } else {
        Some(expand_directory_mentions_for_model(
            &expanded,
            &directory_attachments,
        ))
    };
    let image_pastes: Vec<PromptPasteContent> = active_pasted_contents
        .iter()
        .filter(|content| content.kind == "image")
        .cloned()
        .collect();

    if expanded.is_empty() && image_pastes.is_empty() && directory_attachments.is_empty() {
        return None;
    }

    Some(SubmitPayload {
        text: expanded,
        model_text,
        user_message_uuid: None,
        image_pastes,
        directory_attachments,
        execution_policy: None,
        skill_invocations: Vec::new(),
    })
}

/// Pop the next queued text prompt in FIFO order.
pub fn pop_next_queued_submit_with_mode(app: &mut AppState) -> Option<(SubmitPayload, String)> {
    if app.queued_commands.is_empty() {
        app.queued_auto_drain_paused_after_withdrawal = false;
        return None;
    }
    let cmd = app.queued_commands.remove(0);
    let mode = cmd.mode;
    match cmd.value {
        QueuedCommandValue::Text(text) => {
            let submit = if app.queued_submit_payloads.is_empty() {
                SubmitPayload {
                    text,
                    model_text: None,
                    user_message_uuid: None,
                    image_pastes: Vec::new(),
                    directory_attachments: Vec::new(),
                    execution_policy: None,
                    skill_invocations: Vec::new(),
                }
            } else {
                app.queued_submit_payloads.remove(0)
            };
            if let Some(poller) = app.mid_turn_queued_submit_poller.as_ref() {
                poller.remove_submit(&submit);
            }
            if app.queued_commands.is_empty() && app.queued_submit_payloads.is_empty() {
                app.queued_auto_drain_paused_after_withdrawal = false;
            }
            Some((submit, mode))
        }
        QueuedCommandValue::NonText => {
            if app.queued_commands.is_empty() && app.queued_submit_payloads.is_empty() {
                app.queued_auto_drain_paused_after_withdrawal = false;
            }
            None
        }
    }
}

#[cfg(test)]
/// Pop the next queued text prompt in FIFO order.
pub fn pop_next_queued_submit(app: &mut AppState) -> Option<SubmitPayload> {
    pop_next_queued_submit_with_mode(app).map(|(submit, _mode)| submit)
}

/// Clear the live prompt buffer without touching transcript state.
pub fn clear_input(app: &mut AppState) {
    app.input.clear();
    app.cursor_offset = 0;
}

/// Whether the queue contains at least one editable (text-mode) command.
pub fn has_editable_queued_commands(app: &AppState) -> bool {
    app.queued_commands
        .iter()
        .any(|cmd| matches!(cmd.value, QueuedCommandValue::Text(_)))
}

/// Pop the last editable queued command into the prompt buffer for
/// editing. Called on Up-arrow when the cursor is on the first line
/// and the queue has editable items.
pub fn pop_queued_command_into_input(app: &mut AppState) {
    let pos = app
        .queued_commands
        .iter()
        .rposition(|cmd| matches!(cmd.value, QueuedCommandValue::Text(_)));
    let Some(pos) = pos else { return };
    let payload_idx = app.queued_commands[..pos]
        .iter()
        .filter(|cmd| matches!(cmd.value, QueuedCommandValue::Text(_)))
        .count();
    let cmd = app.queued_commands.remove(pos);
    if let QueuedCommandValue::Text(text) = cmd.value {
        let submit = if payload_idx < app.queued_submit_payloads.len() {
            app.queued_submit_payloads.remove(payload_idx)
        } else {
            SubmitPayload {
                text,
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            }
        };
        let mut restore_submit = true;
        if let Some(poller) = app.mid_turn_queued_submit_poller.as_ref() {
            restore_submit = !matches!(
                poller.withdraw_submit(&submit),
                crate::session::mid_turn_queue::QueuedSubmitWithdrawal::AlreadyConsumed
            );
        }
        if !restore_submit {
            if app.queued_commands.is_empty() && app.queued_submit_payloads.is_empty() {
                app.queued_auto_drain_paused_after_withdrawal = false;
            }
            return;
        }
        app.input = submit.text;
        let mode = get_mode_from_input(&app.input);
        app.mode = format!("{mode:?}").to_lowercase();
        if app.mode == "bash" {
            app.input = get_value_from_input(&app.input);
        }
        app.cursor_offset = app.input.len();
        app.pasted_contents = submit.image_pastes;
        app.next_paste_id = next_paste_id_after_restore(&app.pasted_contents);
        app.queued_auto_drain_paused_after_withdrawal = false;
    }
}

/// Enqueue the current input (if non-empty) and clear the buffer,
/// letting the queue auto-drain via [`maybe_spawn_next_queued_prompt`].
///
/// Called when Esc is pressed while the queue has content — the user
/// is confirming "send these queued messages" rather than popping
/// them for editing.
pub fn flush_queue(app: &mut AppState) {
    let snapshot = app.input.clone();
    if let Some(submit) = take_submit_payload(app, &snapshot) {
        enqueue_submit_payload(app, submit);
    } else {
        clear_input(app);
        app.pasted_contents.clear();
        app.next_paste_id = 1;
    }
}

pub fn next_paste_id_after_restore(pasted_contents: &[PromptPasteContent]) -> u32 {
    pasted_contents
        .iter()
        .map(|content| content.id)
        .max()
        .map(|id| id.saturating_add(1))
        .unwrap_or(1)
}

/// Return the visible queue length after promptinput-style filtering.
pub fn visible_queue_len(app: &AppState) -> usize {
    process_queued_commands(&app.queued_commands).len()
}

/// Whether the queue overflow summary row should be visible.
#[cfg(test)]
pub fn queue_has_overflow(app: &AppState) -> bool {
    app.queued_commands.len() > rebon_tui::promptinput::MAX_VISIBLE_NOTIFICATIONS
}

/// Build the synthetic overflow row text for the current queue size.
#[cfg(test)]
pub fn queue_overflow_message(app: &AppState) -> Option<String> {
    if !queue_has_overflow(app) {
        return None;
    }
    Some(
        rebon_tui::promptinput::create_overflow_notification_message(
            app.queued_commands.len() - (rebon_tui::promptinput::MAX_VISIBLE_NOTIFICATIONS - 1),
        ),
    )
}

/// Whether a queued command value is a plain text prompt.
#[cfg(test)]
pub fn queued_text(cmd: &QueuedCommand) -> Option<&str> {
    match &cmd.value {
        QueuedCommandValue::Text(text) => Some(text),
        QueuedCommandValue::NonText => None,
    }
}

/// Navigate up in input history. Called when Up is pressed on the
/// first line. Saves the current draft on the first press, then
/// replaces the input with progressively older history entries.
pub fn history_up(app: &mut AppState) {
    if app.history.is_empty() || app.history_index >= app.history.len() {
        return;
    }
    // Save draft on first press into history.
    if app.history_index == 0 {
        app.saved_draft = if app.input.trim().is_empty() {
            None
        } else {
            Some(app.input.clone())
        };
        app.saved_draft_pasted_contents = if app.pasted_contents.is_empty() {
            None
        } else {
            Some(app.pasted_contents.clone())
        };
    }
    app.history_index += 1;
    let entry = &app.history[app.history.len() - app.history_index];
    app.input = entry.display.clone();
    app.cursor_offset = 0;
    app.pasted_contents = entry.pasted_contents.clone();
    app.next_paste_id = next_paste_id_after_restore(&app.pasted_contents);
}

/// Navigate down in input history. Called when Down is pressed on the
/// last line. When reaching the bottom, restores the saved draft.
///
/// Returns `true` if navigation actually moved, including the
/// exit-history-to-draft transition consumed by
/// [`rebon_tui::promptinput::enter_footer_from_history`] to transition
/// focus into the footer pill row.
pub fn history_down(app: &mut AppState) -> bool {
    if app.history_index == 0 {
        return false;
    }
    app.history_index -= 1;
    if app.history_index == 0 {
        app.input = app.saved_draft.take().unwrap_or_default();
        app.cursor_offset = app.input.len();
        app.pasted_contents = app.saved_draft_pasted_contents.take().unwrap_or_default();
        app.next_paste_id = next_paste_id_after_restore(&app.pasted_contents);
    } else {
        let entry = &app.history[app.history.len() - app.history_index];
        app.input = entry.display.clone();
        app.cursor_offset = app.input.len();
        app.pasted_contents = entry.pasted_contents.clone();
        app.next_paste_id = next_paste_id_after_restore(&app.pasted_contents);
    }
    true
}

/// Save a submitted prompt to in-memory history and reset navigation.
pub fn save_to_history(app: &mut AppState, text: &str) -> Option<HistoryEntry> {
    let saved = if !text.trim().is_empty() {
        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let entry = HistoryEntry {
            display: text.to_string(),
            pasted_contents: referenced_pastes(text, &app.pasted_contents),
            timestamp: now_ms,
        };
        app.history.push(entry.clone());
        Some(entry)
    } else {
        None
    };
    app.history_index = 0;
    app.saved_draft = None;
    app.saved_draft_pasted_contents = None;
    saved
}

fn referenced_pastes(
    text: &str,
    pasted_contents: &[PromptPasteContent],
) -> Vec<PromptPasteContent> {
    let referenced_ids: std::collections::HashSet<u32> = parse_references(text)
        .into_iter()
        .map(|reference| reference.id)
        .collect();
    pasted_contents
        .iter()
        .filter(|content| referenced_ids.contains(&content.id))
        .cloned()
        .collect()
}

pub fn commit_submit_payload_to_transcript(
    app: &mut AppState,
    submit: &mut SubmitPayload,
    session_id: &str,
) {
    commit_submit_payload_to_transcript_with_meta(app, submit, session_id, false);
}

pub fn commit_internal_submit_payload_to_transcript(
    app: &mut AppState,
    submit: &mut SubmitPayload,
    session_id: &str,
) {
    commit_submit_payload_to_transcript_with_meta(app, submit, session_id, true);
}

fn commit_submit_payload_to_transcript_with_meta(
    app: &mut AppState,
    submit: &mut SubmitPayload,
    session_id: &str,
    is_meta: bool,
) {
    if let Some(uuid) = submit.user_message_uuid.as_deref() {
        if app.rebon_tui.transcript.get(uuid).is_some() {
            return;
        }
    }

    let uuid = if is_meta {
        ensure_internal_submit_payload_user_uuid(submit, session_id)
    } else {
        ensure_submit_payload_user_uuid(submit, session_id)
    };
    let timestamp = format_system_time_iso_ms(SystemTime::now());
    let mut content = Vec::new();
    if !submit.text.is_empty() {
        content.push(UserContentBlock::Text(UserTextBlock {
            text: submit.text.clone(),
        }));
    }
    content.extend(submit.image_pastes.iter().map(|image| {
        UserContentBlock::Image(UserImageBlock {
            source: serde_json::json!({
                "type": "base64",
                "media_type": image.media_type.clone().unwrap_or_else(|| String::from("image/png")),
                "data": image.content,
            }),
        })
    }));
    let msg = Message::User(UserMessage {
        uuid,
        timestamp,
        message: UserMessageInner {
            role: UserRole::User,
            content,
        },
        is_compact_summary: None,
        is_meta: is_meta.then_some(true),
        is_visible_in_transcript_only: None,
        image_paste_ids: (!submit.image_pastes.is_empty())
            .then(|| submit.image_pastes.iter().map(|image| image.id).collect()),
        plan_content: None,
    });
    reducer(&mut app.rebon_tui, Action::Commit(msg));
    commit_directory_attachment_rows(app, &submit.directory_attachments);
}

/// Apply a [`KeyAction::Submit`](crate::tui::event::KeyAction::Submit)
/// gesture to [`AppState`].
///
/// The flow:
///
/// 1. Trim trailing whitespace from the raw buffer.
/// 2. If the trimmed text is empty, return `None`; caller treats it
///    as a no-op Enter.
/// 3. Commit a `Message::User` row through the `rebon-tui` reducer
///    so the user's prompt shows up in the transcript immediately,
///    not after the engine round-trips it. The uuid format follows
///    the engine's `u-user-{session}-{ms}` convention so collisions
///    with engine-generated messages are impossible. The timestamp
///    goes through `rebon_session::format_system_time_iso_ms` so it
///    matches the ISO-8601 shape every other transcript row uses.
/// 4. Clear the prompt buffer and reset the cursor to 0.
/// 5. Return the trimmed text so the caller can hand it to
///    [`rebon_agent_core::PromptRequest::prompt`] without re-cloning.
///
/// The caller (runner) is responsible for spawning the async
/// executor call and tracking the in-flight `PromptOutcome`; this
/// function is strictly about the synchronous state mutation.
pub fn apply_submit(app: &mut AppState, raw_text: &str, session_id: &str) -> Option<SubmitPayload> {
    apply_submit_with_images(app, raw_text, session_id, Vec::new())
}

pub fn apply_submit_with_images(
    app: &mut AppState,
    raw_text: &str,
    session_id: &str,
    images: Vec<PromptPasteContent>,
) -> Option<SubmitPayload> {
    apply_submit_with_images_and_uuid(app, raw_text, session_id, images, None)
}

pub fn apply_submit_with_images_and_uuid(
    app: &mut AppState,
    raw_text: &str,
    session_id: &str,
    images: Vec<PromptPasteContent>,
    user_message_uuid: Option<String>,
) -> Option<SubmitPayload> {
    let raw_text = text_with_mode_prefix(raw_text, &app.mode);
    let mut submit =
        build_submit_payload(&raw_text, &app.pasted_contents, &app.cwd, &app.file_index)?;
    submit.user_message_uuid = user_message_uuid;
    submit.image_pastes.extend(images);

    app.pasted_contents.clear();
    app.next_paste_id = 1;

    commit_submit_payload_to_transcript(app, &mut submit, session_id);

    app.input.clear();
    app.mode = String::from("prompt");
    app.cursor_offset = 0;

    Some(submit)
}

pub fn apply_internal_submit(
    app: &mut AppState,
    mut submit: SubmitPayload,
    session_id: &str,
) -> Option<SubmitPayload> {
    submit.text = submit.text.trim_end().to_string();
    if submit.text.is_empty() {
        return None;
    }

    commit_internal_submit_payload_to_transcript(app, &mut submit, session_id);
    Some(submit)
}

fn commit_directory_attachment_rows(app: &mut AppState, attachments: &[DirectoryAttachment]) {
    for (idx, attachment) in attachments.iter().enumerate() {
        let uuid = format!("u-directory-{}-{idx}", rebon_types::wall_clock_ms_u128());
        let timestamp = format_system_time_iso_ms(SystemTime::now());
        reducer(
            &mut app.rebon_tui,
            Action::Commit(Message::Attachment(AttachmentRow {
                uuid,
                timestamp,
                attachment: serde_json::json!({
                    "type": "directory",
                    "path": attachment.path,
                    "content": attachment.content,
                    "displayPath": attachment.display_path,
                }),
            })),
        );
    }
}

fn collect_directory_attachments(text: &str, cwd: &str) -> Vec<DirectoryAttachment> {
    let mut attachments = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for mention in directory_mention_paths(text) {
        let normalized = normalize_mention_path(&mention);
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        let Some(dir_path) = safe_resolved_directory_path(cwd, &normalized) else {
            continue;
        };
        let content = format_directory_listing(&dir_path);
        attachments.push(DirectoryAttachment {
            path: dir_path.display().to_string(),
            display_path: display_path_for_directory(&normalized),
            content,
        });
    }
    attachments
}

fn directory_mention_paths(text: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    let mut indices = text.char_indices().peekable();
    while let Some((idx, ch)) = indices.next() {
        if ch != '@' || (idx > 0 && !text[..idx].ends_with(char::is_whitespace)) {
            continue;
        }
        let start = idx + ch.len_utf8();
        let mut end = start;
        while let Some(&(next_idx, next_ch)) = indices.peek() {
            if next_ch.is_whitespace() {
                break;
            }
            end = next_idx + next_ch.len_utf8();
            indices.next();
        }
        if end > start {
            mentions.push(text[start..end].to_string());
        }
    }
    mentions
}

fn normalize_mention_path(path: &str) -> String {
    path.trim_end_matches(['/', '\\']).replace('\\', "/")
}

fn display_path_for_directory(path: &str) -> String {
    path.replace('\\', "/")
}

fn safe_resolved_directory_path(cwd: &str, normalized_path: &str) -> Option<PathBuf> {
    let directory_query = format!("{}/", normalized_path.trim_end_matches('/'));
    let query = FileMentionQuery::parse(&directory_query).ok()?;
    canonical_file_mention_location(cwd, &query).map(|location| location.target_dir)
}

fn format_directory_listing(path: &Path) -> String {
    let mut entries = match std::fs::read_dir(path) {
        Ok(read_dir) => read_dir
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.is_empty() {
                    return None;
                }
                let is_dir = entry.file_type().ok().is_some_and(|ty| ty.is_dir());
                Some((name, is_dir))
            })
            .collect::<Vec<_>>(),
        Err(err) => return format!("Failed to list directory: {err}"),
    };
    entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let lines = entries
        .into_iter()
        .take(200)
        .map(|(name, is_dir)| {
            if is_dir {
                format!("{name}{}", std::path::MAIN_SEPARATOR)
            } else {
                name
            }
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        String::from("(empty directory)")
    } else {
        lines.join("\n")
    }
}

fn expand_directory_mentions_for_model(text: &str, attachments: &[DirectoryAttachment]) -> String {
    let mut model_text = text.to_string();
    for attachment in attachments {
        model_text.push_str("\n\nDirectory listing for ");
        model_text.push_str(&attachment.display_path);
        model_text.push_str(":\n");
        model_text.push_str(&attachment.content);
    }
    model_text
}

/// Replace `[Pasted text #N ...]` chip references in `text` with
/// their stored full content from `pasted_contents`. References
/// whose id is not found are left as-is.
pub fn expand_paste_references(
    text: &str,
    pasted_contents: &[rebon_types::PromptPasteContent],
) -> String {
    let refs = parse_references(text);
    if refs.is_empty() {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    for r in &refs {
        result.push_str(&text[last_end..r.index]);
        if let Some(content) = pasted_contents
            .iter()
            .find(|c| c.id == r.id && c.kind == "text")
        {
            result.push_str(&content.content);
        } else {
            // Not a stored paste (or an image ref) — keep as-is
            result.push_str(&r.matched_text);
        }
        last_end = r.index + r.matched_text.len();
    }
    result.push_str(&text[last_end..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::submit_payload::submit_payload_to_api_message;
    use rebon_api::ContentBlock as ApiContentBlock;
    use rebon_core::query::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};
    use rebon_tui::promptinput::{
        plan_input_change, plan_input_event, InputChangeInput, InputEventInput,
    };
    use QueuedCommandValue;

    fn history_entry(display: &str, timestamp: u64) -> HistoryEntry {
        HistoryEntry {
            display: display.into(),
            pasted_contents: Vec::new(),
            timestamp,
        }
    }

    fn text_paste(id: u32, content: &str) -> PromptPasteContent {
        PromptPasteContent {
            id,
            kind: "text".into(),
            content: content.into(),
            media_type: None,
            filename: None,
            source_path: None,
        }
    }

    fn test_edit(current: &str, cursor: usize, next: &str, intended: usize) -> TextEdit {
        TextEdit {
            event_input: InputEventInput {
                ch: String::new(),
                current_input: current.to_string(),
                cursor_offset: cursor,
                full_screen_dialog_open: false,
                is_macos: false,
                option_shortcut: None,
                terminal_display_name: None,
                footer_item_selected: false,
                ctrl: false,
                meta: false,
                escape: false,
                return_key: false,
                backspace: false,
                delete: false,
                help_open: false,
                speculation_active: false,
                side_question_visible: false,
                has_editable_queued_command: false,
                has_messages: false,
                input_is_empty: current.is_empty(),
                is_loading: false,
            },
            change_input: InputChangeInput {
                next_value: next.to_string(),
                current_input: current.to_string(),
                cursor_offset: cursor,
            },
            intended_cursor: intended,
        }
    }

    #[test]
    fn normal_char_insert_replaces_buffer_and_moves_cursor() {
        let mut app = AppState::new();
        app.input = String::from("helo");
        app.cursor_offset = 3;
        let edit = test_edit("helo", 3, "hello", 4);
        let event_plan = plan_input_event(&edit.event_input);
        let change_plan = plan_input_change(&edit.change_input);

        apply_text_edit(&mut app, &edit, &event_plan, &change_plan);

        assert_eq!(app.input, "hello");
        assert_eq!(app.cursor_offset, 4);
    }

    #[test]
    fn backspace_shortens_buffer_and_pulls_cursor_back() {
        let mut app = AppState::new();
        app.input = String::from("hello");
        app.cursor_offset = 5;
        let edit = test_edit("hello", 5, "hell", 4);
        let event_plan = plan_input_event(&edit.event_input);
        let change_plan = plan_input_change(&edit.change_input);

        apply_text_edit(&mut app, &edit, &event_plan, &change_plan);

        assert_eq!(app.input, "hell");
        assert_eq!(app.cursor_offset, 4);
    }

    #[test]
    fn tab_is_expanded_to_four_spaces_by_plan_input_change() {
        let mut app = AppState::new();
        app.input = String::from("ab");
        app.cursor_offset = 2;
        // Caller spliced the raw tab in; the plan expands it.
        let edit = test_edit("ab", 2, "ab\t", 3);
        let event_plan = plan_input_event(&edit.event_input);
        let change_plan = plan_input_change(&edit.change_input);

        apply_text_edit(&mut app, &edit, &event_plan, &change_plan);

        assert_eq!(app.input, "ab    ");
        // `plan_input_change` normalises tab → spaces but leaves
        // next_cursor_offset at None for this branch, so the
        // dispatch layer falls back to `intended_cursor` = 3 (the
        // byte length of the raw splice). Since "ab    " is 6
        // bytes, the intended cursor gets clamped accordingly — a
        // caller that knows the expanded byte width up front can
        // tighten this.
        assert!(app.cursor_offset <= app.input.len());
    }

    #[test]
    fn cursor_clamping_protects_against_out_of_bounds() {
        assert_eq!(clamp_cursor_offset("hello", 99), 5);
        assert_eq!(clamp_cursor_offset("你好", 4), 3);
        assert_eq!(clamp_cursor_offset("你好", 0), 0);
    }

    #[test]
    fn apply_submit_commits_user_message_and_clears_buffer() {
        let mut app = AppState::new();
        app.input = String::from("hello world");
        app.cursor_offset = 11;
        let snapshot = app.input.clone();

        let result = apply_submit(&mut app, &snapshot, "sess-test");

        assert_eq!(
            result.as_ref().map(|s| s.text.as_str()),
            Some("hello world")
        );
        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        let row = &app.rebon_tui.transcript.rows()[0];
        match row {
            Message::User(user) => {
                assert!(user.uuid.starts_with("u-user-sess-test-"));
                assert!(!user.timestamp.is_empty());
                match &user.message.content[0] {
                    UserContentBlock::Text(t) => assert_eq!(t.text, "hello world"),
                    other => panic!("expected text block, got {other:?}"),
                }
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn apply_internal_submit_preserves_live_input() {
        let mut app = AppState::new();
        app.input = String::from("draft while agent finishes");
        app.cursor_offset = 12;
        app.pasted_contents.push(PromptPasteContent {
            id: 1,
            kind: "text".into(),
            content: "draft paste".into(),
            media_type: None,
            filename: None,
            source_path: None,
        });
        app.next_paste_id = 2;

        let result = apply_internal_submit(
            &mut app,
            SubmitPayload {
                text: "<task-notification>\n<summary>done</summary>\n</task-notification>".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
            "sess-test",
        );

        assert_eq!(
            result.as_ref().map(|s| s.text.as_str()),
            Some("<task-notification>\n<summary>done</summary>\n</task-notification>")
        );
        assert_eq!(app.input, "draft while agent finishes");
        assert_eq!(app.cursor_offset, 12);
        assert_eq!(app.pasted_contents.len(), 1);
        assert_eq!(app.next_paste_id, 2);
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        let row = &app.rebon_tui.transcript.rows()[0];
        match row {
            Message::User(user) => {
                assert!(user.uuid.starts_with("u-internal-sess-test-"));
                assert_eq!(user.is_meta, Some(true));
                match &user.message.content[0] {
                    UserContentBlock::Text(t) => assert_eq!(
                        t.text,
                        "<task-notification>\n<summary>done</summary>\n</task-notification>"
                    ),
                    other => panic!("expected text block, got {other:?}"),
                }
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn apply_submit_trims_trailing_whitespace_only() {
        let mut app = AppState::new();
        app.input = String::from("  leading stays\n\n");
        app.cursor_offset = app.input.len();
        let snapshot = app.input.clone();

        let result = apply_submit(&mut app, &snapshot, "sess");

        // `trim_end` strips trailing whitespace but leaves leading
        // intact.
        assert_eq!(
            result.map(|submit| submit.text),
            Some(String::from("  leading stays"))
        );
        let row = &app.rebon_tui.transcript.rows()[0];
        match row {
            Message::User(user) => match &user.message.content[0] {
                UserContentBlock::Text(t) => assert_eq!(t.text, "  leading stays"),
                other => panic!("expected text block, got {other:?}"),
            },
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn apply_submit_returns_none_for_whitespace_only_input() {
        let mut app = AppState::new();
        app.input = String::from("   \n\t  ");
        app.cursor_offset = app.input.len();
        let original = app.input.clone();
        let snapshot = app.input.clone();

        let result = apply_submit(&mut app, &snapshot, "sess");

        assert_eq!(result, None);
        // Nothing committed, buffer untouched.
        assert_eq!(app.input, original);
        assert!(app.rebon_tui.transcript.is_empty());
    }

    #[test]
    fn apply_submit_handles_multibyte_text_without_panicking() {
        let mut app = AppState::new();
        app.input = String::from("你好，世界  ");
        app.cursor_offset = app.input.len();
        let snapshot = app.input.clone();

        let result = apply_submit(&mut app, &snapshot, "sess");

        assert_eq!(
            result.map(|submit| submit.text),
            Some(String::from("你好，世界"))
        );
        assert_eq!(app.rebon_tui.transcript.len(), 1);
    }

    #[test]
    fn bash_mode_empty_submit_returns_none_without_prefixing_bang() {
        let mut app = AppState::new();
        app.mode = String::from("bash");
        let snapshot = app.input.clone();

        let result = apply_submit(&mut app, &snapshot, "sess");

        assert_eq!(result, None);
        assert!(app.rebon_tui.transcript.is_empty());
    }

    #[test]
    fn apply_submit_prepends_bash_mode_prefix_and_resets_mode() {
        let mut app = AppState::new();
        app.mode = String::from("bash");
        app.input = String::from("echo hi");
        app.cursor_offset = app.input.len();
        let snapshot = app.input.clone();

        let result = apply_submit(&mut app, &snapshot, "sess").expect("bash submit");

        assert_eq!(result.text, "!echo hi");
        assert_eq!(app.mode, "prompt");
        match &app.rebon_tui.transcript.rows()[0] {
            Message::User(user) => match &user.message.content[0] {
                UserContentBlock::Text(t) => assert_eq!(t.text, "!echo hi"),
                other => panic!("expected text block, got {other:?}"),
            },
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn pop_queued_command_restores_bash_mode_without_visible_prefix() {
        let mut app = AppState::new();
        requeue_submit_payload_front(
            &mut app,
            SubmitPayload {
                text: "!echo hi".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
            String::from("bash"),
        );

        pop_queued_command_into_input(&mut app);

        assert_eq!(app.mode, "bash");
        assert_eq!(app.input, "echo hi");
        assert_eq!(app.cursor_offset, "echo hi".len());
    }

    #[test]
    fn normalized_change_pushes_previous_input_to_undo_stack() {
        let mut app = AppState::new();
        app.input = String::from("ab");
        app.cursor_offset = 2;
        let edit = test_edit("ab", 2, "a\tb", 3);
        let event_plan = plan_input_event(&edit.event_input);
        let change_plan = plan_input_change(&edit.change_input);

        apply_text_edit(&mut app, &edit, &event_plan, &change_plan);

        assert_eq!(app.undo_stack, vec![(String::from("ab"), 2)]);
    }

    #[test]
    fn pop_undo_restores_previous_input_and_cursor() {
        let mut app = AppState::new();
        app.undo_stack.push((String::from("hello"), 5));
        app.input = String::from("bye");
        app.cursor_offset = 3;

        assert!(pop_undo(&mut app));
        assert_eq!(app.input, "hello");
        assert_eq!(app.cursor_offset, 5);
        assert!(app.undo_stack.is_empty());
    }

    #[test]
    fn submit_payload_model_text_is_one_shot_and_preserves_display_text() {
        let mut app = AppState::new();
        app.mode = String::from("prompt");
        let mut submit = apply_submit(&mut app, "/ultraplan build auth", "sess")
            .expect("slash ultraplan display submit should be accepted");
        submit.model_text = Some("WRAPPED ULTRAPLAN PROMPT".into());

        assert_eq!(submit.text, "/ultraplan build auth");
        assert_eq!(submit.clone().prompt_text(), "WRAPPED ULTRAPLAN PROMPT");
        assert_eq!(submit.prompt_text(), "WRAPPED ULTRAPLAN PROMPT");
        assert_eq!(
            app.history.len(),
            0,
            "apply_submit does not write history itself"
        );
        let row = &app.rebon_tui.transcript.rows()[0];
        match row {
            Message::User(user) => match &user.message.content[0] {
                UserContentBlock::Text(t) => assert_eq!(t.text, "/ultraplan build auth"),
                other => panic!("expected text block, got {other:?}"),
            },
            other => panic!("expected user message, got {other:?}"),
        }

        let next = apply_submit(&mut app, "ordinary followup", "sess")
            .expect("ordinary followup should submit");
        assert_eq!(next.text, "ordinary followup");
        assert_eq!(next.model_text, None);
        assert_eq!(next.prompt_text(), "ordinary followup");
    }

    #[test]
    fn queued_submit_payload_keeps_raw_text_and_pops_model_override_once() {
        let mut app = AppState::new();
        app.mode = String::from("prompt");
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "please ultraplan cache invalidation".into(),
                model_text: Some("WRAPPED KEYWORD PROMPT".into()),
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "ordinary queued followup".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );

        assert_eq!(
            queued_text(&app.queued_commands[0]),
            Some("please ultraplan cache invalidation")
        );
        assert_eq!(
            queued_text(&app.queued_commands[1]),
            Some("ordinary queued followup")
        );

        let first = pop_next_queued_submit(&mut app).expect("first queued payload");
        assert_eq!(first.text, "please ultraplan cache invalidation");
        assert_eq!(first.prompt_text(), "WRAPPED KEYWORD PROMPT");

        let second = pop_next_queued_submit(&mut app).expect("second queued payload");
        assert_eq!(second.text, "ordinary queued followup");
        assert_eq!(second.model_text, None);
        assert_eq!(second.prompt_text(), "ordinary queued followup");
        assert_eq!(pop_next_queued_submit(&mut app), None);
    }

    #[test]
    fn history_and_new_app_state_do_not_inherit_model_override_text() {
        let mut app = AppState::new();
        let mut submit = apply_submit(&mut app, "/ultraplan build auth", "sess")
            .expect("slash ultraplan display submit should be accepted");
        submit.model_text = Some("WRAPPED ULTRAPLAN PROMPT".into());
        save_to_history(&mut app, &submit.text);

        assert_eq!(app.history[0].display, "/ultraplan build auth");
        assert!(!app.history[0].display.contains("REBON LOCAL ULTRAPLAN"));

        let new_app = AppState::new();
        assert!(new_app.history.is_empty());
        assert!(new_app.queued_submit_payloads.is_empty());
        assert!(new_app.queued_commands.is_empty());
    }

    #[test]
    fn enqueue_and_pop_next_queued_submit_are_fifo() {
        let mut app = AppState::new();
        app.mode = String::from("prompt");
        enqueue_submit(&mut app, String::from("first"));
        enqueue_submit(&mut app, String::from("second"));

        assert_eq!(queued_text(&app.queued_commands[0]), Some("first"));
        assert_eq!(queued_text(&app.queued_commands[1]), Some("second"));

        let first = pop_next_queued_submit(&mut app);
        let second = pop_next_queued_submit(&mut app);

        assert_eq!(first.map(|submit| submit.text), Some(String::from("first")));
        assert_eq!(
            second.map(|submit| submit.text),
            Some(String::from("second"))
        );
        assert_eq!(pop_next_queued_submit(&mut app), None);
    }

    #[test]
    fn visible_queue_len_caps_notifications_like_promptinput() {
        let mut app = AppState::new();
        for idx in 0..4 {
            app.queued_commands.push(QueuedCommand {
                mode: String::from("task-notification"),
                value: QueuedCommandValue::Text(format!("task-{idx}")),
            });
        }
        assert_eq!(visible_queue_len(&app), 3);
        assert!(queue_has_overflow(&app));
        assert_eq!(
            queue_overflow_message(&app).as_deref(),
            Some(
                "<task-notification>\n<summary>+2 more tasks completed</summary>\n<status>completed</status>\n</task-notification>"
            )
        );
    }

    #[test]
    fn queue_and_undo_affordance_state_is_visible_to_prompt_input() {
        let mut app = AppState::new();
        app.undo_stack.push(("old".into(), 3));
        enqueue_submit(&mut app, "one".into());

        assert_eq!(visible_queue_len(&app), 1);
        assert!(!app.undo_stack.is_empty());
    }

    #[test]
    fn clear_input_resets_buffer_and_cursor() {
        let mut app = AppState::new();
        app.input = String::from("draft");
        app.cursor_offset = 3;
        clear_input(&mut app);
        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
    }

    #[test]
    fn expand_paste_references_replaces_stored_text() {
        use rebon_types::PromptPasteContent;
        let contents = vec![PromptPasteContent {
            id: 1,
            kind: "text".into(),
            content: "line one\nline two\nline three".into(),
            media_type: None,
            filename: None,
            source_path: None,
        }];
        let input = "fix this [Pasted text #1 +2 lines] please";
        let expanded = expand_paste_references(input, &contents);
        assert_eq!(expanded, "fix this line one\nline two\nline three please");
    }

    #[test]
    fn expand_paste_references_leaves_unknown_ids_intact() {
        let input = "look at [Pasted text #99]";
        let expanded = expand_paste_references(input, &[]);
        assert_eq!(expanded, input);
    }

    #[test]
    fn expand_paste_references_handles_no_refs() {
        let expanded = expand_paste_references("plain text", &[]);
        assert_eq!(expanded, "plain text");
    }

    #[test]
    fn apply_submit_expands_paste_refs_and_clears_state() {
        use rebon_types::PromptPasteContent;
        let mut app = AppState::new();
        app.input = String::from("[Pasted text #1 +1 lines]");
        app.cursor_offset = app.input.len();
        app.next_paste_id = 2;
        app.pasted_contents.push(PromptPasteContent {
            id: 1,
            kind: "text".into(),
            content: "hello\nworld".into(),
            media_type: None,
            filename: None,
            source_path: None,
        });
        let snapshot = app.input.clone();

        let result = apply_submit(&mut app, &snapshot, "sess");

        assert_eq!(
            result.map(|submit| submit.text),
            Some(String::from("hello\nworld"))
        );
        assert!(app.pasted_contents.is_empty());
        assert_eq!(app.next_paste_id, 1);
    }

    #[test]
    fn build_submit_payload_collects_referenced_images() {
        use rebon_types::PromptPasteContent;
        let file_index = crate::file_scanner::FileIndex::default();
        let payload = build_submit_payload(
            "look [Image #2]",
            &[
                PromptPasteContent {
                    id: 1,
                    kind: "image".into(),
                    content: "ignored".into(),
                    media_type: Some("image/png".into()),
                    filename: Some("ignored.png".into()),
                    source_path: None,
                },
                PromptPasteContent {
                    id: 2,
                    kind: "image".into(),
                    content: "kept".into(),
                    media_type: Some("image/png".into()),
                    filename: Some("kept.png".into()),
                    source_path: None,
                },
            ],
            "",
            &file_index,
        )
        .expect("image ref should submit");

        assert_eq!(payload.text, "look [Image #2]");
        assert_eq!(payload.image_pastes.len(), 1);
        assert_eq!(payload.image_pastes[0].id, 2);
        assert_eq!(payload.image_pastes[0].content, "kept");
    }

    #[test]
    fn apply_submit_collects_directory_mentions_for_model_and_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let rebon_dir = tmp.path().join("rebon");
        std::fs::create_dir(&rebon_dir).unwrap();
        std::fs::create_dir(rebon_dir.join("src")).unwrap();
        std::fs::write(rebon_dir.join("README.md"), "# rebon").unwrap();

        let mut file_index = crate::file_scanner::FileIndex::default();
        file_index.merge(vec!["rebon/README.md".into(), "rebon/src/lib.rs".into()]);
        let mut app = AppState::new();
        app.cwd = tmp.path().display().to_string();
        app.file_index = file_index;
        app.input = "@rebon\\ 你好".into();
        app.cursor_offset = app.input.len();
        let snapshot = app.input.clone();

        let submit = apply_submit(&mut app, &snapshot, "sess").unwrap();

        assert_eq!(submit.text, "@rebon\\ 你好");
        let api_message = submit_payload_to_api_message(&submit);
        assert!(matches!(
            api_message.content.as_slice(),
            [ApiContentBlock::Text(text)] if text.text == "@rebon\\ 你好"
        ));
        assert_eq!(submit.directory_attachments.len(), 1);
        assert_eq!(submit.directory_attachments[0].display_path, "rebon");
        assert!(submit.directory_attachments[0]
            .content
            .contains("README.md"));
        assert!(submit.directory_attachments[0]
            .content
            .contains(&format!("src{}", std::path::MAIN_SEPARATOR)));
        let model_text = submit.model_text.as_deref().unwrap();
        assert!(model_text.contains("Directory listing for rebon:\n"));
        assert!(model_text.contains("README.md"));

        assert_eq!(app.rebon_tui.transcript.len(), 2);
        let rebon_tui::Message::Attachment(row) = &app.rebon_tui.transcript.rows()[1] else {
            panic!("expected directory attachment row");
        };
        assert_eq!(row.attachment["type"], "directory");
        assert_eq!(row.attachment["displayPath"], "rebon");
        assert_eq!(
            row.attachment["content"],
            submit.directory_attachments[0].content
        );
    }

    #[test]
    fn apply_submit_expands_parent_directory_without_file_index_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work/project");
        let shared = tmp.path().join("work/shared");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(shared.join("src")).unwrap();
        std::fs::write(shared.join("README.md"), "# shared").unwrap();

        let mut app = AppState::new();
        app.cwd = cwd.display().to_string();
        app.input = "review @../shared/ please".into();
        app.cursor_offset = app.input.len();
        assert!(app.file_index.is_empty());
        let snapshot = app.input.clone();

        let submit = apply_submit(&mut app, &snapshot, "sess-parent").unwrap();

        assert_eq!(submit.directory_attachments.len(), 1);
        let attachment = &submit.directory_attachments[0];
        assert_eq!(attachment.display_path, "../shared");
        assert_eq!(
            Path::new(&attachment.path).canonicalize().unwrap(),
            shared.canonicalize().unwrap()
        );
        assert!(attachment.content.contains("README.md"));
        assert!(attachment
            .content
            .contains(&format!("src{}", std::path::MAIN_SEPARATOR)));
        assert!(submit
            .model_text
            .as_deref()
            .unwrap()
            .contains("Directory listing for ../shared:\n"));
    }

    #[test]
    fn directory_dispatch_supports_multilevel_windows_query_and_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work/nested/project");
        let shared = tmp.path().join("work/shared");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("visible.txt"), "visible").unwrap();
        std::fs::create_dir_all(cwd.join("inner")).unwrap();

        let attachments =
            collect_directory_attachments(r"inspect @..\..\shared\ now", cwd.to_str().unwrap());
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].display_path, "../../shared");
        assert!(attachments[0].content.contains("visible.txt"));

        let absolute = format!("@{}/", tmp.path().display());
        assert!(collect_directory_attachments(&absolute, cwd.to_str().unwrap()).is_empty());
        assert!(collect_directory_attachments("@inner/../", cwd.to_str().unwrap()).is_empty());
        assert!(collect_directory_attachments("@C:/", cwd.to_str().unwrap()).is_empty());
    }

    #[test]
    fn directory_dispatch_rejects_symlink_target_outside_allowed_parent_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        let cwd = work.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        let link = work.join("escape");

        #[cfg(unix)]
        if std::os::unix::fs::symlink(outside.path(), &link).is_err() {
            return;
        }
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(outside.path(), &link).is_err() {
            // Creating symlinks may require elevated privileges on Windows.
            return;
        }

        assert!(collect_directory_attachments("@../escape/", cwd.to_str().unwrap()).is_empty());
    }

    #[test]
    fn apply_submit_commits_image_block_without_empty_text_block() {
        use rebon_types::PromptPasteContent;
        let mut app = AppState::new();
        app.pasted_contents.push(PromptPasteContent {
            id: 1,
            kind: "image".into(),
            content: "BASE64".into(),
            media_type: Some("image/png".into()),
            filename: Some("img.png".into()),
            source_path: None,
        });

        let result = apply_submit(&mut app, "", "sess");

        let submit = result.expect("image-only submit should be allowed");
        assert!(submit.text.is_empty());
        assert_eq!(submit.image_pastes.len(), 1);
        let message = app
            .rebon_tui
            .transcript
            .rows()
            .last()
            .expect("user message committed");
        let rebon_tui::Message::User(user_message) = message else {
            panic!("expected user message");
        };
        assert_eq!(user_message.image_paste_ids.as_deref(), Some(&[1][..]));
        assert!(matches!(
            user_message.message.content.as_slice(),
            [rebon_tui::UserContentBlock::Image(_)]
        ));
    }

    #[test]
    fn apply_submit_with_images_attaches_injected_image_to_payload_and_transcript() {
        let mut app = AppState::new();
        let image = PromptPasteContent {
            id: 42,
            kind: "image".into(),
            content: "REMOTE-BASE64".into(),
            media_type: Some("image/webp".into()),
            filename: Some("remote.webp".into()),
            source_path: None,
        };

        let submit =
            apply_submit_with_images(&mut app, "[image attached]", "sess-rich", vec![image])
                .expect("fallback text plus image should submit");

        assert_eq!(submit.text, "[image attached]");
        assert_eq!(submit.image_pastes.len(), 1);
        let api_message = submit_payload_to_api_message(&submit);
        assert!(matches!(
            api_message.content.as_slice(),
            [ApiContentBlock::Text(text), ApiContentBlock::Image(image)]
                if text.text == "[image attached]"
                    && image.source.media_type == "image/webp"
        ));
        let Message::User(user) = &app.rebon_tui.transcript.rows()[0] else {
            panic!("expected user message");
        };
        assert!(matches!(
            user.message.content.as_slice(),
            [UserContentBlock::Text(_), UserContentBlock::Image(_)]
        ));
    }

    #[test]
    fn history_up_navigates_newest_first() {
        let mut app = AppState::new();
        app.history = vec![
            history_entry("first", 1),
            history_entry("second", 2),
            history_entry("third", 3),
        ];
        app.input = "draft".into();

        history_up(&mut app);
        assert_eq!(app.input, "third");
        assert_eq!(app.history_index, 1);
        assert_eq!(app.cursor_offset, 0);
        assert_eq!(app.saved_draft.as_deref(), Some("draft"));

        history_up(&mut app);
        assert_eq!(app.input, "second");
        assert_eq!(app.history_index, 2);

        history_up(&mut app);
        assert_eq!(app.input, "first");
        assert_eq!(app.history_index, 3);

        // At oldest — no-op
        history_up(&mut app);
        assert_eq!(app.input, "first");
        assert_eq!(app.history_index, 3);
    }

    #[test]
    fn history_down_restores_draft() {
        let mut app = AppState::new();
        app.history = vec![history_entry("old", 1), history_entry("new", 2)];
        app.input = "my draft".into();

        history_up(&mut app); // → "new"
        history_up(&mut app); // → "old"
        history_down(&mut app); // → "new"
        assert_eq!(app.input, "new");
        assert_eq!(app.cursor_offset, 3); // at end

        history_down(&mut app); // → restore draft
        assert_eq!(app.input, "my draft");
        assert_eq!(app.cursor_offset, 8);
        assert_eq!(app.history_index, 0);
        assert!(app.saved_draft.is_none());
    }

    #[test]
    fn history_down_noop_when_at_draft() {
        let mut app = AppState::new();
        app.input = "hello".into();
        history_down(&mut app);
        assert_eq!(app.input, "hello");
        assert_eq!(app.history_index, 0);
    }

    #[test]
    fn history_up_noop_when_empty() {
        let mut app = AppState::new();
        app.input = "hello".into();
        history_up(&mut app);
        assert_eq!(app.input, "hello");
        assert_eq!(app.history_index, 0);
    }

    #[test]
    fn save_to_history_appends_and_resets_index() {
        let mut app = AppState::new();
        save_to_history(&mut app, "first prompt");
        save_to_history(&mut app, "second prompt");

        assert_eq!(
            app.history
                .iter()
                .map(|entry| entry.display.as_str())
                .collect::<Vec<_>>(),
            vec!["first prompt", "second prompt"]
        );
        assert_eq!(app.history_index, 0);

        // Whitespace-only is not saved
        assert!(save_to_history(&mut app, "  \n  ").is_none());
        assert_eq!(app.history.len(), 2);
    }

    #[test]
    fn history_up_saves_empty_draft_as_none() {
        let mut app = AppState::new();
        app.history = vec![history_entry("entry", 1)];
        app.input = "   ".into(); // whitespace-only

        history_up(&mut app);
        assert_eq!(app.input, "entry");
        assert!(app.saved_draft.is_none());

        history_down(&mut app);
        assert_eq!(app.input, ""); // empty, not whitespace
    }

    #[test]
    fn history_up_restores_pasted_contents_and_expand_still_works() {
        let mut app = AppState::new();
        app.history = vec![HistoryEntry {
            display: "fix [Pasted text #1 +1 lines]".into(),
            pasted_contents: vec![text_paste(1, "hello\nworld")],
            timestamp: 1,
        }];

        history_up(&mut app);

        assert_eq!(app.input, "fix [Pasted text #1 +1 lines]");
        assert_eq!(app.pasted_contents, vec![text_paste(1, "hello\nworld")]);
        assert_eq!(app.next_paste_id, 2);
        assert_eq!(
            expand_paste_references(&app.input, &app.pasted_contents),
            "fix hello\nworld"
        );
    }

    #[test]
    fn history_down_to_draft_restores_draft_paste_state() {
        let mut app = AppState::new();
        app.history = vec![HistoryEntry {
            display: "[Pasted text #4]".into(),
            pasted_contents: vec![text_paste(4, "old")],
            timestamp: 1,
        }];
        app.input = "draft [Pasted text #2]".into();
        app.pasted_contents = vec![text_paste(2, "draft paste")];
        app.next_paste_id = 3;

        history_up(&mut app);
        assert_eq!(app.next_paste_id, 5);
        history_down(&mut app);

        assert_eq!(app.input, "draft [Pasted text #2]");
        assert_eq!(app.pasted_contents, vec![text_paste(2, "draft paste")]);
        assert_eq!(app.next_paste_id, 3);
    }

    #[test]
    fn save_to_history_stores_only_referenced_text_pastes() {
        let mut app = AppState::new();
        app.pasted_contents = vec![
            text_paste(1, "kept"),
            text_paste(2, "ignored"),
            PromptPasteContent {
                id: 3,
                kind: "image".into(),
                content: "BASE64".into(),
                media_type: Some("image/png".into()),
                filename: Some("img.png".into()),
                source_path: None,
            },
        ];

        let saved = save_to_history(&mut app, "use [Pasted text #1]").unwrap();

        assert_eq!(saved.pasted_contents, vec![text_paste(1, "kept")]);
        assert_eq!(app.history[0].pasted_contents, vec![text_paste(1, "kept")]);
    }

    #[test]
    fn restored_paste_ids_do_not_conflict_with_future_pastes() {
        let mut app = AppState::new();
        app.history = vec![HistoryEntry {
            display: "old [Pasted text #3]".into(),
            pasted_contents: vec![text_paste(3, "old paste")],
            timestamp: 1,
        }];

        history_up(&mut app);
        let plan = rebon_tui::promptinput::paste_flow::plan_text_paste(
            &rebon_tui::promptinput::paste_flow::TextPasteInput {
                raw_text: "new\npaste\nwith\nenough\nlines".into(),
                current_input_is_empty: app.input.is_empty(),
                next_paste_id: app.next_paste_id,
                terminal_rows: 24,
            },
        );

        assert_eq!(plan.new_content.as_ref().map(|content| content.id), Some(4));
    }

    // ── has_editable_queued_commands ─────────────────────────────

    #[test]
    fn has_editable_detects_text_commands_only() {
        let mut app = AppState::new();
        assert!(!has_editable_queued_commands(&app));

        app.queued_commands.push(QueuedCommand {
            mode: "task-notification".into(),
            value: QueuedCommandValue::NonText,
        });
        assert!(!has_editable_queued_commands(&app));

        app.queued_commands.push(QueuedCommand {
            mode: "prompt".into(),
            value: QueuedCommandValue::Text("hello".into()),
        });
        assert!(has_editable_queued_commands(&app));
    }

    // ── pop_queued_command_into_input ────────────────────────────

    #[test]
    fn pop_queued_command_moves_last_text_into_input() {
        let mut app = AppState::new();
        app.queued_commands = vec![
            QueuedCommand {
                mode: "prompt".into(),
                value: QueuedCommandValue::Text("first".into()),
            },
            QueuedCommand {
                mode: "prompt".into(),
                value: QueuedCommandValue::Text("second".into()),
            },
        ];

        pop_queued_command_into_input(&mut app);
        assert_eq!(app.input, "second");
        assert_eq!(app.cursor_offset, 6);
        assert_eq!(app.queued_commands.len(), 1);
        assert_eq!(queued_text(&app.queued_commands[0]), Some("first"));
    }

    #[test]
    fn pop_queued_command_skips_non_text_items() {
        let mut app = AppState::new();
        app.queued_commands = vec![
            QueuedCommand {
                mode: "task-notification".into(),
                value: QueuedCommandValue::NonText,
            },
            QueuedCommand {
                mode: "prompt".into(),
                value: QueuedCommandValue::Text("editable".into()),
            },
        ];

        pop_queued_command_into_input(&mut app);
        assert_eq!(app.input, "editable");
        assert_eq!(app.cursor_offset, 8);
        // NonText item stays in queue
        assert_eq!(app.queued_commands.len(), 1);
        assert!(matches!(
            app.queued_commands[0].value,
            QueuedCommandValue::NonText
        ));
    }

    #[test]
    fn pop_queued_command_does_not_restore_mid_turn_consumed_submit() {
        let mut app = AppState::new();
        let poller = crate::session::mid_turn_queue::MidTurnQueuedSubmitPoller::new("sess-1");
        app.mid_turn_queued_submit_poller = Some(poller.clone());
        app.is_loading = true;
        app.input = "draft".into();
        app.cursor_offset = app.input.len();
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "already injected".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let uuid = app.queued_submit_payloads[0]
            .user_message_uuid
            .clone()
            .expect("queued uuid");
        assert_eq!(
            poller
                .poll(AttachmentPollRequest::new(
                    "sess-1",
                    "turn-1",
                    1,
                    AttachmentPollPhase::Regular,
                ))
                .len(),
            1
        );

        pop_queued_command_into_input(&mut app);

        assert_eq!(app.input, "draft");
        assert_eq!(app.cursor_offset, "draft".len());
        assert!(app.queued_commands.is_empty());
        assert!(app.queued_submit_payloads.is_empty());
        assert!(poller.drain_consumed_user_message_uuids().contains(&uuid));
    }

    #[test]
    fn pop_queued_command_noop_on_empty_queue() {
        let mut app = AppState::new();
        app.input = "untouched".into();
        app.cursor_offset = 3;

        pop_queued_command_into_input(&mut app);
        assert_eq!(app.input, "untouched");
        assert_eq!(app.cursor_offset, 3);
    }

    #[test]
    fn pop_queued_command_places_cursor_at_end_for_multibyte() {
        let mut app = AppState::new();
        app.queued_commands = vec![QueuedCommand {
            mode: "prompt".into(),
            value: QueuedCommandValue::Text("你好世界".into()),
        }];

        pop_queued_command_into_input(&mut app);
        assert_eq!(app.input, "你好世界");
        assert_eq!(app.cursor_offset, app.input.len());
    }

    // ── flush_queue ─────────────────────────────────────────────

    #[test]
    fn flush_queue_enqueues_current_input_and_clears() {
        let mut app = AppState::new();
        app.mode = "prompt".into();
        app.input = "current draft".into();
        app.cursor_offset = 5;
        app.queued_commands = vec![QueuedCommand {
            mode: "prompt".into(),
            value: QueuedCommandValue::Text("already queued".into()),
        }];

        flush_queue(&mut app);

        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
        assert_eq!(app.queued_commands.len(), 2);
        assert_eq!(queued_text(&app.queued_commands[0]), Some("already queued"));
        assert_eq!(queued_text(&app.queued_commands[1]), Some("current draft"));
    }

    #[test]
    fn flush_queue_does_not_enqueue_whitespace_only_input() {
        let mut app = AppState::new();
        app.mode = "prompt".into();
        app.input = "   \n  ".into();
        app.queued_commands = vec![QueuedCommand {
            mode: "prompt".into(),
            value: QueuedCommandValue::Text("queued".into()),
        }];

        flush_queue(&mut app);

        assert!(app.input.is_empty());
        assert_eq!(app.queued_commands.len(), 1);
        assert_eq!(queued_text(&app.queued_commands[0]), Some("queued"));
    }

    #[test]
    fn flush_queue_trims_trailing_whitespace_from_enqueued_input() {
        let mut app = AppState::new();
        app.mode = "prompt".into();
        app.input = "fix the bug   ".into();

        flush_queue(&mut app);

        assert_eq!(app.queued_commands.len(), 1);
        assert_eq!(queued_text(&app.queued_commands[0]), Some("fix the bug"));
    }

    #[test]
    fn flush_queue_with_empty_input_just_clears() {
        let mut app = AppState::new();
        app.mode = "prompt".into();
        app.queued_commands = vec![QueuedCommand {
            mode: "prompt".into(),
            value: QueuedCommandValue::Text("queued".into()),
        }];

        flush_queue(&mut app);

        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
        // Queue untouched — auto-drain handles it
        assert_eq!(app.queued_commands.len(), 1);
    }

    // ── apply_event_plan tests ─────────────────────────

    fn event_plan_with(f: impl FnOnce(&mut InputEventPlan)) -> InputEventPlan {
        let mut plan = InputEventPlan::default();
        f(&mut plan);
        plan
    }

    #[test]
    fn event_plan_option_meta_hint_sets_toast() {
        let mut app = AppState::new();
        assert!(app.option_meta_hint_toast.is_none());

        let plan = event_plan_with(|p| {
            p.option_meta_hint = Some(rebon_tui::promptinput::OptionMetaHint {
                shortcut: "alt+p".into(),
                terminal_display_name: Some("Ghostty".into()),
            });
        });
        apply_event_plan(&mut app, &plan);

        let toast = app.option_meta_hint_toast.unwrap();
        assert_eq!(toast.shortcut, "alt+p");
        assert_eq!(toast.terminal_display_name.as_deref(), Some("Ghostty"));
    }

    #[test]
    fn event_plan_reset_prompt_mode_resets_to_prompt() {
        let mut app = AppState::new();
        app.mode = "bash".into();

        let plan = event_plan_with(|p| p.reset_prompt_mode = true);
        apply_event_plan(&mut app, &plan);

        assert_eq!(app.mode, "prompt");
    }

    #[test]
    fn event_plan_close_help_clears_help_open() {
        let mut app = AppState::new();
        app.help_open = true;

        let plan = event_plan_with(|p| p.close_help = true);
        apply_event_plan(&mut app, &plan);

        assert!(!app.help_open);
    }

    #[test]
    fn event_plan_type_to_exit_footer_updates_input_and_clears_pickers() {
        use crate::tui::slash_picker::SlashPickerState;
        let mut app = AppState::new();
        app.input = "abc".into();
        app.cursor_offset = 1;
        app.slash_picker = Some(SlashPickerState::empty_for_test());

        let plan = InputEventPlan {
            primary_action: InputEventPrimaryAction::TypeToExitFooter {
                next_input: "axbc".into(),
                next_cursor_offset: 2,
            },
            ..InputEventPlan::default()
        };
        apply_event_plan(&mut app, &plan);

        assert_eq!(app.input, "axbc");
        assert_eq!(app.cursor_offset, 2);
        assert!(app.slash_picker.is_none());
    }

    #[test]
    fn event_plan_abort_speculation_clears_flag() {
        let mut app = AppState::new();
        app.speculation_active = true;

        let plan = InputEventPlan {
            primary_action: InputEventPrimaryAction::AbortSpeculation,
            ..InputEventPlan::default()
        };
        apply_event_plan(&mut app, &plan);

        assert!(!app.speculation_active);
    }

    #[test]
    fn event_plan_dismiss_side_question_clears_flag() {
        let mut app = AppState::new();
        app.side_question_visible = true;

        let plan = InputEventPlan {
            primary_action: InputEventPrimaryAction::DismissSideQuestion,
            ..InputEventPlan::default()
        };
        apply_event_plan(&mut app, &plan);

        assert!(!app.side_question_visible);
    }

    #[test]
    fn event_plan_defer_to_footer_clears_pickers() {
        use crate::tui::slash_picker::SlashPickerState;
        let mut app = AppState::new();
        app.slash_picker = Some(SlashPickerState::empty_for_test());

        let plan = InputEventPlan {
            primary_action: InputEventPrimaryAction::DeferToFooterSelection,
            ..InputEventPlan::default()
        };
        apply_event_plan(&mut app, &plan);

        assert!(app.slash_picker.is_none());
        assert!(app.at_mention_picker.is_none());
    }

    #[test]
    fn event_plan_trigger_double_press_esc_sets_timestamp() {
        let mut app = AppState::new();
        assert_eq!(app.last_esc_press_ms, 0);

        let plan = InputEventPlan {
            primary_action: InputEventPrimaryAction::TriggerDoublePressEscFromEmpty,
            ..InputEventPlan::default()
        };
        apply_event_plan(&mut app, &plan);

        assert!(app.last_esc_press_ms > 0);
    }

    // ── toggle_help tests ───────────────────────────────────────

    #[test]
    fn toggle_help_opens_when_closed() {
        let mut app = AppState::new();
        assert!(!app.help_open);

        let edit = test_edit("", 0, "?", 1);
        let event_plan = plan_input_event(&edit.event_input);
        let change_plan = plan_input_change(&edit.change_input);
        apply_text_edit(&mut app, &edit, &event_plan, &change_plan);

        assert!(app.help_open);
    }

    #[test]
    fn toggle_help_closes_when_open() {
        let mut app = AppState::new();
        app.help_open = true;

        let edit = test_edit("", 0, "?", 1);
        let event_plan = plan_input_event(&edit.event_input);
        let change_plan = plan_input_change(&edit.change_input);
        apply_text_edit(&mut app, &edit, &event_plan, &change_plan);

        assert!(!app.help_open);
    }

    // ── normalized_change side-effect tests ─────────────────────

    #[test]
    fn normalized_change_close_help_clears_help_open() {
        let mut app = AppState::new();
        app.help_open = true;
        app.input = "a".into();
        app.cursor_offset = 1;

        // Typing a normal character triggers close_help in the
        // NormalizedInputChange path (every non-'?' input closes help).
        let edit = test_edit("a", 1, "ab", 2);
        let event_plan = plan_input_event(&edit.event_input);
        let change_plan = plan_input_change(&edit.change_input);
        apply_text_edit(&mut app, &edit, &event_plan, &change_plan);

        assert!(!app.help_open);
        assert_eq!(app.input, "ab");
    }

    #[test]
    fn normalized_change_abort_speculation_clears_flag() {
        let mut app = AppState::new();
        app.speculation_active = true;

        let edit = test_edit("", 0, "x", 1);
        let event_plan = plan_input_event(&edit.event_input);
        let change_plan = plan_input_change(&edit.change_input);
        apply_text_edit(&mut app, &edit, &event_plan, &change_plan);

        // Any typing aborts speculation via the input-change path.
        assert!(!app.speculation_active);
    }

    #[test]
    fn normalized_change_dismiss_stash_hint() {
        let mut app = AppState::new();
        assert!(!app.stash_hint_dismissed);

        let edit = test_edit("", 0, "x", 1);
        let event_plan = plan_input_event(&edit.event_input);
        let change_plan = plan_input_change(&edit.change_input);
        apply_text_edit(&mut app, &edit, &event_plan, &change_plan);

        assert!(app.stash_hint_dismissed);
    }

    #[test]
    fn normalized_change_abort_prompt_suggestion() {
        let mut app = AppState::new();
        app.prompt_suggestion_active = true;

        let edit = test_edit("", 0, "x", 1);
        let event_plan = plan_input_event(&edit.event_input);
        let change_plan = plan_input_change(&edit.change_input);
        apply_text_edit(&mut app, &edit, &event_plan, &change_plan);

        assert!(!app.prompt_suggestion_active);
    }
}
