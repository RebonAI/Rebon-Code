//! Rewind / resume dialog logic — transcript truncation, code
//! restoration, and session transcript replacement.

use crate::session::commands::rewind::{
    apply_code_rewind, durable_conversation_rewind, durable_conversation_summary,
    file_history_diff_stats_to_picker, replace_session_transcript_from_rows,
    selectable_rewind_user, DurableConversationRewind,
};
#[cfg(test)]
use crate::session::commands::rewind::{
    committed_projection_outcome, rewind_session_for_owner, transcript_entries_from_rows,
};
use crate::tui::app::AppState;
use crate::tui::rewind_dialog::{
    build_message_option_label, RewindDialogState, RewindMessageOption,
};
use crate::tui::wiring::TuiEngineSession;
#[cfg(test)]
use std::path::Path;

use super::inject_system_message;

pub(super) fn parse_resume_command(text: &str) -> bool {
    super::commands::is_bare_command(text, "resume")
}

/// Recognize `/rewind` — or `/checkpoint`, its catalog alias — in the current
/// prompt input.
pub(super) fn parse_rewind_command(text: &str) -> bool {
    super::commands::is_bare_command(text, "rewind")
}

/// Build the rewind dialog by extracting selectable user messages from the
/// transcript and creating options.
pub(super) fn open_rewind_dialog(app: &AppState, session: &TuiEngineSession) -> RewindDialogState {
    let rows = app.rebon_tui.transcript.rows();
    let file_history_store = session.engine_half.runtime.file_history_tracker.store();
    let mut options: Vec<RewindMessageOption> = Vec::new();
    for row in rows.iter() {
        let Some((message_id, raw_text, _filter_text)) = selectable_rewind_user(row) else {
            continue;
        };
        let label = build_message_option_label(&raw_text, 80);
        let capability = file_history_store.restore_capability(&message_id);
        let diff_stats = if matches!(capability, rebon_session::FileRestoreCapability::Clean(_)) {
            file_history_store
                .diff_stats(&message_id)
                .ok()
                .flatten()
                .map(file_history_diff_stats_to_picker)
        } else {
            None
        };
        let label = match &capability {
            rebon_session::FileRestoreCapability::Clean(plan) => format!(
                "{label}  [code: {} write, {} delete, {} unchanged]",
                plan.writes, plan.deletes, plan.unchanged
            ),
            rebon_session::FileRestoreCapability::Conflicted(plan) => format!(
                "{label}  [code unavailable: {} manual/bash conflict(s)]",
                plan.conflicts
            ),
            rebon_session::FileRestoreCapability::Unavailable(reason) => {
                format!("{label}  [code unavailable: {reason:?}]")
            }
        };
        options.push(RewindMessageOption {
            label,
            raw_text,
            diff_stats,
            file_restore: capability,
        });
    }
    RewindDialogState::open(options)
}

fn apply_durable_summary(
    app: &mut AppState,
    session: &TuiEngineSession,
    selected_user_uuid: &str,
    mode: rebon_session::SummarizeConversationMode,
    feedback: Option<&str>,
) {
    let (receipt, rows) = match durable_conversation_summary(
        session,
        selected_user_uuid,
        mode,
        feedback,
    ) {
        Ok(committed) => committed,
        Err((committed, error)) => {
            if committed {
                session
                    .engine_half
                    .projection_invalid
                    .store(true, std::sync::atomic::Ordering::Release);
                inject_system_message(
                        app,
                        "rewind-error",
                        &format!("The summary committed on disk, but finalization could not be verified: {error}. Restart or resume this session before sending another turn."),
                    );
            } else {
                // A CAS/storage refusal must not mutate the visible transcript.
                tracing::error!(%error, "rebon: durable summary was not committed");
            }
            return;
        }
    };
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            session
                .engine_half
                .projection_invalid
                .store(true, std::sync::atomic::Ordering::Release);
            inject_system_message(
                app,
                "rewind-error",
                &format!("The summary committed on disk, but the TUI projection could not be rebuilt: {error}. Restart or resume this session before sending another turn."),
            );
            return;
        }
    };
    if !session
        .engine_half
        .handler
        .state()
        .clone()
        .replace_transcript_entries(&session.session_id, receipt.entries.clone())
    {
        session
            .engine_half
            .projection_invalid
            .store(true, std::sync::atomic::Ordering::Release);
        inject_system_message(
            app,
            "rewind-error",
            "The summary committed on disk, but the engine transcript could not be refreshed; restart or resume this session before sending another turn.",
        );
        return;
    }
    app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(rows);
    app.transcript_measure_cache.clear();
    app.follow_transcript_tail = true;
    session
        .engine_half
        .projection_invalid
        .store(false, std::sync::atomic::Ordering::Release);
    tracing::info!(
        dropped = receipt.dropped_count,
        "rebon: durable summary committed"
    );
}

/// Hand a rewind to the process that owns this session.
///
/// Summaries are not routed: they run a model call against the owner's
/// context, and the request for that is `Compact`'s neighbourhood rather than
/// this one. A mirrored terminal is told so plainly instead of silently doing
/// nothing.
fn send_rewind_to_owner(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    selected_index: usize,
    option: rebon_picker::message_selector::RestoreOption,
    _feedback: Option<String>,
) {
    use rebon_picker::message_selector::RestoreOption;

    let scope = match option {
        RestoreOption::Conversation => "conversation",
        RestoreOption::Code => "code",
        RestoreOption::Both => "both",
        RestoreOption::Summarize | RestoreOption::SummarizeUpTo => {
            inject_system_message(
                app,
                "rewind-error",
                "Summarising a mirrored session is not available yet; rewind the conversation, or open it in the terminal that owns it.",
            );
            return;
        }
        RestoreOption::Nevermind => return,
    };
    let Some((selected_user_uuid, prompt, _)) = app
        .rebon_tui
        .transcript
        .rows()
        .iter()
        .filter_map(selectable_rewind_user)
        .nth(selected_index)
    else {
        inject_system_message(
            app,
            "rewind-error",
            "The selected rewind turn is no longer available.",
        );
        return;
    };
    let Some((job_id, endpoint, busy)) =
        session.remote_background_attachment.as_ref().map(|remote| {
            (
                remote.job_id.clone(),
                remote.endpoint(),
                remote.pending_command.is_some(),
            )
        })
    else {
        return;
    };
    let Some(endpoint) = endpoint else {
        inject_system_message(
            app,
            "rewind-error",
            &format!(
                "Worker {job_id} is stopped, so the rewind was not sent. Type a prompt to continue the session in a new worker first."
            ),
        );
        return;
    };
    if busy {
        inject_system_message(
            app,
            "warning",
            "Still waiting on the previous command to answer — the rewind was not sent.",
        );
        return;
    }
    let receiver = crate::background::spawn_remote_session_command(
        &job_id,
        &endpoint,
        "rewind".to_string(),
        vec![selected_user_uuid, scope.to_string()],
    );
    if let Some(remote) = session.remote_background_attachment.as_mut() {
        remote.pending_command = Some(receiver);
    }
    // Prefill from the row this terminal picked rather than waiting for the
    // owner to hand the text back: it is the same turn, and the composer
    // should be ready the moment the user asked for it.
    if matches!(option, RestoreOption::Conversation | RestoreOption::Both) {
        app.input = prompt;
        app.cursor_offset = app.input.len();
    }
    inject_system_message(app, "rewind", "Asked the session's host to rewind…");
    app.follow_transcript_tail = true;
}

/// Apply the outcome of the rewind dialog — truncate the transcript
/// to the selected user message and optionally prefill the input.
pub(super) fn apply_rewind_outcome(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    selected_index: usize,
    option: rebon_picker::message_selector::RestoreOption,
    feedback: Option<String>,
) {
    use rebon_picker::message_selector::RestoreOption;

    if option == RestoreOption::Nevermind {
        return;
    }

    // A mirrored terminal does not hold this session's lock, so it cannot
    // rewrite the transcript: the compare-and-swap is only sound against the
    // chain the owner holds. It used to say so and stop. Now it asks the owner
    // to do it, and the projection follows on the next refresh.
    if session.remote_background_attachment.is_some() {
        send_rewind_to_owner(app, session, selected_index, option, feedback);
        return;
    }

    // Map the selected_index (index among user messages) back to the
    // transcript row index. We walk user messages in the same order
    // open_rewind_dialog built them.
    let rows = app.rebon_tui.transcript.rows();
    let rows_snapshot = rows.to_vec();
    let file_history_store = session.engine_half.runtime.file_history_tracker.store();
    let mut user_msg_idx = 0usize;
    let mut transcript_boundary = rows.len();
    for (i, row) in rows.iter().enumerate() {
        if selectable_rewind_user(row).is_some() {
            if user_msg_idx == selected_index {
                transcript_boundary = i;
                break;
            }
            user_msg_idx += 1;
        }
    }

    match option {
        RestoreOption::Conversation | RestoreOption::Both => {
            let mut post_fork_notices: Vec<String> = Vec::new();
            if option == RestoreOption::Both {
                let capability = rows
                    .get(transcript_boundary)
                    .and_then(selectable_rewind_user)
                    .map(|(message_id, _, _)| file_history_store.restore_capability(&message_id));
                if !matches!(
                    capability,
                    Some(rebon_session::FileRestoreCapability::Clean(_))
                ) {
                    inject_system_message(
                        app,
                        "rewind-error",
                        &format!(
                            "Combined rewind was not started: code preflight is not clean ({capability:?}). Conversation and code were unchanged."
                        ),
                    );
                    return;
                }
            }
            let Some((selected_user_uuid, _, _)) = rows
                .get(transcript_boundary)
                .and_then(selectable_rewind_user)
            else {
                inject_system_message(
                    app,
                    "rewind-error",
                    "The selected rewind turn is no longer available.",
                );
                return;
            };
            let committed =
                match durable_conversation_rewind(session, &selected_user_uuid, &rows_snapshot) {
                    Ok(committed) => committed,
                    Err(error) => {
                        // Nothing in the UI or engine is truncated until durable storage
                        // accepts the canonical compare-and-swap request.
                        inject_system_message(
                            app,
                            "rewind-error",
                            &format!("Conversation rewind was not committed: {error}"),
                        );
                        return;
                    }
                };
            let (receipt, durable_rows) = match committed {
                DurableConversationRewind::Committed { receipt, rows } => (receipt, rows),
                DurableConversationRewind::CommittedProjectionFailed { receipt, error } => {
                    session
                        .engine_half
                        .projection_invalid
                        .store(true, std::sync::atomic::Ordering::Release);
                    app.input = receipt.prefill_prompt;
                    app.cursor_offset = app.input.len();
                    inject_system_message(
                        app,
                        "rewind-error",
                        &format!(
                            "Conversation rewind committed on disk, but the TUI projection could not be rebuilt: {error}. Restart or resume this session before sending another turn."
                        ),
                    );
                    return;
                }
            };
            app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(durable_rows);
            app.transcript_measure_cache.clear();
            app.input = receipt.prefill_prompt.clone();
            app.cursor_offset = app.input.len();
            let conversation_committed =
                replace_session_transcript_from_rows(session, app.rebon_tui.transcript.rows());
            if !conversation_committed {
                session
                    .engine_half
                    .projection_invalid
                    .store(true, std::sync::atomic::Ordering::Release);
                // Durable storage is authoritative. Keep the UI on the receipt and
                // report that the engine projection needs a reload rather than
                // pretending storage failed.
                inject_system_message(
                    app,
                    "rewind-error",
                    "Conversation rewind committed on disk, but the engine transcript could not be refreshed; restart or resume this session before sending another turn.",
                );
                return;
            }
            session
                .engine_half
                .projection_invalid
                .store(false, std::sync::atomic::Ordering::Release);
            if option == RestoreOption::Both {
                match apply_code_rewind(
                    &rows_snapshot,
                    transcript_boundary,
                    &file_history_store,
                    session.session_active_lock.as_ref(),
                ) {
                    Ok(report) => {
                        post_fork_notices.push(format!(
                            "Conversation rewind committed. Code restore then changed {} file(s), skipped {} ({} late conflict(s), {} I/O/backup error(s)). This combined action was sequential, not atomic.",
                            report.restored,
                            report.skipped,
                            report.conflicts.len(),
                            report.errors.len()
                        ));
                    }
                    Err(err) => post_fork_notices.push(format!(
                        "Conversation rewind committed, but the later code restore failed: {err}. This is a partial, non-atomic result."
                    )),
                }
            }
            for notice in post_fork_notices {
                inject_system_message(app, "rewind-code", &notice);
            }
            app.follow_transcript_tail = true;
            tracing::info!(
                boundary = transcript_boundary,
                "rebon: rewind — forked conversation"
            );
        }
        RestoreOption::Summarize => {
            let Some((selected_user_uuid, _, _)) = rows
                .get(transcript_boundary)
                .and_then(selectable_rewind_user)
            else {
                tracing::error!("rebon: selected summary turn is no longer visible");
                return;
            };
            apply_durable_summary(
                app,
                session,
                &selected_user_uuid,
                rebon_session::SummarizeConversationMode::FromSelected,
                feedback.as_deref(),
            );
        }
        RestoreOption::SummarizeUpTo => {
            let Some((selected_user_uuid, _, _)) = rows
                .get(transcript_boundary)
                .and_then(selectable_rewind_user)
            else {
                tracing::error!("rebon: selected summary turn is no longer visible");
                return;
            };
            apply_durable_summary(
                app,
                session,
                &selected_user_uuid,
                rebon_session::SummarizeConversationMode::UpToSelected,
                feedback.as_deref(),
            );
        }
        RestoreOption::Code => {
            match apply_code_rewind(
                rows,
                transcript_boundary,
                &file_history_store,
                session.session_active_lock.as_ref(),
            ) {
                Ok(report) => {
                    let restored = report.restored;
                    let conflicts = report.conflicts;
                    let errors = report.errors;
                    if restored > 0 {
                        inject_system_message(
                            app,
                            "rewind-code",
                            &format!("Rewound code in {restored} file(s)."),
                        );
                    } else {
                        inject_system_message(
                            app,
                            "rewind-code",
                            "The code has not changed (nothing was restored).",
                        );
                    }
                    if !conflicts.is_empty() {
                        let details = conflicts
                            .iter()
                            .take(8)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ");
                        inject_system_message(
                            app,
                            "rewind-code",
                            &format!(
                                "Skipped {} file(s) with manual/bash changes while rewinding code: {details}",
                                conflicts.len()
                            ),
                        );
                    }
                    if !errors.is_empty() {
                        let details = errors
                            .iter()
                            .take(8)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("; ");
                        inject_system_message(
                            app,
                            "rewind-error",
                            &format!(
                                "Code rewind had {} I/O or backup error(s): {details}",
                                errors.len()
                            ),
                        );
                    }
                }
                Err(err) => {
                    inject_system_message(
                        app,
                        "rewind-error",
                        &format!("Failed to restore code: {err}"),
                    );
                }
            }
        }
        RestoreOption::Nevermind => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session_shell::session_command_inputs_from_app;
    use serde_json::json;
    use tempfile::TempDir;

    use super::super::test_support::make_test_tui_session;

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

    fn push_test_assistant_message(app: &mut AppState, uuid: &str, text: &str) {
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
                uuid: uuid.to_string(),
                timestamp: format!("2026-04-14T00:00:00.{}Z", uuid.trim_start_matches('a')),
                message: rebon_tui::AssistantMessageInner {
                    role: rebon_tui::AssistantRole::Assistant,
                    content: vec![rebon_tui::AssistantContentBlock::Text(
                        rebon_tui::AssistantTextBlock {
                            text: text.to_string(),
                        },
                    )],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            })),
        );
    }

    fn push_test_tool_use_message(
        app: &mut AppState,
        uuid: &str,
        file_path: &Path,
        original_file: Option<&str>,
        content: &str,
    ) {
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
                uuid: uuid.to_string(),
                timestamp: format!("2026-04-14T00:00:00.{}Z", uuid.trim_start_matches('a')),
                message: rebon_tui::AssistantMessageInner {
                    role: rebon_tui::AssistantRole::Assistant,
                    content: vec![rebon_tui::AssistantContentBlock::ToolUse(
                        rebon_tui::AssistantToolUseBlock {
                            id: format!("tool-{uuid}"),
                            name: "Write".into(),
                            input: json!({}),
                            tool_call_content: None,
                            raw_output: Some(json!({
                                "filePath": file_path.to_string_lossy(),
                                "type": "update",
                                "originalFile": original_file,
                                "content": content,
                            })),
                            title: None,
                            locations: None,
                            status: None,
                        },
                    )],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            })),
        );
    }

    fn loaded_transcript_for(session: &TuiEngineSession) -> Vec<rebon_session::TranscriptEntry> {
        session
            .engine_half
            .handler
            .state()
            .get_session(&session.session_id)
            .expect("session exists")
            .loaded_transcript
            .clone()
    }

    #[test]
    fn committed_projection_failure_remains_a_typed_committed_outcome() {
        let stamp = rebon_session::TranscriptStamp {
            byte_len: 0,
            modified: None,
            sha256: [7; 32],
        };
        let receipt = rebon_session::RewindConversationReceipt {
            mutation_id: "mutation-committed".into(),
            old_stamp: stamp.clone(),
            new_stamp: stamp.clone(),
            new_source_head_uuid: "head".into(),
            canonical_history: rebon_session::SessionHistorySnapshot {
                transcript_path: std::path::PathBuf::from("committed.jsonl"),
                stamp,
                source_head_uuid: "head".into(),
                turns: Vec::new(),
            },
            prefill_prompt: "retry prompt".into(),
            recovery_backup: std::path::PathBuf::from("backup.jsonl"),
        };

        let outcome = committed_projection_outcome(
            receipt,
            Err("captured canonical row is unavailable".into()),
        );

        let DurableConversationRewind::CommittedProjectionFailed { receipt, error } = outcome
        else {
            panic!("a post-receipt projection failure must remain committed");
        };
        assert_eq!(receipt.mutation_id, "mutation-committed");
        assert_eq!(receipt.prefill_prompt, "retry prompt");
        assert!(error.contains("canonical row"));
    }

    /// Write the rendered rows out as the canonical transcript, which is what
    /// every durable rewind compares against.
    fn write_owner_test_transcript(app: &AppState, session: &TuiEngineSession) {
        let transcript_path = rebon_session::ensure_session_file_path(
            &session.projects_root,
            &session.cwd,
            &session.session_id,
        )
        .unwrap();
        let mut body = Vec::new();
        for entry in transcript_entries_from_rows(app.rebon_tui.transcript.rows()) {
            let mut raw = entry.raw;
            if let Some(object) = raw.as_object_mut() {
                object.insert("type".into(), serde_json::Value::String(entry.entry_type));
                object.insert("uuid".into(), serde_json::Value::String(entry.uuid));
                object.insert(
                    "parentUuid".into(),
                    entry
                        .parent_uuid
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            serde_json::to_writer(&mut body, &raw).unwrap();
            body.push(b'\n');
        }
        std::fs::write(&transcript_path, body).unwrap();
    }

    /// A mirrored terminal cannot rewind for itself, so the owner does it.
    /// This is the owner half: the same durable compare-and-swap the local
    /// path takes, driven by a turn id instead of a dialog selection.
    #[test]
    fn owner_side_rewind_truncates_the_transcript_and_the_engine_chain() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u-1", "first prompt");
        push_test_assistant_message(&mut app, "a-1", "first reply");
        push_test_user_message(&mut app, "u-2", "second prompt");
        push_test_assistant_message(&mut app, "a-2", "second reply");

        let temp = TempDir::new().unwrap();
        let mut session = make_test_tui_session();
        session.projects_root = temp.path().join("projects");
        session.cwd = temp.path().join("work").to_string_lossy().to_string();
        write_owner_test_transcript(&app, &session);
        session.session_active_lock = rebon_session::try_acquire_session_active_lock(
            &session.projects_root,
            &session.cwd,
            &session.session_id,
        )
        .unwrap();
        assert!(replace_session_transcript_from_rows(
            &session,
            app.rebon_tui.transcript.rows()
        ));

        let report = rewind_session_for_owner(
            &session_command_inputs_from_app(&app, session.ui_mode),
            &session,
            "u-2",
            rebon_session_host::RewindScopeWire::Conversation,
        )
        .expect("the owner holds the lock, so it can rewrite");

        assert_eq!(report.turns, Some(1));
        assert!(report.describe().contains("1 turn(s) remain"));
        let entries = loaded_transcript_for(&session);
        assert_eq!(entries.len(), 2, "the second turn is gone from the chain");
        assert_eq!(entries[0].uuid, "u-1");
        assert_eq!(entries[1].uuid, "a-1");
    }

    /// A turn that is not in the conversation must be refused by name rather
    /// than silently rewinding to something else.
    #[test]
    fn owner_side_rewind_refuses_a_turn_it_does_not_have() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u-1", "first prompt");
        let session = make_test_tui_session();

        let error = rewind_session_for_owner(
            &session_command_inputs_from_app(&app, session.ui_mode),
            &session,
            "u-nowhere",
            rebon_session_host::RewindScopeWire::Conversation,
        )
        .unwrap_err();

        assert!(error.contains("u-nowhere"), "unexpected refusal: {error}");
    }

    /// A combined rewind refuses before it changes anything when the code half
    /// cannot land: truncating the conversation first and discovering that
    /// afterwards is the worst of both.
    #[test]
    fn owner_side_combined_rewind_refuses_before_touching_the_conversation() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u-1", "first prompt");
        push_test_assistant_message(&mut app, "a-1", "first reply");
        push_test_user_message(&mut app, "u-2", "second prompt");

        let temp = TempDir::new().unwrap();
        let mut session = make_test_tui_session();
        session.projects_root = temp.path().join("projects");
        session.cwd = temp.path().join("work").to_string_lossy().to_string();
        write_owner_test_transcript(&app, &session);
        session.session_active_lock = rebon_session::try_acquire_session_active_lock(
            &session.projects_root,
            &session.cwd,
            &session.session_id,
        )
        .unwrap();
        assert!(replace_session_transcript_from_rows(
            &session,
            app.rebon_tui.transcript.rows()
        ));

        // No file-history snapshot was ever taken for this turn.
        let error = rewind_session_for_owner(
            &session_command_inputs_from_app(&app, session.ui_mode),
            &session,
            "u-2",
            rebon_session_host::RewindScopeWire::Both,
        )
        .unwrap_err();

        assert!(
            error.contains("before any change"),
            "unexpected refusal: {error}"
        );
        assert_eq!(
            loaded_transcript_for(&session).len(),
            3,
            "the conversation must be untouched after a refusal"
        );
    }

    #[test]
    fn apply_rewind_outcome_conversation_replaces_engine_transcript_and_prefills_input() {
        let mut app = AppState::new();
        push_test_user_message(&mut app, "u-1", "first prompt");
        push_test_assistant_message(&mut app, "a-1", "first reply");
        push_test_user_message(&mut app, "u-2", "second prompt");
        push_test_assistant_message(&mut app, "a-2", "second reply");

        let temp = TempDir::new().unwrap();
        let mut session = make_test_tui_session();
        session.projects_root = temp.path().join("projects");
        session.cwd = temp.path().join("work").to_string_lossy().to_string();
        let transcript_path = rebon_session::ensure_session_file_path(
            &session.projects_root,
            &session.cwd,
            &session.session_id,
        )
        .unwrap();
        let mut body = Vec::new();
        for entry in transcript_entries_from_rows(app.rebon_tui.transcript.rows()) {
            let mut raw = entry.raw;
            if let Some(object) = raw.as_object_mut() {
                object.insert("type".into(), serde_json::Value::String(entry.entry_type));
                object.insert("uuid".into(), serde_json::Value::String(entry.uuid));
                object.insert(
                    "parentUuid".into(),
                    entry
                        .parent_uuid
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            serde_json::to_writer(&mut body, &raw).unwrap();
            body.push(b'\n');
        }
        std::fs::write(&transcript_path, body).unwrap();
        session.session_active_lock = rebon_session::try_acquire_session_active_lock(
            &session.projects_root,
            &session.cwd,
            &session.session_id,
        )
        .unwrap();
        assert!(replace_session_transcript_from_rows(
            &session,
            app.rebon_tui.transcript.rows()
        ));
        app.rewind_dialog = Some(open_rewind_dialog(&app, &session));

        apply_rewind_outcome(
            &mut app,
            &mut session,
            1,
            rebon_picker::message_selector::RestoreOption::Conversation,
            None,
        );

        assert_eq!(app.input, "second prompt");
        assert_eq!(app.cursor_offset, "second prompt".len());
        assert_eq!(app.rebon_tui.transcript.rows().len(), 2);

        let entries = loaded_transcript_for(&session);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].uuid, "u-1");
        assert_eq!(entries[1].uuid, "a-1");
        assert_eq!(entries[0].parent_uuid, None);
        assert_eq!(entries[1].parent_uuid.as_deref(), Some("u-1"));
    }

    #[test]
    fn apply_rewind_outcome_summary_modes_commit_disk_engine_and_ui() {
        use rebon_picker::message_selector::RestoreOption;

        for (option, selected_index, expected_root) in [
            (RestoreOption::Summarize, 0, "u-1"),
            (RestoreOption::SummarizeUpTo, 1, "u-2"),
        ] {
            let mut app = AppState::new();
            push_test_user_message(&mut app, "u-1", "first prompt");
            push_test_assistant_message(&mut app, "a-1", "first reply");
            push_test_user_message(&mut app, "u-2", "second prompt");

            let temp = TempDir::new().unwrap();
            let mut session = make_test_tui_session();
            session.projects_root = temp.path().join("projects");
            session.cwd = temp.path().join("work").to_string_lossy().to_string();
            let transcript_path = rebon_session::ensure_session_file_path(
                &session.projects_root,
                &session.cwd,
                &session.session_id,
            )
            .unwrap();
            let mut body = Vec::new();
            for entry in transcript_entries_from_rows(app.rebon_tui.transcript.rows()) {
                let mut raw = entry.raw;
                let object = raw.as_object_mut().unwrap();
                object.insert("type".into(), serde_json::Value::String(entry.entry_type));
                object.insert("uuid".into(), serde_json::Value::String(entry.uuid));
                object.insert(
                    "parentUuid".into(),
                    entry
                        .parent_uuid
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null),
                );
                serde_json::to_writer(&mut body, &raw).unwrap();
                body.push(b'\n');
            }
            std::fs::write(&transcript_path, body).unwrap();
            session.session_active_lock = rebon_session::try_acquire_session_active_lock(
                &session.projects_root,
                &session.cwd,
                &session.session_id,
            )
            .unwrap();
            assert!(replace_session_transcript_from_rows(
                &session,
                app.rebon_tui.transcript.rows()
            ));

            apply_rewind_outcome(
                &mut app,
                &mut session,
                selected_index,
                option,
                Some("focus on first branch".into()),
            );

            let rows = app.rebon_tui.transcript.rows();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].uuid(), Some(expected_root));
            let rebon_tui::Message::System(system) = &rows[1] else {
                panic!("expected trailing system message");
            };
            assert!(system
                .content
                .as_deref()
                .expect("system content")
                .contains("Feedback: focus on first branch"));

            let entries = loaded_transcript_for(&session);
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].uuid, expected_root);
            assert_eq!(entries[0].parent_uuid, None);
            assert_eq!(entries[1].entry_type, "system");
            assert_eq!(entries[1].parent_uuid.as_deref(), Some(expected_root));
            let restarted = rebon_session::load_transcript_from_file(&transcript_path)
                .unwrap()
                .unwrap();
            assert_eq!(restarted.messages.len(), 2);
            assert_eq!(restarted.messages[1].uuid, entries[1].uuid);
            assert!(restarted.messages[1].raw["content"]
                .as_str()
                .unwrap()
                .contains("Feedback: focus on first branch"));
        }
    }

    #[test]
    fn summary_modes_use_disk_when_the_visible_replay_window_is_evicted() {
        use rebon_picker::message_selector::RestoreOption;

        for (label, option, expected_uuids) in [
            ("from", RestoreOption::Summarize, vec!["u-1", "a-1", "u-2"]),
            (
                "upto",
                RestoreOption::SummarizeUpTo,
                vec!["u-2", "a-2", "u-3"],
            ),
        ] {
            // The visible replay window has evicted every row except the selected
            // user. Durable summary must nevertheless retain canonical disk rows.
            let mut app = AppState::new();
            push_test_user_message(&mut app, "u-2", "selected prompt");

            let temp = TempDir::new().unwrap();
            let mut session = make_test_tui_session();
            session.projects_root = temp.path().join(format!("projects-{label}"));
            session.cwd = temp
                .path()
                .join(format!("work-{label}"))
                .to_string_lossy()
                .to_string();
            let transcript_path = rebon_session::ensure_session_file_path(
                &session.projects_root,
                &session.cwd,
                &session.session_id,
            )
            .unwrap();
            let canonical = [
                json!({"type":"user","uuid":"u-1","parentUuid":null,"timestamp":"2026-04-14T00:00:00.1Z","message":{"role":"user","content":[{"type":"text","text":"first"}]},"rawUser":{"nested":[1,2]}}),
                json!({"type":"assistant","uuid":"a-1","parentUuid":"u-1","timestamp":"2026-04-14T00:00:00.1Z","message":{"role":"assistant","content":[{"type":"text","text":"first reply"}]},"richTool":{"input":{"deep":true},"output":["kept"]}}),
                json!({"type":"user","uuid":"u-2","parentUuid":"a-1","timestamp":"2026-04-14T00:00:00.2Z","message":{"role":"user","content":[{"type":"text","text":"selected prompt"}]},"selectedRaw":{"keep":"yes"}}),
                json!({"type":"assistant","uuid":"a-2","parentUuid":"u-2","timestamp":"2026-04-14T00:00:00.2Z","message":{"role":"assistant","content":[{"type":"text","text":"later reply"}]},"toolUseResult":{"locations":[{"path":"src/main.rs","line":9}]}}),
                json!({"type":"user","uuid":"u-3","parentUuid":"a-2","timestamp":"2026-04-14T00:00:00.3Z","message":{"role":"user","content":[{"type":"text","text":"last"}]},"laterRaw":{"keep":true}}),
            ];
            std::fs::write(
                &transcript_path,
                canonical
                    .iter()
                    .map(serde_json::Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap();
            session.session_active_lock = rebon_session::try_acquire_session_active_lock(
                &session.projects_root,
                &session.cwd,
                &session.session_id,
            )
            .unwrap();
            assert!(replace_session_transcript_from_rows(
                &session,
                app.rebon_tui.transcript.rows()
            ));

            apply_rewind_outcome(&mut app, &mut session, 0, option, None);

            let engine = loaded_transcript_for(&session);
            assert_eq!(
                engine[..engine.len() - 1]
                    .iter()
                    .map(|entry| entry.uuid.as_str())
                    .collect::<Vec<_>>(),
                expected_uuids
            );
            assert_eq!(engine.last().unwrap().entry_type, "system");
            assert_eq!(app.rebon_tui.transcript.rows().len(), engine.len());
            let restarted = rebon_session::load_transcript_from_file(&transcript_path)
                .unwrap()
                .unwrap();
            assert_eq!(
                engine.iter().map(|entry| &entry.raw).collect::<Vec<_>>(),
                restarted
                    .messages
                    .iter()
                    .map(|entry| &entry.raw)
                    .collect::<Vec<_>>()
            );
            if label == "from" {
                assert_eq!(engine[0].raw["rawUser"]["nested"], json!([1, 2]));
                assert_eq!(engine[1].raw["richTool"]["output"], json!(["kept"]));
                assert_eq!(engine[2].raw["selectedRaw"]["keep"], "yes");
            } else {
                assert_eq!(engine[0].parent_uuid, None);
                assert_eq!(engine[0].raw["selectedRaw"]["keep"], "yes");
                assert_eq!(engine[1].raw["toolUseResult"]["locations"][0]["line"], 9);
                assert_eq!(engine[2].raw["laterRaw"]["keep"], true);
            }
            for pair in engine.windows(2) {
                assert_eq!(pair[1].parent_uuid.as_deref(), Some(pair[0].uuid.as_str()));
            }
        }
    }

    #[test]
    fn summary_stale_cas_leaves_visible_and_engine_projections_unchanged() {
        use rebon_picker::message_selector::RestoreOption;

        for (label, option) in [
            ("from", RestoreOption::Summarize),
            ("upto", RestoreOption::SummarizeUpTo),
        ] {
            let mut app = AppState::new();
            push_test_user_message(&mut app, "u-1", "first prompt");
            app.follow_transcript_tail = false;
            let visible_before = app.rebon_tui.transcript.rows().to_vec();

            let temp = TempDir::new().unwrap();
            let mut session = make_test_tui_session();
            session.projects_root = temp.path().join(format!("projects-{label}"));
            session.cwd = temp
                .path()
                .join(format!("work-{label}"))
                .to_string_lossy()
                .to_string();
            let transcript_path = rebon_session::ensure_session_file_path(
                &session.projects_root,
                &session.cwd,
                &session.session_id,
            )
            .unwrap();
            let original = json!({"type":"user","uuid":"u-1","parentUuid":null,"message":{"content":"first prompt"}}).to_string();
            std::fs::write(&transcript_path, original).unwrap();
            session.session_active_lock = rebon_session::try_acquire_session_active_lock(
                &session.projects_root,
                &session.cwd,
                &session.session_id,
            )
            .unwrap();
            assert!(replace_session_transcript_from_rows(
                &session,
                app.rebon_tui.transcript.rows()
            ));
            let engine_before = loaded_transcript_for(&session);
            let raced_bytes = [
                json!({"type":"user","uuid":"u-1","parentUuid":null,"message":{"content":"first prompt"}}),
                json!({"type":"assistant","uuid":"a-race","parentUuid":"u-1","message":{"content":"concurrent durable reply"}}),
            ]
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes();
            let hook_bytes = raced_bytes.clone();
            crate::session::commands::rewind::SUMMARY_BEFORE_COMMIT_HOOK.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move |path| {
                    std::fs::write(path, hook_bytes).unwrap();
                }));
            });

            apply_rewind_outcome(&mut app, &mut session, 0, option, None);

            assert_eq!(std::fs::read(&transcript_path).unwrap(), raced_bytes);
            assert_eq!(app.rebon_tui.transcript.rows(), visible_before.as_slice());
            assert_eq!(
                loaded_transcript_for(&session)
                    .iter()
                    .map(|entry| (&entry.uuid, &entry.raw))
                    .collect::<Vec<_>>(),
                engine_before
                    .iter()
                    .map(|entry| (&entry.uuid, &entry.raw))
                    .collect::<Vec<_>>()
            );
            assert!(!app.follow_transcript_tail);
            assert!(app.transcript_measure_cache.is_empty());
            assert!(!session
                .engine_half
                .projection_invalid
                .load(std::sync::atomic::Ordering::Acquire));
        }
    }

    #[test]
    fn apply_summary_modes_mark_projection_invalid_when_engine_replacement_is_refused() {
        use rebon_picker::message_selector::RestoreOption;

        for (option, selected_index) in [
            (RestoreOption::Summarize, 0),
            (RestoreOption::SummarizeUpTo, 1),
        ] {
            let mut app = AppState::new();
            push_test_user_message(&mut app, "u-1", "first prompt");
            push_test_assistant_message(&mut app, "a-1", "first reply");
            push_test_user_message(&mut app, "u-2", "second prompt");
            let visible_before = app.rebon_tui.transcript.rows().to_vec();

            let temp = TempDir::new().unwrap();
            let mut session = make_test_tui_session();
            session.projects_root = temp.path().join("projects");
            session.cwd = temp.path().join("work").to_string_lossy().to_string();
            let transcript_path = rebon_session::ensure_session_file_path(
                &session.projects_root,
                &session.cwd,
                &session.session_id,
            )
            .unwrap();
            let values = [
                json!({"type":"user","uuid":"u-1","parentUuid":null,"message":{"content":"first prompt"},"rich":{"preserved":true}}),
                json!({"type":"assistant","uuid":"a-1","parentUuid":"u-1","message":{"content":"first reply"}}),
                json!({"type":"user","uuid":"u-2","parentUuid":"a-1","message":{"content":"second prompt"},"rich":{"preserved":true}}),
            ];
            let body = values
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(&transcript_path, body).unwrap();
            session.session_active_lock = rebon_session::try_acquire_session_active_lock(
                &session.projects_root,
                &session.cwd,
                &session.session_id,
            )
            .unwrap();
            assert!(replace_session_transcript_from_rows(
                &session,
                app.rebon_tui.transcript.rows()
            ));
            let replay = session
                .engine_half
                .handler
                .state()
                .take_transcript_for_replay(&session.session_id)
                .expect("active replay handoff");
            let engine_before = replay.entries.clone();

            apply_rewind_outcome(&mut app, &mut session, selected_index, option, None);

            assert!(session
                .engine_half
                .projection_invalid
                .load(std::sync::atomic::Ordering::Acquire));
            assert_eq!(
                replay
                    .entries
                    .iter()
                    .map(|entry| (&entry.uuid, &entry.raw))
                    .collect::<Vec<_>>(),
                engine_before
                    .iter()
                    .map(|entry| (&entry.uuid, &entry.raw))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                &app.rebon_tui.transcript.rows()[..visible_before.len()],
                visible_before.as_slice()
            );
            let guidance = app
                .rebon_tui
                .transcript
                .rows()
                .last()
                .and_then(|row| match row {
                    rebon_tui::Message::System(system) => system.content.as_deref(),
                    _ => None,
                })
                .expect("restart guidance");
            assert!(guidance.contains("Restart or resume"));
            let committed = rebon_session::load_transcript_from_file(&transcript_path)
                .unwrap()
                .unwrap();
            assert_eq!(committed.messages.last().unwrap().entry_type, "system");
            assert_eq!(committed.messages[0].raw["rich"]["preserved"], true);
        }
    }

    #[test]
    fn apply_rewind_outcome_both_refuses_unverified_transcript_fallback() {
        let tempdir = TempDir::new().expect("tempdir");
        let file_path = tempdir.path().join("rewind.txt");
        std::fs::write(&file_path, "new content").expect("write final content");

        let mut app = AppState::new();
        push_test_user_message(&mut app, "u-1", "first prompt");
        push_test_assistant_message(&mut app, "a-1", "first reply");
        push_test_user_message(&mut app, "u-2", "second prompt");
        push_test_tool_use_message(
            &mut app,
            "a-2",
            &file_path,
            Some("old content"),
            "new content",
        );

        let mut session = make_test_tui_session();
        assert!(replace_session_transcript_from_rows(
            &session,
            app.rebon_tui.transcript.rows()
        ));
        app.rewind_dialog = Some(open_rewind_dialog(&app, &session));

        apply_rewind_outcome(
            &mut app,
            &mut session,
            1,
            rebon_picker::message_selector::RestoreOption::Both,
            None,
        );

        assert_eq!(
            std::fs::read_to_string(&file_path).expect("unchanged file"),
            "new content"
        );
        assert!(app.input.is_empty());

        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::System(system) = rows.last().expect("trailing system message")
        else {
            panic!("expected trailing system message");
        };
        assert!(system
            .content
            .as_deref()
            .expect("system content")
            .contains("Combined rewind was not started"));
    }

    #[test]
    fn apply_rewind_outcome_code_refuses_unverified_transcript_fallback() {
        let tempdir = TempDir::new().expect("tempdir");
        let file_path = tempdir.path().join("demo.txt");
        std::fs::write(&file_path, "manual content").expect("write manual content");

        let mut app = AppState::new();
        push_test_user_message(&mut app, "u-1", "first prompt");
        push_test_tool_use_message(
            &mut app,
            "a-1",
            &file_path,
            Some("old content"),
            "new content",
        );

        let mut session = make_test_tui_session();
        apply_rewind_outcome(
            &mut app,
            &mut session,
            0,
            rebon_picker::message_selector::RestoreOption::Code,
            None,
        );

        assert_eq!(
            std::fs::read_to_string(&file_path).expect("unchanged file"),
            "manual content"
        );
        let rows = app.rebon_tui.transcript.rows();
        let rebon_tui::Message::System(system) = rows.last().expect("trailing system") else {
            panic!("expected system message");
        };
        assert!(system
            .content
            .as_deref()
            .expect("system content")
            .contains("Failed to restore code:"));
    }
}
