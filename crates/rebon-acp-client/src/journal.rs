//! Writing somebody else's turn into Rebon's history.
//!
//! An ACP agent keeps its own conversation. If Rebon just streamed the
//! updates to the screen and forgot them, the session would be a live
//! view of a history Rebon does not have: `--resume` would reopen an
//! empty transcript, `/rewind` would have no turns to rewind to, and
//! switching back to the local engine would drop the user into an
//! amnesiac conversation. So every ACP turn is written down here, in
//! the same on-disk shape the local engine writes.
//!
//! # What a turn becomes
//!
//! One turn produces up to three transcript rows:
//!
//! 1. a `user` row for the prompt, written before the agent is asked
//!    anything — it is also the id the file-history snapshot for this
//!    turn is keyed on, so it has to exist first;
//! 2. an `assistant` row with the agent's text and the tool calls it
//!    made;
//! 3. a `user` row carrying the tool results, when there were tools.
//!
//! Within the turn, text is kept in arrival order and tool calls follow
//! it, rather than being interleaved exactly as they happened. This is
//! a deliberate simplification: what has to survive is a *replayable*
//! shape — every `tool_use` paired with a `tool_result` — because the
//! local engine may be handed this transcript later. A tool the agent
//! never finished is closed out with an error result for the same
//! reason: a dangling `tool_use` is a message no model will accept.
//!
//! Thinking is streamed to the UI but not recorded. Extended-thinking
//! blocks are only replayable with the provider signature that produced
//! them, which an ACP agent does not expose; writing unsigned ones down
//! would produce a transcript that renders fine and fails to send.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rebon_session::TranscriptWriteEntry;
use rebon_types::{ContentBlock, SessionUpdate, StopReason, ToolCallContent, ToolCallStatus};

use crate::host_fs::HostFileHistory;

/// Where an ACP turn is written down.
///
/// The backend drives this: [`Self::begin_turn`] before prompting,
/// [`Self::record_update`] for everything the agent streams back, and
/// [`Self::end_turn`] however the turn finishes — including cancelled
/// and failed, which are exactly the turns whose partial work most
/// needs to be recoverable.
///
/// Every method takes the host session id. Implementations that serve
/// one session should reject ids that are not theirs rather than write
/// one session's turn into another's transcript.
pub trait TurnJournal: Send + Sync {
    /// Record the prompt and open a turn. Returns the turn id — the
    /// transcript's user-row uuid, which is also what file history
    /// keys its snapshot on.
    ///
    /// `None` means nothing was recorded; the caller carries on with
    /// the turn, because failing to write history is not a reason to
    /// refuse to answer the user.
    fn begin_turn(&self, session_id: &str, prompt: &[ContentBlock]) -> Option<String>;

    /// Fold one streamed update into the open turn.
    fn record_update(&self, session_id: &str, update: &SessionUpdate);

    /// Close the turn out and flush it to disk.
    fn end_turn(&self, session_id: &str, turn_id: &str, stop_reason: Option<StopReason>);

    /// Remember the agent's own session id for this host session, so a
    /// later run can offer it back to `session/load`.
    fn record_agent_session(&self, _session_id: &str, _agent_session_id: &str) {}

    /// Record a message steered into the open turn — written down
    /// immediately (a steered message that only lived in memory would
    /// vanish if the agent died mid-turn), under the caller's uuid so
    /// the UI's echo and the disk row are the same message.
    fn record_steered_prompt(
        &self,
        _session_id: &str,
        _user_message_uuid: &str,
        _prompt: &[ContentBlock],
    ) {
    }
}

/// A journal that records nothing.
///
/// For hosts that drive an agent without a session on disk — tests, and
/// any caller that has not opted into transcript persistence. Honest by
/// omission: nothing is written, so nothing is promised.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTurnJournal;

impl TurnJournal for NoopTurnJournal {
    fn begin_turn(&self, _session_id: &str, _prompt: &[ContentBlock]) -> Option<String> {
        None
    }

    fn record_update(&self, _session_id: &str, _update: &SessionUpdate) {}

    fn end_turn(&self, _session_id: &str, _turn_id: &str, _stop_reason: Option<StopReason>) {}
}

/// One tool call, as it accumulates across `tool_call` /
/// `tool_call_update` messages.
#[derive(Debug, Clone)]
struct RecordedToolCall {
    id: String,
    title: String,
    input: serde_json::Value,
    output: Option<String>,
    status: ToolCallStatus,
}

impl RecordedToolCall {
    fn tool_use_block(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "tool_use",
            "id": self.id,
            "name": self.title,
            "input": self.input,
        })
    }

    /// The paired result. A tool the agent never finished still gets
    /// one, marked as an error — an unpaired `tool_use` is not a
    /// message any model will accept on replay.
    fn tool_result_block(&self) -> serde_json::Value {
        let finished = matches!(
            self.status,
            ToolCallStatus::Completed | ToolCallStatus::Failed
        );
        let content = match (&self.output, finished) {
            (Some(output), _) if !output.is_empty() => output.clone(),
            (_, true) => String::new(),
            (_, false) => "Tool call did not finish before the turn ended.".to_string(),
        };
        let is_error = matches!(self.status, ToolCallStatus::Failed) || !finished;
        serde_json::json!({
            "type": "tool_result",
            "tool_use_id": self.id,
            "content": content,
            "is_error": is_error,
        })
    }
}

/// Everything an open turn has accumulated.
#[derive(Debug, Default)]
struct OpenTurn {
    user_uuid: String,
    /// The most recent user row on disk for this turn — the prompt, or
    /// the last steered message. The assistant row hangs off this, so
    /// a replay reads user(prompt), user(steered…), assistant, results
    /// as one unbroken chain.
    last_user_uuid: String,
    text: String,
    /// First-seen order, so the transcript reads the way the turn ran.
    tool_order: Vec<String>,
    tools: HashMap<String, RecordedToolCall>,
}

impl OpenTurn {
    fn tool_calls(&self) -> Vec<&RecordedToolCall> {
        self.tool_order
            .iter()
            .filter_map(|id| self.tools.get(id))
            .collect()
    }
}

/// Where every persisted row is mirrored, beyond the JSONL file.
///
/// The host's in-memory transcript projection has a revision counter
/// that replay caching keys on. A journal that only writes disk leaves
/// that projection stale — the local engine, asked to take over after
/// an ACP turn, would replay history missing the rows this journal
/// just wrote. Args: (host session id, the finalized entry).
pub type TranscriptSink = Arc<dyn Fn(&str, rebon_session::TranscriptEntry) + Send + Sync>;

/// Writes ACP turns into a Rebon session transcript.
///
/// Bound to one session. A host driving several sessions through one
/// agent gives each its own journal rather than sharing this one —
/// see the session-id check in [`Self::check_session`], which turns a
/// mistake there into a loud log line instead of a corrupted history.
pub struct TranscriptJournal {
    projects_root: PathBuf,
    cwd: String,
    /// Immutable session binding. A front end that switches sessions must build
    /// a fresh journal and let any in-flight turn retain this instance.
    session_id: String,
    agent_id: String,
    file_history: Option<Arc<dyn HostFileHistory>>,
    transcript_sink: Option<TranscriptSink>,
    open: Mutex<Option<OpenTurn>>,
    /// `(transcript file length, tail uuid)` at the last time either
    /// was observed. Keyed on the length so writers this journal does
    /// not know about — the local engine taking a turn in the same
    /// session, most of all — invalidate it: a size mismatch falls
    /// back to reading the file. Without this, every `begin_turn`
    /// re-parses the entire transcript just to find its last row.
    tail_cache: Mutex<Option<(u64, String)>>,
}

impl TranscriptJournal {
    pub fn new(
        projects_root: impl Into<PathBuf>,
        cwd: impl Into<String>,
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            projects_root: projects_root.into(),
            cwd: cwd.into(),
            session_id: session_id.into(),
            agent_id: agent_id.into(),
            file_history: None,
            transcript_sink: None,
            open: Mutex::new(None),
            tail_cache: Mutex::new(None),
        }
    }

    /// Mirror every persisted row into the host's in-memory transcript
    /// projection — see [`TranscriptSink`]. Without this, switching
    /// back to the local engine can replay a cached history that is
    /// missing the ACP turns.
    pub fn with_transcript_sink(mut self, sink: TranscriptSink) -> Self {
        self.transcript_sink = Some(sink);
        self
    }

    /// Arm and disarm the host's file-history pipeline around each
    /// turn, so `/rewind` has a boundary to restore to.
    ///
    /// Without this the snapshots taken during the turn belong to no
    /// turn in particular, which is the difference between rewind
    /// working and rewind finding nothing.
    pub fn with_file_history(mut self, file_history: Arc<dyn HostFileHistory>) -> Self {
        self.file_history = Some(file_history);
        self
    }

    fn check_session(&self, session_id: &str) -> bool {
        if session_id == self.session_id {
            return true;
        }
        tracing::error!(
            journal_session = %self.session_id,
            requested_session = %session_id,
            "acp-client: refusing to journal a turn belonging to another session"
        );
        false
    }

    /// The uuid the next row should hang off: the tail of whatever is
    /// already on disk. Served from `tail_cache` when the file length
    /// still matches; a full parse only happens on the first turn and
    /// after another writer touched the transcript.
    fn tail_uuid(&self) -> Option<String> {
        let path =
            rebon_session::transcript_file_path(&self.projects_root, &self.cwd, &self.session_id);
        let file_len = std::fs::metadata(&path).ok()?.len();
        {
            let cache = self
                .tail_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((cached_len, uuid)) = cache.as_ref() {
                if *cached_len == file_len {
                    return Some(uuid.clone());
                }
            }
        }
        let loaded = rebon_session::load_transcript_from_file(&path).ok()??;
        let tail = loaded.messages.last().map(|entry| entry.uuid.clone())?;
        *self
            .tail_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((file_len, tail.clone()));
        Some(tail)
    }

    /// Record the tail this journal just wrote, so the next
    /// [`Self::tail_uuid`] does not have to parse the file for it.
    fn note_written_tail(&self, uuid: &str) {
        let path =
            rebon_session::transcript_file_path(&self.projects_root, &self.cwd, &self.session_id);
        let Ok(metadata) = std::fs::metadata(&path) else {
            return;
        };
        *self
            .tail_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((metadata.len(), uuid.to_string()));
    }

    /// Close out this turn's file-history arming.
    ///
    /// Called on every path out of [`TurnJournal::end_turn`], including
    /// the ones that write nothing: leaving a turn armed would let the
    /// next turn's snapshots land against a boundary that has passed.
    fn release_file_history(&self, turn_id: &str) {
        let Some(history) = &self.file_history else {
            return;
        };
        if let Err(err) = history.end_turn(&self.session_id, turn_id) {
            tracing::warn!(
                error = %err,
                session_id = %self.session_id,
                "acp-client: could not close out file history for this turn"
            );
        }
    }

    fn append(&self, entry: TranscriptWriteEntry, label: &str) -> Option<String> {
        match rebon_session::append_transcript_entry(
            &self.projects_root,
            &self.cwd,
            &self.session_id,
            entry,
        ) {
            Ok(entry) => {
                let uuid = entry.uuid.clone();
                self.note_written_tail(&uuid);
                if let Some(sink) = &self.transcript_sink {
                    sink(&self.session_id, entry);
                }
                Some(uuid)
            }
            Err(err) => {
                // A history we could not write is worth a loud log and
                // nothing else: the turn itself is still the user's
                // answer, and refusing it would trade a degraded
                // transcript for no conversation at all.
                tracing::warn!(
                    error = %err,
                    session_id = %self.session_id,
                    label,
                    "acp-client: failed to persist a transcript entry"
                );
                None
            }
        }
    }
}

impl std::fmt::Debug for TranscriptJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptJournal")
            .field("session_id", &self.session_id)
            .field("agent_id", &self.agent_id)
            .field("cwd", &self.cwd)
            .field("file_history", &self.file_history.is_some())
            .finish()
    }
}

impl TurnJournal for TranscriptJournal {
    fn begin_turn(&self, session_id: &str, prompt: &[ContentBlock]) -> Option<String> {
        if !self.check_session(session_id) {
            return None;
        }

        let entry = TranscriptWriteEntry::new(
            "user",
            serde_json::json!({
                "message": {
                    "role": "user",
                    "content": prompt_content(prompt),
                },
                // Marks whose turn this was. A transcript that mixes
                // local and ACP turns still reads correctly, but the
                // provenance is worth keeping.
                "acpAgent": self.agent_id,
            }),
        );
        let entry = match self.tail_uuid() {
            Some(parent) => entry.with_parent(parent),
            None => entry,
        };
        let user_uuid = self.append(entry, "acp user prompt")?;

        if let Some(history) = &self.file_history {
            if let Err(err) = history.begin_turn(&self.session_id, &user_uuid) {
                tracing::warn!(
                    error = %err,
                    session_id = %self.session_id,
                    "acp-client: could not arm file history; /rewind will not cover this turn"
                );
            }
        }

        *self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(OpenTurn {
            user_uuid: user_uuid.clone(),
            last_user_uuid: user_uuid.clone(),
            ..OpenTurn::default()
        });
        Some(user_uuid)
    }

    fn record_update(&self, session_id: &str, update: &SessionUpdate) {
        if !self.check_session(session_id) {
            return;
        }
        let mut guard = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(turn) = guard.as_mut() else {
            // Updates outside a turn have nowhere to go. Dropping them
            // is right: the alternative is attributing them to a turn
            // that has already been written.
            return;
        };

        match update {
            SessionUpdate::AgentMessageChunk { content } => {
                if let Some(text) = content_text(content) {
                    turn.text.push_str(&text);
                }
            }
            SessionUpdate::ToolCall {
                tool_call_id,
                title,
                status,
                raw_input,
                raw_output,
                content,
                ..
            } => {
                let recorded = RecordedToolCall {
                    id: tool_call_id.clone(),
                    title: title.clone(),
                    input: raw_input
                        .as_ref()
                        .map(|input| serde_json::json!(input))
                        .unwrap_or_else(|| serde_json::json!({})),
                    output: tool_output(raw_output.as_ref(), content.as_deref()),
                    status: *status,
                };
                if !turn.tools.contains_key(tool_call_id) {
                    turn.tool_order.push(tool_call_id.clone());
                }
                turn.tools.insert(tool_call_id.clone(), recorded);
            }
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status,
                title,
                content,
                raw_output,
                ..
            } => {
                let Some(recorded) = turn.tools.get_mut(tool_call_id) else {
                    // An update for a call we never saw announced. The
                    // agent is within its rights to do this, but we
                    // have no input to pair it with, so recording a
                    // half tool call would be worse than skipping it.
                    return;
                };
                if let Some(status) = status {
                    recorded.status = *status;
                }
                if let Some(title) = title {
                    recorded.title = title.clone();
                }
                if let Some(output) = tool_output(raw_output.as_ref(), content.as_deref()) {
                    recorded.output = Some(output);
                }
            }
            // Thinking is shown live and not recorded — see the module
            // docs. Everything else is UI state with no transcript
            // meaning.
            _ => {}
        }
    }

    fn end_turn(&self, session_id: &str, turn_id: &str, stop_reason: Option<StopReason>) {
        if !self.check_session(session_id) {
            return;
        }
        // Matched *before* taking. A late-ending earlier turn must not
        // carry off the open turn's accumulated output on its way out —
        // it would end up discarded, and the live turn would flush
        // empty.
        let turn = {
            let mut open = self
                .open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match open.as_ref() {
                Some(turn) if turn.user_uuid == turn_id => open.take(),
                Some(turn) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        open_turn = %turn.user_uuid,
                        ending_turn = %turn_id,
                        "acp-client: ignoring an out-of-order turn end"
                    );
                    None
                }
                None => None,
            }
        };
        let Some(turn) = turn else {
            // Still release the file-history arming: a turn that opened
            // is a turn that armed, whatever happened to its output.
            self.release_file_history(turn_id);
            return;
        };

        let tools = turn.tool_calls();
        let mut content = Vec::new();
        if !turn.text.is_empty() {
            content.push(serde_json::json!({ "type": "text", "text": turn.text }));
        }
        content.extend(tools.iter().map(|tool| tool.tool_use_block()));

        if !content.is_empty() {
            // No reported reason means the turn never got to report
            // one — it errored or the agent died. Recording that as
            // `EndTurn` would make a failed turn read as a completed
            // one in the history a later run replays.
            let stop = stop_reason.unwrap_or(StopReason::Cancelled);
            let assistant = TranscriptWriteEntry::new(
                "assistant",
                serde_json::json!({
                    "message": {
                        "role": "assistant",
                        "content": content,
                        "stop_reason": stop,
                        // ACP reports no token usage; zeros here mean
                        // "not reported", not "free".
                        "usage": rebon_types::Usage::default(),
                    },
                    "iteration": 0,
                    "acpAgent": self.agent_id,
                }),
            )
            .with_parent(turn.last_user_uuid.clone());

            if let Some(assistant_uuid) = self.append(assistant, "acp assistant message") {
                if !tools.is_empty() {
                    let results: Vec<serde_json::Value> =
                        tools.iter().map(|tool| tool.tool_result_block()).collect();
                    let entry = TranscriptWriteEntry::new(
                        "user",
                        serde_json::json!({
                            "message": { "role": "user", "content": results },
                            "acpAgent": self.agent_id,
                        }),
                    )
                    .with_parent(assistant_uuid);
                    self.append(entry, "acp tool results");
                }
            }
        }

        self.release_file_history(turn_id);
    }

    fn record_steered_prompt(
        &self,
        session_id: &str,
        user_message_uuid: &str,
        prompt: &[ContentBlock],
    ) {
        if !self.check_session(session_id) {
            return;
        }
        let mut guard = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(turn) = guard.as_mut() else {
            // The turn ended while the steer was in flight. The caller
            // sees that race through the steer outcome and re-sends
            // the message as a fresh prompt; recording it here would
            // put it on disk twice.
            tracing::warn!(
                session_id = %self.session_id,
                "acp-client: dropping a steered message that missed its turn"
            );
            return;
        };

        let entry = TranscriptWriteEntry::new(
            "user",
            serde_json::json!({
                "message": {
                    "role": "user",
                    "content": prompt_content(prompt),
                },
                // The same marker the local engine writes for a
                // mid-turn message, so every renderer treats the two
                // legs alike.
                "queuedCommand": true,
                "acpAgent": self.agent_id,
            }),
        )
        .with_uuid(user_message_uuid)
        .with_parent(turn.last_user_uuid.clone());
        // Written immediately, not buffered to end_turn: a steered
        // message the agent is already acting on must survive the
        // agent dying mid-turn.
        if let Some(uuid) = self.append(entry, "acp steered prompt") {
            turn.last_user_uuid = uuid;
        }
    }

    fn record_agent_session(&self, session_id: &str, agent_session_id: &str) {
        if !self.check_session(session_id) {
            return;
        }
        if let Err(err) = rebon_session::save_session_agent(
            &self.projects_root,
            &self.cwd,
            &self.session_id,
            &self.agent_id,
        ) {
            tracing::warn!(
                error = %err,
                session_id = %self.session_id,
                "acp-client: could not record which agent runs this session"
            );
            return;
        }
        if let Err(err) = rebon_session::save_agent_session_id(
            &self.projects_root,
            &self.cwd,
            &self.session_id,
            agent_session_id,
        ) {
            tracing::warn!(
                error = %err,
                session_id = %self.session_id,
                "acp-client: could not record the agent's session id; \
                 resuming after a restart will start a fresh agent session"
            );
        }
    }
}

/// The prompt as transcript content.
///
/// Non-text blocks are named rather than embedded: the transcript is
/// what a later local turn replays, and inlining an agent-shaped
/// resource block there would produce content the model cannot read.
fn prompt_content(prompt: &[ContentBlock]) -> Vec<serde_json::Value> {
    let mut blocks = Vec::new();
    for block in prompt {
        if let Some(text) = content_text(block) {
            blocks.push(serde_json::json!({ "type": "text", "text": text }));
        }
    }
    if blocks.is_empty() {
        blocks.push(serde_json::json!({ "type": "text", "text": "" }));
    }
    blocks
}

/// The text of a content block, when it has any.
fn content_text(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(text) => Some(text.text.clone()),
        ContentBlock::Resource(resource) => resource_text(&resource.resource),
        ContentBlock::ResourceLink(link) => Some(format!("[{}]({})", link.name, link.uri)),
        // Images and audio have no textual form worth inventing.
        ContentBlock::Image(_) | ContentBlock::Audio(_) => None,
    }
}

fn resource_text(resource: &rebon_types::ResourceBody) -> Option<String> {
    resource.text.clone()
}

/// The tool's output, preferring what the agent said explicitly.
#[allow(clippy::type_complexity)]
fn tool_output(
    raw_output: Option<&HashMap<String, serde_json::Value>>,
    content: Option<&[ToolCallContent]>,
) -> Option<String> {
    if let Some(raw) = raw_output {
        if !raw.is_empty() {
            return Some(
                serde_json::to_string(raw).unwrap_or_else(|_| "<unserializable output>".into()),
            );
        }
    }
    let content = content?;
    let mut text = String::new();
    for item in content {
        match item {
            ToolCallContent::Content(regular) => {
                if let Some(chunk) = content_text(&regular.content) {
                    text.push_str(&chunk);
                }
            }
            ToolCallContent::Diff(diff) => {
                text.push_str(&format!("(diff of {})", diff.path));
            }
            ToolCallContent::Terminal(terminal) => {
                text.push_str(&format!("(terminal {})", terminal.terminal_id));
            }
        }
    }
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_types::{RegularContent, TextContent, ToolKind};
    use std::path::Path;

    fn temp_root(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("rebon-acp-journal-{name}-"))
            .tempdir()
            .expect("projects root")
    }

    const CWD: &str = "/tmp/repo";
    const SESSION: &str = "sess-1";

    fn journal(root: &Path) -> TranscriptJournal {
        TranscriptJournal::new(root, CWD, SESSION, "claude-code")
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text(TextContent {
            text: text.to_string(),
            annotations: None,
        })
    }

    fn rows(root: &Path) -> Vec<(String, serde_json::Value)> {
        let path = rebon_session::transcript_file_path(root, CWD, SESSION);
        rebon_session::load_raw_transcript_from_file(&path)
            .expect("read transcript")
            .map(|raw| {
                raw.entries
                    .into_iter()
                    .map(|entry| (entry.entry_type, entry.raw))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn content_of(row: &serde_json::Value) -> &Vec<serde_json::Value> {
        row["message"]["content"]
            .as_array()
            .expect("message content is an array")
    }

    fn tool_call(id: &str, title: &str, status: ToolCallStatus) -> SessionUpdate {
        SessionUpdate::ToolCall {
            tool_call_id: id.to_string(),
            title: title.to_string(),
            kind: ToolKind::Edit,
            status,
            content: None,
            locations: None,
            raw_input: Some(HashMap::from([(
                "path".to_string(),
                serde_json::json!("src/main.rs"),
            )])),
            raw_output: None,
        }
    }

    #[test]
    fn a_prompt_is_written_down_before_the_agent_is_asked_anything() {
        let root = temp_root("prompt");
        let journal = journal(root.path());
        let turn = journal
            .begin_turn(SESSION, &[text_block("fix the build")])
            .expect("turn opened");

        let rows = rows(root.path());
        assert_eq!(rows.len(), 1, "only the user row exists yet");
        assert_eq!(rows[0].0, "user");
        assert_eq!(content_of(&rows[0].1)[0]["text"], "fix the build");
        assert_eq!(rows[0].1["uuid"], turn);
        assert_eq!(
            rows[0].1["acpAgent"], "claude-code",
            "the transcript records which agent ran the turn"
        );
    }

    #[test]
    fn a_turn_becomes_an_assistant_row_and_a_paired_tool_result_row() {
        let root = temp_root("full-turn");
        let journal = journal(root.path());
        let turn = journal
            .begin_turn(SESSION, &[text_block("edit it")])
            .unwrap();

        journal.record_update(
            SESSION,
            &SessionUpdate::AgentMessageChunk {
                content: text_block("I will "),
            },
        );
        journal.record_update(
            SESSION,
            &SessionUpdate::AgentMessageChunk {
                content: text_block("edit main.rs."),
            },
        );
        journal.record_update(
            SESSION,
            &tool_call("t1", "Edit main.rs", ToolCallStatus::Pending),
        );
        journal.record_update(
            SESSION,
            &SessionUpdate::ToolCallUpdate {
                tool_call_id: "t1".into(),
                status: Some(ToolCallStatus::Completed),
                title: None,
                content: Some(vec![ToolCallContent::Content(RegularContent {
                    content: text_block("wrote 3 lines"),
                })]),
                locations: None,
                raw_output: None,
            },
        );
        journal.end_turn(SESSION, &turn, Some(StopReason::EndTurn));

        let rows = rows(root.path());
        assert_eq!(rows.len(), 3, "user prompt, assistant, tool results");

        let assistant = &rows[1];
        assert_eq!(assistant.0, "assistant");
        let content = content_of(&assistant.1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(
            content[0]["text"], "I will edit main.rs.",
            "streamed chunks are joined in arrival order"
        );
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "t1");
        assert_eq!(content[1]["name"], "Edit main.rs");
        assert_eq!(content[1]["input"]["path"], "src/main.rs");
        assert_eq!(assistant.1["message"]["stop_reason"], "end_turn");
        assert_eq!(assistant.1["parentUuid"], turn);

        let results = &rows[2];
        assert_eq!(results.0, "user");
        let content = content_of(&results.1);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "t1");
        assert_eq!(content[0]["content"], "wrote 3 lines");
        assert_eq!(content[0]["is_error"], false);
        assert_eq!(
            results.1["parentUuid"], assistant.1["uuid"],
            "the result row hangs off the assistant row that made the call"
        );
    }

    #[test]
    fn a_tool_the_agent_never_finished_still_gets_a_result() {
        // A dangling tool_use is a message no model will accept, so an
        // interrupted turn must still close its calls out.
        let root = temp_root("dangling");
        let journal = journal(root.path());
        let turn = journal.begin_turn(SESSION, &[text_block("go")]).unwrap();
        journal.record_update(
            SESSION,
            &tool_call("t1", "Edit", ToolCallStatus::InProgress),
        );
        journal.end_turn(SESSION, &turn, Some(StopReason::Cancelled));

        let rows = rows(root.path());
        let results = content_of(&rows[2].1);
        assert_eq!(results[0]["tool_use_id"], "t1");
        assert_eq!(results[0]["is_error"], true);
        assert!(
            results[0]["content"]
                .as_str()
                .unwrap()
                .contains("did not finish"),
            "the result should say why it is empty"
        );
        assert_eq!(rows[1].1["message"]["stop_reason"], "cancelled");
    }

    #[test]
    fn a_failed_tool_is_recorded_as_an_error_with_its_output() {
        let root = temp_root("failed-tool");
        let journal = journal(root.path());
        let turn = journal.begin_turn(SESSION, &[text_block("go")]).unwrap();
        journal.record_update(SESSION, &tool_call("t1", "Edit", ToolCallStatus::Pending));
        journal.record_update(
            SESSION,
            &SessionUpdate::ToolCallUpdate {
                tool_call_id: "t1".into(),
                status: Some(ToolCallStatus::Failed),
                title: Some("Edit main.rs".into()),
                content: None,
                locations: None,
                raw_output: Some(HashMap::from([(
                    "error".to_string(),
                    serde_json::json!("permission denied"),
                )])),
            },
        );
        journal.end_turn(SESSION, &turn, Some(StopReason::EndTurn));

        let rows = rows(root.path());
        assert_eq!(
            content_of(&rows[1].1)[0]["name"],
            "Edit main.rs",
            "a later title replaces the announced one"
        );
        let result = &content_of(&rows[2].1)[0];
        assert_eq!(result["is_error"], true);
        assert!(result["content"]
            .as_str()
            .unwrap()
            .contains("permission denied"));
    }

    #[test]
    fn an_update_for_a_call_that_was_never_announced_is_skipped() {
        // There is no input to pair it with, so half a tool call is
        // worse than none.
        let root = temp_root("orphan-update");
        let journal = journal(root.path());
        let turn = journal.begin_turn(SESSION, &[text_block("go")]).unwrap();
        journal.record_update(
            SESSION,
            &SessionUpdate::ToolCallUpdate {
                tool_call_id: "ghost".into(),
                status: Some(ToolCallStatus::Completed),
                title: None,
                content: None,
                locations: None,
                raw_output: None,
            },
        );
        journal.record_update(
            SESSION,
            &SessionUpdate::AgentMessageChunk {
                content: text_block("done"),
            },
        );
        journal.end_turn(SESSION, &turn, Some(StopReason::EndTurn));

        let rows = rows(root.path());
        assert_eq!(rows.len(), 2, "no tool-result row without a tool call");
        assert_eq!(content_of(&rows[1].1).len(), 1);
    }

    #[test]
    fn thinking_is_streamed_but_not_recorded() {
        // Extended-thinking blocks only replay with the signature that
        // produced them, which ACP does not carry.
        let root = temp_root("thinking");
        let journal = journal(root.path());
        let turn = journal.begin_turn(SESSION, &[text_block("go")]).unwrap();
        journal.record_update(
            SESSION,
            &SessionUpdate::ThinkingDelta { text: "hmm".into() },
        );
        journal.record_update(SESSION, &SessionUpdate::ThinkingEnd);
        journal.record_update(
            SESSION,
            &SessionUpdate::AgentMessageChunk {
                content: text_block("answer"),
            },
        );
        journal.end_turn(SESSION, &turn, Some(StopReason::EndTurn));

        let rows = rows(root.path());
        let content = content_of(&rows[1].1);
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "answer");
    }

    #[test]
    fn a_turn_with_no_output_writes_no_assistant_row() {
        let root = temp_root("silent");
        let journal = journal(root.path());
        let turn = journal.begin_turn(SESSION, &[text_block("go")]).unwrap();
        journal.end_turn(SESSION, &turn, Some(StopReason::EndTurn));
        assert_eq!(rows(root.path()).len(), 1, "just the prompt");
    }

    #[test]
    fn turns_chain_onto_the_previous_tail() {
        let root = temp_root("chain");
        let journal = journal(root.path());
        let first = journal.begin_turn(SESSION, &[text_block("one")]).unwrap();
        journal.record_update(
            SESSION,
            &SessionUpdate::AgentMessageChunk {
                content: text_block("a"),
            },
        );
        journal.end_turn(SESSION, &first, Some(StopReason::EndTurn));

        let second = journal.begin_turn(SESSION, &[text_block("two")]).unwrap();
        journal.end_turn(SESSION, &second, Some(StopReason::EndTurn));

        let rows = rows(root.path());
        assert_eq!(rows.len(), 3);
        assert!(rows[0].1["parentUuid"].is_null(), "the first row is a root");
        assert_eq!(rows[1].1["parentUuid"], first);
        assert_eq!(
            rows[2].1["parentUuid"], rows[1].1["uuid"],
            "the next prompt continues the chain instead of starting a second root"
        );
    }

    #[test]
    fn updates_outside_a_turn_are_dropped() {
        let root = temp_root("no-turn");
        let journal = journal(root.path());
        journal.record_update(
            SESSION,
            &SessionUpdate::AgentMessageChunk {
                content: text_block("late"),
            },
        );
        journal.end_turn(SESSION, "never-opened", Some(StopReason::EndTurn));
        assert!(rows(root.path()).is_empty());
    }

    #[test]
    fn an_out_of_order_turn_end_leaves_the_open_turn_alone() {
        let root = temp_root("out-of-order");
        let journal = journal(root.path());
        let first = journal.begin_turn(SESSION, &[text_block("one")]).unwrap();
        let second = journal.begin_turn(SESSION, &[text_block("two")]).unwrap();
        journal.record_update(
            SESSION,
            &SessionUpdate::AgentMessageChunk {
                content: text_block("belongs to the second turn"),
            },
        );

        // The first turn finishing late must neither claim the open
        // turn's output nor carry it off: matching happens before the
        // take, so the live turn keeps everything it has accumulated.
        journal.end_turn(SESSION, &first, Some(StopReason::EndTurn));
        assert_eq!(
            rows(root.path()).len(),
            2,
            "two prompts, no assistant row yet"
        );

        journal.end_turn(SESSION, &second, Some(StopReason::EndTurn));
        let rows = rows(root.path());
        assert_eq!(rows.len(), 3, "the second turn still flushes its answer");
        assert_eq!(
            content_of(&rows[2].1)[0]["text"],
            "belongs to the second turn"
        );
        assert_eq!(rows[2].1["parentUuid"], second);
    }

    #[test]
    fn independent_session_journals_can_finish_out_of_order_without_cross_contamination() {
        #[derive(Default)]
        struct HistorySpy(Mutex<Vec<(String, String, String)>>);
        impl HostFileHistory for HistorySpy {
            fn begin_turn(&self, session: &str, turn_id: &str) -> anyhow::Result<()> {
                self.0.lock().unwrap().push((
                    session.to_string(),
                    "begin".to_string(),
                    turn_id.to_string(),
                ));
                Ok(())
            }

            fn snapshot_before_write(&self, _session: &str, _path: &Path) -> anyhow::Result<()> {
                Ok(())
            }

            fn end_turn(&self, session: &str, turn_id: &str) -> anyhow::Result<()> {
                self.0.lock().unwrap().push((
                    session.to_string(),
                    "end".to_string(),
                    turn_id.to_string(),
                ));
                Ok(())
            }
        }

        fn session_rows(root: &Path, cwd: &str, session: &str) -> Vec<serde_json::Value> {
            let path = rebon_session::transcript_file_path(root, cwd, session);
            rebon_session::load_raw_transcript_from_file(&path)
                .unwrap()
                .unwrap()
                .entries
                .into_iter()
                .map(|entry| entry.raw)
                .collect()
        }

        let root = temp_root("parallel-session-bindings");
        let history = Arc::new(HistorySpy::default());
        let sink_rows: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let sink = {
            let sink_rows = sink_rows.clone();
            Arc::new(
                move |session: &str, entry: rebon_session::TranscriptEntry| {
                    sink_rows
                        .lock()
                        .unwrap()
                        .push((session.to_string(), entry.entry_type));
                },
            ) as TranscriptSink
        };
        let old = TranscriptJournal::new(root.path(), "/old", "sess-old", "agent")
            .with_file_history(history.clone())
            .with_transcript_sink(sink.clone());
        let new = TranscriptJournal::new(root.path(), "/new", "sess-new", "agent")
            .with_file_history(history.clone())
            .with_transcript_sink(sink);

        let old_turn = old
            .begin_turn("sess-old", &[text_block("old prompt")])
            .unwrap();
        let new_turn = new
            .begin_turn("sess-new", &[text_block("new prompt")])
            .unwrap();
        old.record_update(
            "sess-old",
            &SessionUpdate::AgentMessageChunk {
                content: text_block("old answer"),
            },
        );
        new.record_update(
            "sess-new",
            &SessionUpdate::AgentMessageChunk {
                content: text_block("new answer"),
            },
        );
        new.end_turn("sess-new", &new_turn, Some(StopReason::EndTurn));
        old.end_turn("sess-old", &old_turn, Some(StopReason::EndTurn));

        let old_rows = session_rows(root.path(), "/old", "sess-old");
        let new_rows = session_rows(root.path(), "/new", "sess-new");
        assert_eq!(content_of(&old_rows[0])[0]["text"], "old prompt");
        assert_eq!(content_of(&old_rows[1])[0]["text"], "old answer");
        assert_eq!(content_of(&new_rows[0])[0]["text"], "new prompt");
        assert_eq!(content_of(&new_rows[1])[0]["text"], "new answer");

        let history = history.0.lock().unwrap().clone();
        assert!(history.contains(&("sess-old".to_string(), "end".to_string(), old_turn)));
        assert!(history.contains(&("sess-new".to_string(), "end".to_string(), new_turn)));
        assert_eq!(
            sink_rows.lock().unwrap().as_slice(),
            &[
                ("sess-old".to_string(), "user".to_string()),
                ("sess-new".to_string(), "user".to_string()),
                ("sess-new".to_string(), "assistant".to_string()),
                ("sess-old".to_string(), "assistant".to_string()),
            ]
        );
    }

    #[test]
    fn a_journal_refuses_to_write_another_sessions_turn() {
        let root = temp_root("wrong-session");
        let journal = journal(root.path());
        assert!(journal
            .begin_turn("other-session", &[text_block("go")])
            .is_none());
        assert!(rows(root.path()).is_empty());
    }

    #[test]
    fn the_agents_own_session_id_is_remembered_for_the_next_run() {
        let root = temp_root("agent-session");
        let journal = journal(root.path());
        journal.record_agent_session(SESSION, "agent-side-42");
        assert_eq!(
            rebon_session::load_session_agent(root.path(), CWD, SESSION).as_deref(),
            Some("claude-code")
        );
        assert_eq!(
            rebon_session::load_agent_session_id(root.path(), CWD, SESSION).as_deref(),
            Some("agent-side-42")
        );
    }

    #[test]
    fn file_history_is_armed_for_the_turn_and_released_when_it_ends() {
        #[derive(Default)]
        struct Spy(Mutex<Vec<String>>);
        impl HostFileHistory for Spy {
            fn begin_turn(&self, _session: &str, turn_id: &str) -> anyhow::Result<()> {
                self.0.lock().unwrap().push(format!("begin {turn_id}"));
                Ok(())
            }
            fn snapshot_before_write(&self, _session: &str, _path: &Path) -> anyhow::Result<()> {
                Ok(())
            }
            fn end_turn(&self, _session: &str, turn_id: &str) -> anyhow::Result<()> {
                self.0.lock().unwrap().push(format!("end {turn_id}"));
                Ok(())
            }
        }

        let root = temp_root("file-history");
        let spy = Arc::new(Spy::default());
        let journal = TranscriptJournal::new(root.path(), CWD, SESSION, "claude-code")
            .with_file_history(spy.clone());
        let turn = journal.begin_turn(SESSION, &[text_block("go")]).unwrap();
        journal.end_turn(SESSION, &turn, Some(StopReason::EndTurn));

        assert_eq!(
            spy.0.lock().unwrap().clone(),
            vec![format!("begin {turn}"), format!("end {turn}")],
            "the snapshot boundary must bracket the turn, keyed on the user row"
        );
    }

    #[test]
    fn a_prompt_with_nothing_textual_still_produces_a_row() {
        // An image-only prompt is still a turn; a transcript that
        // skipped it would leave the assistant row parented on nothing.
        let root = temp_root("image-prompt");
        let journal = journal(root.path());
        let turn = journal
            .begin_turn(
                SESSION,
                &[ContentBlock::Image(rebon_types::ImageContent {
                    mime_type: "image/png".into(),
                    data: "abc".into(),
                    uri: None,
                    annotations: None,
                })],
            )
            .expect("turn opened");
        assert!(!turn.is_empty());
        assert_eq!(content_of(&rows(root.path())[0].1).len(), 1);
    }

    #[test]
    fn every_persisted_row_reaches_the_transcript_sink() {
        // The sink is what keeps the host's in-memory projection (and
        // its revision counter) in step with the disk — the difference
        // between the local engine replaying the ACP turns and
        // replaying a stale cache without them.
        let root = temp_root("sink");
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let sink_seen = seen.clone();
        let journal = TranscriptJournal::new(root.path(), CWD, SESSION, "claude-code")
            .with_transcript_sink(Arc::new(move |session, entry| {
                sink_seen
                    .lock()
                    .unwrap()
                    .push((session.to_string(), entry.entry_type.clone()));
            }));

        let turn = journal.begin_turn(SESSION, &[text_block("go")]).unwrap();
        journal.record_update(SESSION, &tool_call("t1", "Edit", ToolCallStatus::Completed));
        journal.end_turn(SESSION, &turn, Some(StopReason::EndTurn));

        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                (SESSION.to_string(), "user".to_string()),
                (SESSION.to_string(), "assistant".to_string()),
                (SESSION.to_string(), "user".to_string()),
            ],
            "every row that reached disk must reach the sink, in order"
        );
    }

    #[test]
    fn the_noop_journal_records_nothing_and_says_so() {
        let journal = NoopTurnJournal;
        assert!(journal.begin_turn("s", &[text_block("go")]).is_none());
        journal.record_update(
            "s",
            &SessionUpdate::AgentMessageChunk {
                content: text_block("x"),
            },
        );
        journal.end_turn("s", "t", None);
    }
}
