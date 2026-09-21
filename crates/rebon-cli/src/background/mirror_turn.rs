//! What the persisted transcript says about a mirrored turn, and whether the
//! rows already on screen provably stand for it.
//!
//! A mirror draws a turn twice over: once from the owner's stream, as it
//! arrives, and once from the transcript the owner writes. Deciding which of
//! those two the reader should end up with is this module's whole job —
//! reading a turn out of the persisted entries, asking whether the projection
//! covers it, keeping the covered set honest across a rewind, and merging the
//! authoritative rows into what is on screen without moving a row the inline
//! scrollback has already printed.
//!
//! None of that needs a screen. It reads `rebon_session::TranscriptEntry`,
//! answers in `rebon_tui::Message` and in plain booleans, and touches the
//! mirror's own [`crate::background::RemoteBackgroundAttachment`] — never an
//! `AppState`. Committing an answer to a terminal is
//! `crate::tui::runner::remote_background_attachment`'s business.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::time::Instant;

#[derive(Debug, Default)]
pub(crate) struct PersistedRemoteTurn {
    pub(crate) boundary_found: bool,
    pub(crate) next_user_found: bool,
    pub(crate) assistant_text: String,
    pub(crate) thinking_text: String,
    pub(crate) tool_call_ids: HashSet<String>,
    /// Assistant entries inside the turn, in file order. These are the rows
    /// a locally settled turn covers instead of splicing.
    pub(crate) assistant_uuids: Vec<String>,
    /// An assistant entry carried a block the stream projection cannot
    /// represent (an image, an unknown type). Covering such a turn would
    /// hide content this terminal never drew, so it always falls back to
    /// the splice.
    pub(crate) has_unprojectable_content: bool,
}

fn entry_is_meta_user(entry: &rebon_session::TranscriptEntry) -> bool {
    entry
        .raw
        .get("isMeta")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || entry
            .raw
            .get("runtimeContext")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || entry
            .raw
            .get("message")
            .and_then(|message| message.get("isMeta"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

fn entry_is_user_turn(entry: &rebon_session::TranscriptEntry) -> bool {
    if entry.entry_type != "user" || entry_is_meta_user(entry) {
        return false;
    }
    let Some(content) = entry
        .raw
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return true;
    };
    content.as_array().map_or(true, |blocks| {
        blocks.is_empty()
            || blocks.iter().any(|block| {
                block.get("type").and_then(serde_json::Value::as_str) != Some("tool_result")
            })
    })
}

pub(crate) fn last_remote_user_turn_uuid(
    entries: &[rebon_session::TranscriptEntry],
) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|entry| entry_is_user_turn(entry))
        .map(|entry| entry.uuid.clone())
}

pub(crate) fn persisted_remote_turn(
    entries: &[rebon_session::TranscriptEntry],
    user_uuid: &str,
) -> PersistedRemoteTurn {
    let Some(boundary) = entries.iter().position(|entry| entry.uuid == user_uuid) else {
        return PersistedRemoteTurn::default();
    };
    let mut turn = PersistedRemoteTurn {
        boundary_found: true,
        ..PersistedRemoteTurn::default()
    };
    for entry in &entries[boundary + 1..] {
        let content = entry
            .raw
            .get("message")
            .and_then(|message| message.get("content"));
        if entry_is_user_turn(entry) {
            turn.next_user_found = true;
            break;
        }
        if entry.entry_type == "assistant" {
            turn.assistant_uuids.push(entry.uuid.clone());
            if let Some(text) = content.and_then(serde_json::Value::as_str) {
                turn.assistant_text.push_str(text);
            }
        }
        let Some(blocks) = content.and_then(serde_json::Value::as_array) else {
            continue;
        };
        for block in blocks {
            let kind = block.get("type").and_then(serde_json::Value::as_str);
            if entry.entry_type == "assistant"
                && !matches!(
                    kind,
                    Some("text" | "thinking" | "redacted_thinking" | "tool_use" | "tool_result")
                )
            {
                turn.has_unprojectable_content = true;
            }
            match kind {
                Some("text") if entry.entry_type == "assistant" => {
                    if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
                        turn.assistant_text.push_str(text);
                    }
                }
                Some("thinking") | Some("redacted_thinking") if entry.entry_type == "assistant" => {
                    if let Some(text) = block
                        .get("thinking")
                        .or_else(|| block.get("text"))
                        .and_then(serde_json::Value::as_str)
                    {
                        turn.thinking_text.push_str(text);
                    }
                }
                Some("tool_use") => {
                    if let Some(id) = block.get("id").and_then(serde_json::Value::as_str) {
                        turn.tool_call_ids.insert(id.to_string());
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = block
                        .get("tool_use_id")
                        .or_else(|| block.get("toolUseId"))
                        .and_then(serde_json::Value::as_str)
                    {
                        turn.tool_call_ids.insert(id.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    turn
}

pub(crate) fn remote_turn_is_absorbed(
    entries: &[rebon_session::TranscriptEntry],
    remote: &crate::background::RemoteBackgroundAttachment,
    transcript_fingerprint: u64,
    terminal_turn: bool,
) -> bool {
    if !remote.awaiting_overlay_absorption {
        return false;
    }
    let Some(user_uuid) = remote.current_turn_user_uuid.as_deref() else {
        return false;
    };
    let turn = persisted_remote_turn(entries, user_uuid);
    if !turn.boundary_found {
        return false;
    }
    if turn.next_user_found || terminal_turn {
        return true;
    }
    if remote.initial_overlay_fingerprint == Some(transcript_fingerprint) {
        return false;
    }

    let has_projection = !remote.current_turn_projected_text.is_empty()
        || !remote.current_turn_projected_thinking.is_empty()
        || !remote.current_turn_visible_tool_call_ids.is_empty();
    has_projection
        && (remote.current_turn_projected_text.is_empty()
            || turn
                .assistant_text
                .starts_with(&remote.current_turn_projected_text))
        && (remote.current_turn_projected_thinking.is_empty()
            || turn
                .thinking_text
                .starts_with(&remote.current_turn_projected_thinking))
        && remote
            .current_turn_visible_tool_call_ids
            .is_subset(&turn.tool_call_ids)
}

/// Whether a session update is a tool event for a turn this terminal has
/// already settled — its card is committed, so the update has nothing on
/// screen to land on and must not re-open a live overlay card.
pub(crate) fn update_targets_settled_tool(
    remote: &crate::background::RemoteBackgroundAttachment,
    update: &rebon_types::SessionUpdate,
) -> bool {
    let (rebon_types::SessionUpdate::ToolCall { tool_call_id, .. }
    | rebon_types::SessionUpdate::ToolCallUpdate { tool_call_id, .. }) = update
    else {
        return false;
    };
    remote
        .last_settled_turn
        .as_ref()
        .is_some_and(|settled| settled.tool_call_ids.contains(tool_call_id))
}

/// Whether the projection provably contains everything the persisted turn
/// says happened.
///
/// Note the direction: `remote_turn_is_absorbed` asks the opposite question
/// (has the file caught *up to* the projection). The two coexist on purpose
/// — this one gates covering persisted rows instead of splicing them, and
/// covering on the weaker check would hide content this terminal never
/// rendered. When it fails, the caller falls back to the splice: the
/// authoritative rows print even if a flushed slab already showed part of
/// them.
pub(crate) fn projection_covers_persisted_turn(
    turn: &PersistedRemoteTurn,
    projected_text: &str,
    projected_thinking: &str,
    projected_tool_call_ids: &HashSet<String>,
) -> bool {
    // An empty string is a prefix of everything; demand watched evidence so
    // an untouched projection cannot cover a turn it never saw.
    //
    // Thinking is deliberately not a dimension. Providers re-chunk and
    // re-join reasoning between the stream and the file — o-series
    // summaries arrive as parts and are persisted joined with blank lines —
    // so a byte-prefix check on thinking fails on healthy turns and
    // demotes them to the splice, which is the duplicate. Text and tool
    // ids are delivered verbatim on both sides and carry the fidelity.
    let watched_something = !projected_text.is_empty()
        || !projected_thinking.is_empty()
        || !projected_tool_call_ids.is_empty();
    watched_something
        && !turn.has_unprojectable_content
        && projected_text.starts_with(&turn.assistant_text)
        && turn.tool_call_ids.is_subset(projected_tool_call_ids)
}

/// Every tool id the projection accounted for: the ones it drew and the
/// ones it deliberately hid.
pub(crate) fn projected_tool_ids(
    remote: &crate::background::RemoteBackgroundAttachment,
) -> HashSet<String> {
    remote
        .current_turn_visible_tool_call_ids
        .union(&remote.remote_hidden_tool_call_ids)
        .cloned()
        .collect()
}

/// Drop coverage for entries a rewind removed from the file — and, with
/// them, a settled-turn snapshot whose boundary is gone. A re-sent prompt
/// can reuse that boundary position with different content, and covering
/// new entries against the stale projection would hide them.
pub(crate) fn prune_stale_coverage(
    remote: &mut crate::background::RemoteBackgroundAttachment,
    persisted_uuids: &HashSet<String>,
) {
    remote
        .covered_persisted_uuids
        .retain(|uuid| persisted_uuids.contains(uuid));
    if remote
        .last_settled_turn
        .as_ref()
        .is_some_and(|settled| !persisted_uuids.contains(&settled.user_uuid))
    {
        remote.last_settled_turn = None;
    }
}

/// Cover the late entries of the last locally settled turn.
///
/// JSONL can file a settled turn's final entries after the settle; while
/// the kept snapshot still contains them, they are content the settle
/// already committed, and splicing them in would be the second print. The
/// snapshot drops once the next user turn is on disk. This walk is separate
/// from the in-flight turn's walk in `refresh_remote_transcript` — the two
/// turns have different user boundaries, and folding them into one walk is
/// how that distinction gets lost.
pub(crate) fn cover_settled_turn_entries(
    remote: &mut crate::background::RemoteBackgroundAttachment,
    entries: &[rebon_session::TranscriptEntry],
) {
    let Some(settled) = remote.last_settled_turn.as_ref() else {
        return;
    };
    let turn = persisted_remote_turn(entries, &settled.user_uuid);
    if !turn.boundary_found {
        return;
    }
    if projection_covers_persisted_turn(
        &turn,
        &settled.assistant_text,
        &settled.thinking_text,
        &settled.tool_call_ids,
    ) {
        remote
            .covered_persisted_uuids
            .extend(turn.assistant_uuids.iter().cloned());
    }
    if turn.next_user_found {
        remote.last_settled_turn = None;
    }
}

/// Rebuild the transcript around what is on screen already.
///
/// Rows keep the order they have. The inline scrollback has printed them in
/// that order and cannot take any of them back, so a row that is moved is a
/// row that gets printed twice — which is what happened to every local
/// notice the moment a remote turn landed after it. A persisted row that is
/// on screen is replaced by its fresh copy in place; one that was persisted
/// before and is gone now (a rewind) is dropped; a local row that is not a
/// system row is a projection the persisted rows now stand for, and is
/// dropped too. Persisted rows that are new go where the first dropped
/// projection stood — that is what they replace — or at the end.
///
/// Two exceptions keep a watched turn from printing twice. A local
/// `partial-*` row is kept while it stands for persisted content: forever
/// once its turn settled, and provisionally while its turn is still
/// watched live. A turn that falls to the splice drops its unsettled slabs
/// like any other projection — keeping them beside the persisted rows they
/// duplicate would be two prints of the same content in the store. And a
/// persisted row in `skipped_persisted_uuids` is content local rows
/// already stand for (or the overlay still streams): splicing it in as new
/// is the second print, so it is skipped — though one already on screen
/// keeps refreshing in place.
pub(crate) struct MergeStreamingContext<'a> {
    /// Persisted entry uuids that must not splice in as new rows: covered
    /// by settled local rows, or withheld while their turn still streams.
    pub(crate) skipped_persisted_uuids: &'a std::collections::HashSet<String>,
    /// Local `partial-*` rows that belong to settled turns and stand for
    /// covered entries permanently.
    pub(crate) settled_local_row_uuids: &'a std::collections::HashSet<String>,
    /// Whether unsettled `partial-*` rows survive this refresh — true only
    /// while the turn they stream for is watched live.
    pub(crate) keep_streaming_slabs: bool,
}

pub(crate) fn merge_local_system_rows(
    persisted_rows: Vec<rebon_tui::Message>,
    current_rows: &[rebon_tui::Message],
    previous_persisted_uuids: &std::collections::HashSet<String>,
    context: MergeStreamingContext<'_>,
) -> Vec<rebon_tui::Message> {
    let mut fresh: std::collections::HashMap<&str, &rebon_tui::Message> = persisted_rows
        .iter()
        .filter_map(|row| row.uuid().map(|uuid| (uuid, row)))
        .collect();
    let mut out = Vec::with_capacity(persisted_rows.len() + current_rows.len());
    let mut new_rows_at = None;
    for row in current_rows {
        if let Some(uuid) = row.uuid() {
            if let Some(fresh_row) = fresh.remove(uuid) {
                out.push((*fresh_row).clone());
                continue;
            }
            if previous_persisted_uuids.contains(uuid) {
                continue;
            }
        }
        let stream_commit_kept = row_is_local_stream_commit(row)
            && (context.keep_streaming_slabs
                || row
                    .uuid()
                    .is_some_and(|uuid| context.settled_local_row_uuids.contains(uuid)));
        if matches!(row, rebon_tui::Message::System(_)) || stream_commit_kept {
            out.push(row.clone());
        } else {
            new_rows_at.get_or_insert(out.len());
        }
    }
    let new_rows = persisted_rows
        .iter()
        .filter(|row| {
            row.uuid().is_none_or(|uuid| {
                fresh.contains_key(uuid) && !context.skipped_persisted_uuids.contains(uuid)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    match new_rows_at {
        Some(index) => {
            out.splice(index..index, new_rows);
        }
        None => out.extend(new_rows),
    }
    out
}

/// A row the streaming pipeline committed locally: `flush_sealed_prefix`'s
/// slabs (`partial-{n}-{idx}`) or a settled watched turn's tail
/// (`partial-final-{user_uuid}`). It is already printed to inline
/// scrollback under that uuid, and the persisted transcript never contains
/// it — dropping it from the store makes the commit cursor's prefix
/// diverge there and re-emit everything after it.
fn row_is_local_stream_commit(row: &rebon_tui::Message) -> bool {
    row.uuid().is_some_and(|uuid| uuid.starts_with("partial-"))
}

/// Feeds serialized JSON straight into the hasher, avoiding a per-entry
/// String allocation of the whole raw payload on every refresh.
struct HashWriter<'a>(&'a mut DefaultHasher);

impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Hasher::write(self.0, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn transcript_fingerprint(entries: &[rebon_session::TranscriptEntry]) -> u64 {
    let mut hasher = DefaultHasher::new();
    entries.len().hash(&mut hasher);
    for entry in entries {
        entry.uuid.hash(&mut hasher);
        let _ = serde_json::to_writer(HashWriter(&mut hasher), &entry.raw);
    }
    hasher.finish()
}

/// What the job record is allowed to say about the turn on screen.
///
/// Everything, when the owner streams no turns: the record is the only word
/// there is, read on this tick's cadence. Almost nothing, when it does: the
/// stream announces each turn before its first token and after its last,
/// and the record's status trails every one of those by a write — read both
/// and the spinner goes out on the stream's word and comes back on the
/// record's, for a tick, at the end of every turn.
///
/// The one thing the record still ends is a turn the owner never announced:
/// this terminal starts the clock on the user's Enter, and a prompt the
/// worker refuses before its turn loop ends in the record — terminal status,
/// nothing queued — with no `Turn` event to close what no event opened.
pub(crate) fn sync_turn_with_record(
    remote: &mut crate::background::RemoteBackgroundAttachment,
    state: &rebon_session_host::BackgroundJobState,
    streaming_turns: bool,
    now: Instant,
) {
    let record_says_running = matches!(
        state.process.status,
        rebon_session_host::BackgroundJobStatus::Queued
            | rebon_session_host::BackgroundJobStatus::Running
            | rebon_session_host::BackgroundJobStatus::NeedsInput
    );
    if !streaming_turns {
        if record_says_running {
            remote.begin_turn(now, false);
        } else {
            remote.end_turn();
        }
    } else if !remote.turn_started_by_stream && !record_says_running && !state.has_pending_prompts()
    {
        remote.end_turn();
    }
}

pub(crate) fn background_owner_is_still_recorded(
    store: &rebon_session_host::BackgroundStore,
    job_id: &str,
) -> bool {
    let Ok(mut state) = store.read_state(job_id) else {
        return false;
    };
    if store.reconcile_stale_pid(&mut state).is_err() {
        return state.process.pid.is_some();
    }
    state.process.pid.is_some()
}

pub(crate) fn clear_remote_turn_projection(
    remote: &mut crate::background::RemoteBackgroundAttachment,
) {
    remote.current_turn_projected_text.clear();
    remote.current_turn_projected_thinking.clear();
    remote.current_turn_visible_tool_call_ids.clear();
    remote.awaiting_overlay_absorption = false;
    remote.initial_overlay_fingerprint = None;
    remote.current_turn_watched = false;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerTurnOutcome {
    Running,
    Finished { succeeded: bool },
}

/// Presentation values a terminal adopts from one whole owner snapshot.
/// The authoritative snapshot itself stays on the attachment in this module.
pub(crate) struct OwnerStatusPresentation {
    pub(crate) usage: Option<rebon_types::Usage>,
    pub(crate) pending_permission: Option<rebon_session_host::BackgroundPermissionQuerySnapshot>,
    pub(crate) permission_mode: Option<String>,
    pub(crate) effort: Option<String>,
}

fn adopt_owner_status(
    remote: &mut crate::background::RemoteBackgroundAttachment,
    snapshot: rebon_session_host::SessionStatusSnapshot,
) -> OwnerStatusPresentation {
    if let Some(mcp) = snapshot.mcp.clone() {
        remote.owner_mcp = Some(mcp);
    }
    let usage = snapshot.usage.as_ref().map(|usage| rebon_types::Usage {
        input_tokens: usage.input_tokens.min(u64::from(u32::MAX)) as u32,
        output_tokens: usage.output_tokens.min(u64::from(u32::MAX)) as u32,
        cache_read_input_tokens: usage.cache_read_tokens.min(u64::from(u32::MAX)) as u32,
        cache_creation_input_tokens: usage.cache_creation_tokens.min(u64::from(u32::MAX)) as u32,
        ..rebon_types::Usage::default()
    });
    let presentation = OwnerStatusPresentation {
        usage,
        pending_permission: snapshot.pending_permission.clone(),
        permission_mode: snapshot.permission_mode.clone(),
        effort: snapshot.effort.clone(),
    };
    // Whole, and replacing whatever was there. A field the owner removes must
    // not survive in a client's stale patchwork copy.
    remote.owner = Some(snapshot);
    presentation
}

/// A hello is the one snapshot whose busy bit decides the mirrored turn.
pub(crate) fn apply_owner_hello(
    remote: &mut crate::background::RemoteBackgroundAttachment,
    snapshot: rebon_session_host::SessionStatusSnapshot,
    now: Instant,
) -> OwnerStatusPresentation {
    if snapshot.busy || snapshot.status == rebon_session_host::BackgroundJobStatus::NeedsInput {
        remote.begin_turn(now, false);
    } else {
        remote.end_turn();
    }
    adopt_owner_status(remote, snapshot)
}

pub(crate) fn apply_owner_status(
    remote: &mut crate::background::RemoteBackgroundAttachment,
    snapshot: rebon_session_host::SessionStatusSnapshot,
) -> OwnerStatusPresentation {
    adopt_owner_status(remote, snapshot)
}

/// Apply the owner's turn transition to the background-owned attachment.
pub(crate) fn apply_owner_turn(
    remote: &mut crate::background::RemoteBackgroundAttachment,
    state: rebon_session_host::TurnStreamState,
    stop_reason: Option<String>,
    now: Instant,
) -> OwnerTurnOutcome {
    match state {
        rebon_session_host::TurnStreamState::Running => {
            remote.begin_turn(now, true);
            remote.terminal_transcript_synced = false;
            tracing::debug!(job_id = %remote.job_id, "mirror: owner announced a turn");
            OwnerTurnOutcome::Running
        }
        rebon_session_host::TurnStreamState::Idle => {
            remote.end_turn();
            let succeeded = stop_reason.as_deref() == Some("end_turn");
            tracing::debug!(
                job_id = %remote.job_id,
                stop_reason = stop_reason.as_deref().unwrap_or(""),
                "mirror: owner announced the turn ended"
            );
            OwnerTurnOutcome::Finished { succeeded }
        }
    }
}

/// What the drain hands on to be applied, in the order the owner said it.
pub(crate) enum OwnerWord {
    Hello(Box<rebon_session_host::SessionStatusSnapshot>),
    Status(Box<rebon_session_host::SessionStatusSnapshot>),
    Turn {
        state: rebon_session_host::TurnStreamState,
        stop_reason: Option<String>,
    },
    Permission(Box<rebon_session_host::BackgroundPermissionQuerySnapshot>),
}

/// Take in whatever the owner has streamed since the last frame.
///
/// Snapshots (`hello`, `status`) come back to be applied by the caller. Deltas
/// do not: they are held on the link for the caller's event-log read, which
/// applies them after any read the frame owes — so a delta the file is about to
/// say lands once, in the owner's order. Turn transitions and permission
/// prompts reach a terminal by their existing routes.
///
/// The receiver is taken off the link for the length of the drain and handed
/// back unless the owner's end hung up. A stream that ended is left off, so the
/// reopen cadence at the top can replace it on a later frame.
pub(crate) fn drain_owner_words(
    remote: &mut crate::background::RemoteBackgroundAttachment,
) -> Vec<OwnerWord> {
    let mut words = Vec::new();
    let mut stream_ended = false;
    let job_id = remote.job_id.clone();
    let Some(worker) = remote.worker.as_mut() else {
        return words;
    };
    // A stream that ended is reopened from the cursor this link reached,
    // on a cadence — an owner that never spoke it is left alone.
    worker.reopen_stream_if_due(&job_id, Instant::now());
    let Some(events) = worker.events_rx.take() else {
        return words;
    };
    loop {
        match events.try_recv() {
            Ok(rebon_session_host::SessionEvent::Hello {
                cursor,
                epoch,
                status,
                ..
            }) => {
                worker.note_stream_hello(epoch, cursor);
                tracing::debug!(%job_id, epoch, cursor, "mirror: owner stream hello");
                words.push(OwnerWord::Hello(status));
            }
            Ok(rebon_session_host::SessionEvent::Status { snapshot, .. }) => {
                words.push(OwnerWord::Status(snapshot))
            }
            Ok(rebon_session_host::SessionEvent::Turn {
                state, stop_reason, ..
            }) => {
                words.push(OwnerWord::Turn { state, stop_reason });
            }
            Ok(rebon_session_host::SessionEvent::Permission { query, .. }) => {
                words.push(OwnerWord::Permission(query));
            }
            Ok(rebon_session_host::SessionEvent::SessionUpdate { cursor, update }) => {
                worker.note_stream_update(cursor, update);
            }
            Ok(rebon_session_host::SessionEvent::Gap { to, .. }) => {
                tracing::debug!(
                    %job_id,
                    to,
                    "mirror: owner announced a stream gap; catching up from the events file"
                );
                worker.note_stream_gap(to);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                stream_ended = true;
                break;
            }
        }
    }
    // The owner stopped streaming — it exited, it never spoke this
    // protocol, or it let this client go. The file carries on either
    // way, so there is nothing to report to the user; a stream that was
    // real is reopened above on the next frame it is due.
    if !stream_ended {
        worker.events_rx = Some(events);
    }
    words
}

/// Read and decode the durable event suffix owed by a mirrored attachment.
///
/// File offsets and stream stamps belong to the attachment; `AppState`
/// projection stays with the terminal and consumes the returned updates.
pub(crate) fn read_remote_session_updates_from_store(
    remote: &mut crate::background::RemoteBackgroundAttachment,
    store: &rebon_session_host::BackgroundStore,
) -> Option<(Vec<rebon_types::SessionUpdateParams>, u64, bool)> {
    let job_id = remote.job_id.clone();
    let session_id = remote.session_id.clone();
    let initializing = !remote.live_events_initialized;
    let (events, new_offset) = store
        .read_events_from_offset(&job_id, remote.live_events_offset)
        .ok()?;

    let lines = events.len();
    let mut already_streamed = 0usize;
    let mut updates = Vec::with_capacity(events.len());
    for event in events {
        if event.kind != "session_update" {
            continue;
        }
        // A line the stream already delivered is not applied again; one it
        // has not moves the cursor so the stream's copy is the one skipped.
        if let Some(stamp) = event.stream_stamp() {
            if let Some(worker) = remote.worker.as_mut() {
                if !worker.file_line_is_new(stamp) {
                    already_streamed += 1;
                    continue;
                }
            }
        }
        let Ok(params) = serde_json::from_value::<rebon_types::SessionUpdateParams>(event.data)
        else {
            continue;
        };
        if params.session_id == session_id {
            updates.push(params);
        }
    }
    if let Some(worker) = remote.worker.as_mut() {
        worker.note_catch_up_read();
    }
    tracing::debug!(
        %job_id,
        lines,
        already_streamed,
        applied = updates.len(),
        "mirror: events file read"
    );
    Some((updates, new_offset, initializing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_message(uuid: &str, content: &str) -> rebon_tui::Message {
        rebon_tui::Message::System(rebon_tui::SystemMessage {
            uuid: uuid.into(),
            timestamp: "2026-07-17T00:00:00.000Z".into(),
            subtype: "info".into(),
            content: Some(content.into()),
            level: Some(rebon_tui::SystemLevel::Info),
            is_meta: None,
        })
    }

    fn assistant_row(uuid: &str) -> rebon_tui::Message {
        rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
            uuid: uuid.into(),
            timestamp: "2026-08-31T00:00:00.000Z".into(),
            message: rebon_tui::AssistantMessageInner {
                role: rebon_tui::AssistantRole::Assistant,
                content: Vec::new(),
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn transcript_entry(
        entry_type: &str,
        uuid: &str,
        content: serde_json::Value,
    ) -> rebon_session::TranscriptEntry {
        rebon_session::TranscriptEntry {
            entry_type: entry_type.into(),
            uuid: uuid.into(),
            parent_uuid: None,
            timestamp: Some("2026-07-17T00:00:00.000Z".into()),
            raw: serde_json::json!({
                "type": entry_type,
                "uuid": uuid,
                "message": {
                    "role": entry_type,
                    "content": content,
                },
            }),
        }
    }

    fn remote_with_projected_turn(
        text: &str,
        tool_call_id: Option<&str>,
    ) -> crate::background::RemoteBackgroundAttachment {
        let mut remote = crate::background::RemoteBackgroundAttachment::new(
            "bg-overlay".into(),
            "sess-overlay".into(),
            ".".into(),
            rebon_session_host::BackgroundJobStatus::Running,
            0,
            crate::background::BackgroundIpcEndpoint {
                pid: 1,
                port: 1,
                token: "overlay-token".into(),
            },
        );
        remote.current_turn_user_uuid = Some("user-current".into());
        remote.current_turn_projected_text = text.into();
        if let Some(tool_call_id) = tool_call_id {
            remote
                .current_turn_visible_tool_call_ids
                .insert(tool_call_id.into());
        }
        remote.awaiting_overlay_absorption = true;
        remote.current_turn_watched = true;
        remote
    }

    fn attachment_for(job_id: &str) -> crate::background::RemoteBackgroundAttachment {
        crate::background::RemoteBackgroundAttachment::new(
            job_id.into(),
            "sess-forward".into(),
            ".".into(),
            rebon_session_host::BackgroundJobStatus::Running,
            0,
            crate::background::BackgroundIpcEndpoint {
                pid: 1,
                port: 1,
                token: "forward-token".into(),
            },
        )
    }

    fn user_row(uuid: &str) -> rebon_tui::Message {
        rebon_tui::Message::User(rebon_tui::UserMessage {
            uuid: uuid.into(),
            timestamp: "2026-08-31T00:00:00.000Z".into(),
            message: rebon_tui::UserMessageInner {
                role: rebon_tui::UserRole::User,
                content: Vec::new(),
            },
            is_meta: None,
            is_compact_summary: None,
            is_visible_in_transcript_only: None,
            image_paste_ids: None,
            plan_content: None,
        })
    }

    #[test]
    fn transcript_refresh_preserves_only_unpersisted_local_system_rows() {
        let persisted_system = system_message("s-persisted", "persisted");
        let persisted_assistant = rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
            uuid: "a-persisted".into(),
            timestamp: "2026-07-17T00:00:00.000Z".into(),
            message: rebon_tui::AssistantMessageInner {
                role: rebon_tui::AssistantRole::Assistant,
                content: Vec::new(),
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        });
        let optimistic_assistant = rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
            uuid: "a-optimistic".into(),
            timestamp: "2026-07-17T00:00:00.000Z".into(),
            message: rebon_tui::AssistantMessageInner {
                role: rebon_tui::AssistantRole::Assistant,
                content: Vec::new(),
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        });
        let previous_persisted_uuids = ["s-persisted".to_string(), "s-removed".to_string()]
            .into_iter()
            .collect();
        let empty = std::collections::HashSet::new();
        let rows = merge_local_system_rows(
            vec![persisted_system.clone(), persisted_assistant],
            &[
                persisted_system,
                system_message("s-removed", "removed remotely"),
                system_message("s-local", "local warning"),
                optimistic_assistant,
            ],
            &previous_persisted_uuids,
            MergeStreamingContext {
                skipped_persisted_uuids: &empty,
                settled_local_row_uuids: &empty,
                keep_streaming_slabs: false,
            },
        );

        // The optimistic row stood after the local warning, so the persisted
        // row that replaces it lands there too: nothing on screen moves.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].uuid(), Some("s-persisted"));
        assert_eq!(rows[1].uuid(), Some("s-local"));
        assert_eq!(rows[2].uuid(), Some("a-persisted"));
    }

    /// A session handed over with rows already on screen — its own turns,
    /// the "Worked" line, the handover's feedback — then mirrors a turn
    /// somebody typed elsewhere. Every row that was there keeps its place;
    /// the new turn goes after them. Anything else re-prints the scrollback.
    #[test]
    fn a_remote_turn_lands_after_the_rows_already_on_screen() {
        let user = |uuid: &str| {
            rebon_tui::Message::User(rebon_tui::UserMessage {
                uuid: uuid.into(),
                timestamp: "2026-07-17T00:00:00.000Z".into(),
                message: rebon_tui::UserMessageInner {
                    role: rebon_tui::UserRole::User,
                    content: Vec::new(),
                },
                is_meta: None,
                is_compact_summary: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            })
        };
        let assistant = |uuid: &str| {
            rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
                uuid: uuid.into(),
                timestamp: "2026-07-17T00:00:00.000Z".into(),
                message: rebon_tui::AssistantMessageInner {
                    role: rebon_tui::AssistantRole::Assistant,
                    content: Vec::new(),
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            })
        };
        let on_screen = [
            user("u-hi"),
            assistant("a-hi"),
            system_message("s-worked", "Worked 4s"),
            system_message("s-hosted", "Moving this session into a worker"),
        ];
        let persisted = vec![
            user("u-hi"),
            assistant("a-hi"),
            user("u-pong"),
            assistant("a-ping"),
        ];
        let previous_persisted_uuids = ["u-hi".to_string(), "a-hi".to_string()]
            .into_iter()
            .collect();

        let empty = std::collections::HashSet::new();
        let rows = merge_local_system_rows(
            persisted,
            &on_screen,
            &previous_persisted_uuids,
            MergeStreamingContext {
                skipped_persisted_uuids: &empty,
                settled_local_row_uuids: &empty,
                keep_streaming_slabs: false,
            },
        );

        let uuids = rows.iter().map(|row| row.uuid()).collect::<Vec<_>>();
        assert_eq!(
            uuids,
            vec![
                Some("u-hi"),
                Some("a-hi"),
                Some("s-worked"),
                Some("s-hosted"),
                Some("u-pong"),
                Some("a-ping"),
            ]
        );
    }

    /// The duplicate the user reported: a settled turn's slabs stand in
    /// scrollback under `partial-*` uuids, and its covered persisted row
    /// must not splice in — while the settled slab rows keep their place,
    /// or the commit cursor's prefix diverges there and re-emits the turn.
    #[test]
    fn covered_persisted_rows_do_not_reprint_settled_slabs() {
        let on_screen = [
            user_row("u-turn"),
            assistant_row("partial-3-0"),
            assistant_row("partial-final-u-turn"),
        ];
        let persisted = vec![user_row("u-turn"), assistant_row("a-answer")];
        let previous_persisted_uuids = ["u-turn".to_string()].into_iter().collect();
        let covered = ["a-answer".to_string()].into_iter().collect();
        let settled = [
            "partial-3-0".to_string(),
            "partial-final-u-turn".to_string(),
        ]
        .into_iter()
        .collect();

        let rows = merge_local_system_rows(
            persisted,
            &on_screen,
            &previous_persisted_uuids,
            MergeStreamingContext {
                skipped_persisted_uuids: &covered,
                settled_local_row_uuids: &settled,
                keep_streaming_slabs: false,
            },
        );

        let uuids = rows.iter().map(|row| row.uuid()).collect::<Vec<_>>();
        assert_eq!(
            uuids,
            vec![
                Some("u-turn"),
                Some("partial-3-0"),
                Some("partial-final-u-turn"),
            ]
        );
    }

    /// A turn that fell to the splice drops its unsettled slabs from the
    /// store like any projection, and the persisted row takes their place —
    /// keeping both is two prints of the same content ("eight Read cards
    /// and then their group card").
    #[test]
    fn the_splice_world_drops_unsettled_slabs_for_the_persisted_rows() {
        let on_screen = [user_row("u-turn"), assistant_row("partial-3-0")];
        let persisted = vec![user_row("u-turn"), assistant_row("a-answer")];
        let previous_persisted_uuids = ["u-turn".to_string()].into_iter().collect();
        let empty = std::collections::HashSet::new();

        let rows = merge_local_system_rows(
            persisted,
            &on_screen,
            &previous_persisted_uuids,
            MergeStreamingContext {
                skipped_persisted_uuids: &empty,
                settled_local_row_uuids: &empty,
                keep_streaming_slabs: false,
            },
        );

        let uuids = rows.iter().map(|row| row.uuid()).collect::<Vec<_>>();
        assert_eq!(uuids, vec![Some("u-turn"), Some("a-answer")]);
    }

    /// Mid-turn on a live watched turn: slabs stay provisionally, and the
    /// turn's own persisted rows are withheld from the splice until it
    /// settles.
    #[test]
    fn a_live_watched_turn_keeps_its_slabs_and_withholds_its_rows() {
        let on_screen = [user_row("u-turn"), assistant_row("partial-3-0")];
        let persisted = vec![user_row("u-turn"), assistant_row("a-answer")];
        let previous_persisted_uuids = ["u-turn".to_string()].into_iter().collect();
        let withheld = ["a-answer".to_string()].into_iter().collect();
        let empty = std::collections::HashSet::new();

        let rows = merge_local_system_rows(
            persisted,
            &on_screen,
            &previous_persisted_uuids,
            MergeStreamingContext {
                skipped_persisted_uuids: &withheld,
                settled_local_row_uuids: &empty,
                keep_streaming_slabs: true,
            },
        );

        let uuids = rows.iter().map(|row| row.uuid()).collect::<Vec<_>>();
        assert_eq!(uuids, vec![Some("u-turn"), Some("partial-3-0")]);
    }

    #[test]
    fn overlay_absorption_handles_tool_only_and_terminal_no_output_turns() {
        let user = transcript_entry(
            "user",
            "user-current",
            serde_json::json!([{"type": "text", "text": "prompt"}]),
        );
        let tool = transcript_entry(
            "assistant",
            "assistant-tool",
            serde_json::json!([{
                "type": "tool_use",
                "id": "tool-1",
                "name": "Read",
                "input": {},
            }]),
        );
        let tool_remote = remote_with_projected_turn("", Some("tool-1"));
        assert!(super::remote_turn_is_absorbed(
            &[user.clone(), tool],
            &tool_remote,
            2,
            false,
        ));

        let mut terminal_remote = remote_with_projected_turn("", Some("tool-missing"));
        terminal_remote.current_turn_projected_thinking = "unfinished thought".into();
        assert!(!super::remote_turn_is_absorbed(
            std::slice::from_ref(&user),
            &terminal_remote,
            2,
            false,
        ));
        assert!(super::remote_turn_is_absorbed(
            &[user],
            &terminal_remote,
            2,
            true,
        ));
    }

    /// Coverage only flows one way: the stream must contain the file, never
    /// the reverse. A projection that is behind the file — or that watched
    /// nothing at all — covers nothing, so the splice keeps printing the
    /// authoritative rows.
    #[test]
    fn projection_coverage_requires_the_stream_to_contain_the_file() {
        let entries = vec![
            transcript_entry(
                "user",
                "user-current",
                serde_json::json!([{"type": "text", "text": "prompt"}]),
            ),
            transcript_entry(
                "assistant",
                "a-1",
                serde_json::json!([{"type": "text", "text": "hello "}]),
            ),
        ];
        let turn = super::persisted_remote_turn(&entries, "user-current");
        assert_eq!(turn.assistant_uuids, vec!["a-1".to_string()]);

        let no_tools = std::collections::HashSet::new();
        assert!(super::projection_covers_persisted_turn(
            &turn,
            "hello world",
            "",
            &no_tools,
        ));
        assert!(!super::projection_covers_persisted_turn(
            &turn, "hel", "", &no_tools,
        ));
        assert!(!super::projection_covers_persisted_turn(
            &turn, "", "", &no_tools,
        ));
    }

    /// A block the stream never draws (an image, an unknown type) means the
    /// local commit cannot stand for the entry; such a turn always splices.
    #[test]
    fn an_unprojectable_assistant_block_blocks_coverage() {
        let entries = vec![
            transcript_entry(
                "user",
                "user-current",
                serde_json::json!([{"type": "text", "text": "prompt"}]),
            ),
            transcript_entry(
                "assistant",
                "a-image",
                serde_json::json!([{"type": "image", "source": {}}]),
            ),
        ];
        let turn = super::persisted_remote_turn(&entries, "user-current");
        assert!(turn.has_unprojectable_content);
        assert!(!super::projection_covers_persisted_turn(
            &turn,
            "some watched text",
            "",
            &std::collections::HashSet::new(),
        ));
    }

    /// JSONL can file a settled turn's final entries after the settle; they
    /// are covered against the kept snapshot until the next user turn is on
    /// disk, and the snapshot drops there.
    #[test]
    fn late_persisted_entries_of_a_settled_turn_are_covered_until_the_next_user_turn() {
        let mut remote = attachment_for("bg-settled");
        remote.last_settled_turn = Some(crate::background::SettledTurnProjection {
            user_uuid: "user-current".into(),
            assistant_text: "hello world".into(),
            thinking_text: String::new(),
            tool_call_ids: std::collections::HashSet::new(),
        });
        let early = vec![
            transcript_entry(
                "user",
                "user-current",
                serde_json::json!([{"type": "text", "text": "prompt"}]),
            ),
            transcript_entry(
                "assistant",
                "a-1",
                serde_json::json!([{"type": "text", "text": "hello "}]),
            ),
        ];
        super::cover_settled_turn_entries(&mut remote, &early);
        assert!(remote.covered_persisted_uuids.contains("a-1"));
        assert!(remote.last_settled_turn.is_some());

        let mut late = early;
        late.push(transcript_entry(
            "assistant",
            "a-2",
            serde_json::json!([{"type": "text", "text": "world"}]),
        ));
        late.push(transcript_entry(
            "user",
            "user-next",
            serde_json::json!([{"type": "text", "text": "next prompt"}]),
        ));
        super::cover_settled_turn_entries(&mut remote, &late);
        assert!(remote.covered_persisted_uuids.contains("a-2"));
        assert!(remote.last_settled_turn.is_none());
    }

    /// After a turn settles, progress updates for its (still running) agent
    /// tools keep arriving. They must not re-open live overlay cards — the
    /// committed group card already stands for them.
    #[test]
    fn a_settled_turns_tool_updates_do_not_reopen_overlay_cards() {
        let mut remote = attachment_for("bg-agents");
        let update = |id: &str| rebon_types::SessionUpdate::ToolCallUpdate {
            tool_call_id: id.into(),
            status: None,
            title: None,
            content: None,
            locations: None,
            raw_output: None,
        };
        assert!(!super::update_targets_settled_tool(
            &remote,
            &update("tool-1")
        ));

        remote.last_settled_turn = Some(crate::background::SettledTurnProjection {
            user_uuid: "user-current".into(),
            assistant_text: String::new(),
            thinking_text: String::new(),
            tool_call_ids: ["tool-1".to_string()].into_iter().collect(),
        });
        assert!(super::update_targets_settled_tool(
            &remote,
            &update("tool-1")
        ));
        assert!(!super::update_targets_settled_tool(
            &remote,
            &update("tool-2")
        ));
    }

    /// A rewind that removes the settled turn's boundary takes the snapshot
    /// down with the coverage, so a re-sent prompt at the same position is
    /// never covered against the old turn's projection.
    #[test]
    fn a_rewound_settled_turn_drops_its_snapshot_with_its_coverage() {
        let mut remote = attachment_for("bg-rewind");
        remote.covered_persisted_uuids.insert("a-old".into());
        remote.last_settled_turn = Some(crate::background::SettledTurnProjection {
            user_uuid: "user-old".into(),
            assistant_text: "old answer".into(),
            thinking_text: String::new(),
            tool_call_ids: std::collections::HashSet::new(),
        });
        let persisted: std::collections::HashSet<String> =
            ["user-new".to_string()].into_iter().collect();

        super::prune_stale_coverage(&mut remote, &persisted);

        assert!(remote.covered_persisted_uuids.is_empty());
        assert!(remote.last_settled_turn.is_none());
    }
    fn empty_runtime() -> rebon_session_host::BackgroundRuntimeFields {
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

    #[test]
    fn recorded_live_owner_keeps_remote_mirror_eligible() {
        let dir = tempfile::tempdir().unwrap();
        let store = rebon_session_host::BackgroundStore::new(dir.path());
        let mut state = store
            .create_job(
                "prompt".into(),
                std::path::PathBuf::from("."),
                empty_runtime(),
            )
            .unwrap();
        state.process.status = rebon_session_host::BackgroundJobStatus::Running;
        state.identity.session_id = Some("session-live-owner".into());
        state.process.pid = Some(std::process::id());
        store.write_state(&state).unwrap();

        assert!(background_owner_is_still_recorded(
            &store,
            &state.identity.job_id
        ));
    }

    /// What the job record may still say about the turn, once the owner
    /// streams turns: almost nothing. It trails the stream by a write, so it
    /// neither ends a turn the stream announced nor restarts one the stream
    /// ended. The one turn it ends is the one this terminal began on the
    /// user's Enter that the owner never announced — a prompt refused before
    /// the turn loop, which ends in the record and in no `Turn` event.
    #[test]
    fn the_record_ends_only_a_turn_the_owner_never_announced() {
        let now = std::time::Instant::now();
        let dir = tempfile::tempdir().unwrap();
        let store = rebon_session_host::BackgroundStore::new(dir.path());
        let mut state = store
            .create_job(
                "prompt".into(),
                std::path::PathBuf::from("."),
                empty_runtime(),
            )
            .unwrap();
        let mut remote = attachment_for(&state.identity.job_id);
        remote.end_turn();

        // Typed here, refused there: terminal, nothing queued, no Turn event.
        remote.begin_turn(now, false);
        state.process.status = rebon_session_host::BackgroundJobStatus::Failed;
        state.identity.pending_prompts.clear();
        super::sync_turn_with_record(&mut remote, &state, true, now);
        assert!(
            remote.running_turn_started_at().is_none(),
            "the record ends a turn the stream never announced"
        );

        // Typed here, still queued there: the worker has not claimed it yet.
        remote.begin_turn(now, false);
        state.process.status = rebon_session_host::BackgroundJobStatus::Queued;
        super::sync_turn_with_record(&mut remote, &state, true, now);
        assert!(remote.running_turn_started_at().is_some());

        // Announced by the stream: the record's terminal status is the lag,
        // and the stream will say when this ends.
        remote.begin_turn(now, true);
        state.process.status = rebon_session_host::BackgroundJobStatus::Succeeded;
        super::sync_turn_with_record(&mut remote, &state, true, now);
        assert!(
            remote.running_turn_started_at().is_some(),
            "a turn the stream announced is the stream's to end"
        );

        // Ended by the stream: the record still saying Running is the lag.
        remote.end_turn();
        state.process.status = rebon_session_host::BackgroundJobStatus::Running;
        super::sync_turn_with_record(&mut remote, &state, true, now);
        assert!(
            remote.running_turn_started_at().is_none(),
            "the record does not restart a turn the stream ended"
        );

        // No stream: the record is all there is.
        super::sync_turn_with_record(&mut remote, &state, false, now);
        assert!(remote.running_turn_started_at().is_some());
        state.process.status = rebon_session_host::BackgroundJobStatus::Idle;
        super::sync_turn_with_record(&mut remote, &state, false, now);
        assert!(remote.running_turn_started_at().is_none());
    }

    /// The receiver is taken off the link to drain it, and has to go back on:
    /// a frame that dropped it would leave the mirror reading the events file
    /// for the rest of the session while the owner went on streaming. The one
    /// case it stays off is an owner that hung up, so the reopen cadence can
    /// put a fresh stream in its place.
    #[test]
    fn an_open_stream_goes_back_on_the_link_and_a_dead_one_does_not() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut remote = attachment_for("bg-drain");
        remote
            .worker
            .as_mut()
            .expect("the attachment was built with a worker")
            .events_rx = Some(rx);

        tx.send(rebon_session_host::SessionEvent::Turn {
            cursor: 1,
            state: rebon_session_host::TurnStreamState::Running,
            stop_reason: None,
            stop_refused: None,
        })
        .expect("the drain has not taken the receiver yet");

        let words = drain_owner_words(&mut remote);
        assert_eq!(words.len(), 1);
        assert!(matches!(
            words[0],
            OwnerWord::Turn {
                state: rebon_session_host::TurnStreamState::Running,
                ..
            }
        ));
        assert!(
            remote
                .worker
                .as_ref()
                .and_then(|worker| worker.events_rx.as_ref())
                .is_some(),
            "an open stream must be handed back, or the mirror never reads it again"
        );

        drop(tx);
        let words = drain_owner_words(&mut remote);
        assert!(words.is_empty(), "a hung-up stream has nothing to say");
        assert!(
            remote
                .worker
                .as_ref()
                .and_then(|worker| worker.events_rx.as_ref())
                .is_none(),
            "a stream whose owner hung up is let go, so a later frame can reopen it"
        );
    }
}
