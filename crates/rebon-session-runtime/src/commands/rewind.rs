//! Rewinding a session with no screen attached: which rows count as a
//! turn, the durable compare-and-swap against the canonical transcript,
//! the code restore that can follow it, and the report an owner sends
//! back to the client that asked.
//!
//! The terminal keeps its half in the binary's `tui::runner::rewind`: the
//! picker, choosing a summary mode, and applying an outcome to the
//! rendered transcript.

use std::time::SystemTime;

use rebon_types::format_system_time_iso_ms;

use crate::EngineSession;

fn user_message_text(user: &rebon_render::transcript_row::UserMessage) -> String {
    user.message
        .content
        .iter()
        .filter_map(|block| {
            if let rebon_render::transcript_row::UserContentBlock::Text(t) = block {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn user_message_last_text(user: &rebon_render::transcript_row::UserMessage) -> String {
    user.message
        .content
        .iter()
        .rev()
        .find_map(|block| {
            if let rebon_render::transcript_row::UserContentBlock::Text(t) = block {
                Some(t.text.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn is_synthetic_user_message(user: &rebon_render::transcript_row::UserMessage, text: &str) -> bool {
    user.uuid.starts_with("u-interrupt-")
        || text.starts_with("[Request interrupted")
        || text.starts_with("[Request cancelled")
}

pub fn selectable_rewind_user(
    row: &rebon_render::transcript_row::Message,
) -> Option<(String, String, String)> {
    let rebon_render::transcript_row::Message::User(user) = row else {
        return None;
    };
    let filter_text = user_message_last_text(user);
    let input = rebon_picker::message_selector::UserMessageFilterInput {
        is_user_type: true,
        is_tool_result: matches!(
            user.message.content.first(),
            Some(rebon_render::transcript_row::UserContentBlock::ToolResult(
                _
            ))
        ),
        is_synthetic: is_synthetic_user_message(user, &filter_text),
        is_meta: user.is_meta == Some(true),
        is_compact_summary: user.is_compact_summary == Some(true),
        is_visible_in_transcript_only: user.is_visible_in_transcript_only == Some(true),
        message_text: filter_text.clone(),
    };
    rebon_picker::message_selector::selectable_user_message(&input)
        .then(|| (user.uuid.clone(), user_message_text(user), filter_text))
}

pub fn apply_code_rewind(
    rows: &[rebon_render::transcript_row::Message],
    transcript_boundary: usize,
    store: &rebon_session::FileHistoryStore,
    active_lock: Option<&rebon_session::SessionActiveLock>,
) -> anyhow::Result<rebon_session::FileHistoryApplyReport> {
    let active_lock = active_lock
        .ok_or_else(|| anyhow::anyhow!("code restore requires this live session's active lock"))?;
    let row = rows
        .get(transcript_boundary)
        .ok_or_else(|| anyhow::anyhow!("selected rewind boundary is unavailable"))?;
    let message_id = selectable_rewind_user(row)
        .map(|(id, _, _)| id)
        .ok_or_else(|| anyhow::anyhow!("selected rewind boundary is not a user turn"))?;
    store
        .apply_snapshot(&message_id, active_lock)
        .map_err(anyhow::Error::from)
}

fn transcript_entry_from_row(
    row: &rebon_render::transcript_row::Message,
    parent_uuid: Option<String>,
) -> Option<rebon_session::TranscriptEntry> {
    let uuid = row.uuid()?.to_string();
    let (entry_type, timestamp) = match row {
        rebon_render::transcript_row::Message::User(user) => ("user", user.timestamp.clone()),
        rebon_render::transcript_row::Message::Assistant(assistant) => {
            ("assistant", assistant.timestamp.clone())
        }
        rebon_render::transcript_row::Message::Attachment(attachment) => {
            ("attachment", attachment.timestamp.clone())
        }
        rebon_render::transcript_row::Message::System(system) => {
            ("system", system.timestamp.clone())
        }
        rebon_render::transcript_row::Message::Unknown => return None,
    };
    let raw = serde_json::to_value(row).ok()?;
    Some(rebon_session::TranscriptEntry {
        entry_type: entry_type.to_string(),
        uuid,
        parent_uuid,
        timestamp: Some(timestamp),
        raw,
    })
}

pub fn transcript_entries_from_rows(
    rows: &[rebon_render::transcript_row::Message],
) -> Vec<rebon_session::TranscriptEntry> {
    let mut entries = Vec::with_capacity(rows.len());
    let mut parent_uuid: Option<String> = None;
    for row in rows {
        let Some(entry) = transcript_entry_from_row(row, parent_uuid.clone()) else {
            continue;
        };
        parent_uuid = Some(entry.uuid.clone());
        entries.push(entry);
    }
    entries
}

pub fn replace_session_transcript_from_rows(
    session: &EngineSession,
    rows: &[rebon_render::transcript_row::Message],
) -> bool {
    let state = session.server_state.clone();
    state.replace_transcript_entries(&session.session_id, transcript_entries_from_rows(rows))
}

#[derive(Debug)]
pub enum DurableConversationRewind {
    Committed {
        receipt: rebon_session::RewindConversationReceipt,
        rows: Vec<rebon_render::transcript_row::Message>,
    },
    CommittedProjectionFailed {
        receipt: rebon_session::RewindConversationReceipt,
        error: String,
    },
}

fn project_committed_rewind_rows(
    canonical_rows: &[rebon_render::transcript_row::Message],
    selected_user_uuid: &str,
) -> Result<Vec<rebon_render::transcript_row::Message>, String> {
    let boundary = canonical_rows
        .iter()
        .position(|row| row.uuid() == Some(selected_user_uuid))
        .ok_or_else(|| {
            format!(
                "selected canonical row {selected_user_uuid} is unavailable in the captured TUI projection"
            )
        })?;
    Ok(canonical_rows[..boundary].to_vec())
}

pub fn committed_projection_outcome(
    receipt: rebon_session::RewindConversationReceipt,
    projection: Result<Vec<rebon_render::transcript_row::Message>, String>,
) -> DurableConversationRewind {
    match projection {
        Ok(rows) => DurableConversationRewind::Committed { receipt, rows },
        Err(error) => DurableConversationRewind::CommittedProjectionFailed { receipt, error },
    }
}

pub fn durable_conversation_rewind(
    session: &EngineSession,
    selected_user_uuid: &str,
    canonical_rows: &[rebon_render::transcript_row::Message],
) -> Result<DurableConversationRewind, String> {
    let transcript_path = rebon_session::transcript_file_path(
        &session.projects_root,
        &session.cwd,
        &session.session_id,
    );
    let canonical = rebon_session::load_session_history(&transcript_path)
        .map_err(|error| format!("failed to load canonical transcript: {error}"))?;
    let turn = canonical
        .turns
        .iter()
        .find(|turn| turn.user_uuid == selected_user_uuid)
        .ok_or_else(|| "selected rewind turn is no longer canonical".to_string())?;
    let request = rebon_session::RewindConversationRequest {
        mutation_id: format!(
            "tui-{selected_user_uuid}-{}",
            rebon_types::wall_clock_ms_u128()
        ),
        session_id: session.session_id.clone(),
        cwd: session.cwd.clone(),
        selected_user_uuid: turn.user_uuid.clone(),
        boundary_parent_uuid: turn.parent_uuid.clone(),
        selected_prompt: turn.prompt.clone(),
        expected_sha256: canonical.stamp.sha256,
        expected_source_head_uuid: canonical.source_head_uuid,
    };
    let active_lock = session
        .session_active_lock
        .as_ref()
        .ok_or_else(|| "the TUI no longer owns the active-session lock".to_string())?;
    let receipt = rebon_session::rewind_conversation_locked(
        &session.projects_root,
        &transcript_path,
        &request,
        active_lock,
    )
    .map_err(|error| error.to_string())?;
    Ok(committed_projection_outcome(
        receipt,
        project_committed_rewind_rows(canonical_rows, selected_user_uuid),
    ))
}

/// What an owner-side rewind did, for the client that asked for it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OwnerRewindReport {
    /// Turns left in the conversation afterwards. `None` when only files were
    /// restored and the transcript was deliberately left alone.
    pub turns: Option<usize>,
    pub restored_files: usize,
    pub skipped_files: usize,
    pub conflicts: usize,
    pub errors: usize,
}

impl OwnerRewindReport {
    /// One line for whoever asked. Says plainly when a combined rewind got
    /// half-way, because a partial result the user is not told about is worse
    /// than a failure.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(turns) = self.turns {
            parts.push(format!("Conversation rewound; {turns} turn(s) remain."));
        }
        if self.restored_files > 0 || self.skipped_files > 0 || self.errors > 0 {
            parts.push(format!(
                "Code restore changed {} file(s), skipped {} ({} late conflict(s), {} error(s)).",
                self.restored_files, self.skipped_files, self.conflicts, self.errors
            ));
        }
        if self.turns.is_some() && (self.restored_files > 0 || self.errors > 0) {
            parts.push("The combined action was sequential, not atomic.".to_string());
        }
        if parts.is_empty() {
            "Nothing was changed.".to_string()
        } else {
            parts.join(" ")
        }
    }
}

/// Rewind on behalf of a client, from the process that owns the session.
///
/// The compare-and-swap this rewrites the transcript with is only sound
/// against the chain the owner holds, so a client doing it locally would
/// leave the owner's in-memory chain pointing at uuids that no longer exist —
/// which is why a mirrored terminal refuses to rewind at all today. This is
/// the same durable path the local one takes, minus the interface work: the
/// caller's own transcript is what it renders from, and it already knows
/// which turn it picked.
pub fn rewind_session_for_owner(
    inputs: &crate::commands::SessionCommandInputs,
    session: &EngineSession,
    user_message_uuid: &str,
    scope: rebon_session_host::RewindScopeWire,
) -> Result<OwnerRewindReport, String> {
    use rebon_session_host::RewindScopeWire;

    let rows = inputs.rows.to_vec();
    let boundary = rows
        .iter()
        .position(|row| {
            selectable_rewind_user(row).is_some_and(|(uuid, _, _)| uuid == user_message_uuid)
        })
        .ok_or_else(|| format!("turn {user_message_uuid} is no longer in this conversation"))?;
    let file_history_store = session.engine_half.runtime.file_history_tracker.store();

    if scope == RewindScopeWire::Code {
        let report = apply_code_rewind(
            &rows,
            boundary,
            &file_history_store,
            session.session_active_lock.as_ref(),
        )
        .map_err(|error| error.to_string())?;
        return Ok(OwnerRewindReport {
            turns: None,
            restored_files: report.restored,
            skipped_files: report.skipped,
            conflicts: report.conflicts.len(),
            errors: report.errors.len(),
        });
    }

    // Refuse a combined rewind up front when the code half cannot land
    // cleanly, rather than truncating the conversation and then discovering
    // the files cannot follow it.
    if scope == RewindScopeWire::Both
        && !matches!(
            file_history_store.restore_capability(user_message_uuid),
            rebon_session::FileRestoreCapability::Clean(_)
        )
    {
        return Err(
            "combined rewind refused before any change: the code half is unavailable or conflicted"
                .to_string(),
        );
    }

    let committed = durable_conversation_rewind(session, user_message_uuid, &rows)?;
    let (receipt, durable_rows) = match committed {
        DurableConversationRewind::Committed { receipt, rows } => (receipt, rows),
        DurableConversationRewind::CommittedProjectionFailed { receipt, error } => {
            session
                .engine_half
                .projection_invalid
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(format!(
                "the rewind is committed on disk ({} turn(s) remain), but this session's projection could not be rebuilt: {error}. Resume the session before sending another turn.",
                receipt.canonical_history.turns.len()
            ));
        }
    };
    if !replace_session_transcript_from_rows(session, &durable_rows) {
        session
            .engine_half
            .projection_invalid
            .store(true, std::sync::atomic::Ordering::Release);
        return Err(
            "the rewind is committed on disk, but this session's engine transcript could not be refreshed; resume the session before sending another turn."
                .to_string(),
        );
    }
    session
        .engine_half
        .projection_invalid
        .store(false, std::sync::atomic::Ordering::Release);

    let mut result = OwnerRewindReport {
        turns: Some(receipt.canonical_history.turns.len()),
        ..OwnerRewindReport::default()
    };
    if scope == RewindScopeWire::Both {
        // Sequential, and said so in the report: the conversation is already
        // committed, so a failure here is a partial result rather than a
        // reason to claim nothing happened.
        match apply_code_rewind(
            &rows,
            boundary,
            &file_history_store,
            session.session_active_lock.as_ref(),
        ) {
            Ok(report) => {
                result.restored_files = report.restored;
                result.skipped_files = report.skipped;
                result.conflicts = report.conflicts.len();
                result.errors = report.errors.len();
            }
            Err(error) => {
                return Err(format!(
                    "the conversation rewind is committed, but the code restore that followed it failed: {error}. This is a partial, non-atomic result."
                ))
            }
        }
    }
    Ok(result)
}

/// Turn the file-history diff counts into the shape the message picker
/// renders. The picker is a screen, but counting is not.
pub fn file_history_diff_stats_to_picker(
    stats: rebon_session::FileHistoryDiffStats,
) -> rebon_picker::message_selector::DiffStats {
    rebon_picker::message_selector::DiffStats {
        files_changed: stats.files_changed,
        insertions: stats.insertions,
        deletions: stats.deletions,
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Deterministic CAS-race injection used only by integration-boundary tests.
    #[doc(hidden)]
    pub static SUMMARY_BEFORE_COMMIT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce(&std::path::Path)>>> =
        std::cell::RefCell::new(None);
}

#[cfg(any(test, feature = "test-support"))]
fn run_summary_before_commit_hook(transcript_path: &std::path::Path) {
    SUMMARY_BEFORE_COMMIT_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook(transcript_path);
        }
    });
}

/// Summarize a conversation at a chosen turn and commit it to the canonical
/// transcript, returning the receipt and the rows projected off it.
///
/// The projection is prepared only after the disk commit, so what a screen
/// draws afterwards comes from the receipt rather than from rows it may
/// already have evicted.
pub fn durable_conversation_summary(
    session: &EngineSession,
    selected_user_uuid: &str,
    mode: rebon_session::SummarizeConversationMode,
    feedback: Option<&str>,
) -> Result<
    (
        rebon_session::SummarizeConversationReceipt,
        Result<Vec<rebon_render::transcript_row::Message>, String>,
    ),
    (bool, String),
> {
    let transcript_path = rebon_session::transcript_file_path(
        &session.projects_root,
        &session.cwd,
        &session.session_id,
    );
    let history = rebon_session::load_session_history(&transcript_path).map_err(|error| {
        (
            false,
            format!("failed to load canonical transcript: {error}"),
        )
    })?;
    let turn = history
        .turns
        .iter()
        .find(|turn| turn.user_uuid == selected_user_uuid)
        .ok_or_else(|| {
            (
                false,
                "selected summary turn is no longer canonical".to_string(),
            )
        })?;
    let loaded = rebon_session::load_transcript_from_file(&transcript_path)
        .map_err(|error| {
            (
                false,
                format!("failed to load canonical transcript: {error}"),
            )
        })?
        .ok_or_else(|| (false, "canonical transcript is empty".to_string()))?;
    let selected_index = loaded
        .messages
        .iter()
        .position(|entry| entry.uuid == selected_user_uuid)
        .ok_or_else(|| {
            (
                false,
                "selected summary turn is no longer canonical".to_string(),
            )
        })?;
    let dropped = match mode {
        rebon_session::SummarizeConversationMode::FromSelected => {
            loaded.messages.len().saturating_sub(selected_index + 1)
        }
        rebon_session::SummarizeConversationMode::UpToSelected => selected_index,
    };
    let feedback_note = feedback
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!(" Feedback: {value}"))
        .unwrap_or_default();
    let (uuid, content) = match mode {
        rebon_session::SummarizeConversationMode::FromSelected => (
            format!("s-rewind-summarize-{}", rebon_types::wall_clock_ms_u128()),
            format!("Rewound: {dropped} messages after this point were summarized.{feedback_note}"),
        ),
        rebon_session::SummarizeConversationMode::UpToSelected => (
            format!(
                "s-rewind-summarize-up-to-{}",
                rebon_types::wall_clock_ms_u128()
            ),
            format!("Rewound: {dropped} preceding messages were summarized.{feedback_note}"),
        ),
    };
    let note = rebon_render::transcript_row::Message::System(
        rebon_render::transcript_row::SystemMessage {
            uuid,
            timestamp: format_system_time_iso_ms(SystemTime::now()),
            subtype: "info".into(),
            content: Some(content),
            level: Some(rebon_render::transcript_row::SystemLevel::Info),
            is_meta: None,
        },
    );
    let request = rebon_session::SummarizeConversationRequest {
        mutation_id: format!(
            "tui-summary-{selected_user_uuid}-{}",
            rebon_types::wall_clock_ms_u128()
        ),
        session_id: session.session_id.clone(),
        cwd: session.cwd.clone(),
        selected_user_uuid: selected_user_uuid.to_string(),
        selected_parent_uuid: turn.parent_uuid.clone(),
        expected_sha256: history.stamp.sha256,
        expected_source_head_uuid: history.source_head_uuid,
        mode,
        note_raw: serde_json::to_value(note).map_err(|error| (false, error.to_string()))?,
    };
    let active_lock = session.session_active_lock.as_ref().ok_or_else(|| {
        (
            false,
            "the TUI no longer owns the active-session lock".to_string(),
        )
    })?;
    // The binary's rewind tests inject the CAS race through this hook, so
    // the call site carries the same gate the hook does.
    #[cfg(any(test, feature = "test-support"))]
    run_summary_before_commit_hook(&transcript_path);
    let receipt = rebon_session::summarize_conversation_locked(
        &session.projects_root,
        &transcript_path,
        &request,
        active_lock,
    )
    .map_err(|error| {
        let committed = matches!(
            error,
            rebon_session::RewindConversationError::CommittedRecoveryRequired(_)
        );
        (committed, error.to_string())
    })?;
    // Projection is deliberately prepared only after the disk commit. The
    // receipt's opaque raw values, not evictable TUI rows, are authoritative.
    let rows = receipt
        .entries
        .iter()
        .map(|entry| {
            serde_json::from_value::<rebon_render::transcript_row::Message>(entry.raw.clone())
                .map_err(|error| format!("could not project committed row {}: {error}", entry.uuid))
        })
        .collect::<Result<Vec<_>, _>>();
    Ok((receipt, rows))
}
