//! From an owner's event stream to session-stream frames.
//!
//! | `SessionEvent` | Frames |
//! |---|---|
//! | `hello` | the pending permission, if any, and `session_state` from its status |
//! | `sessionUpdate` | `session_message` whose `message` is the ACP `session/update` params as the owner sent them, coalesced, with a `message_id` naming the cursor range |
//! | `turn` | `session_state` (`running` / `idle`, `needs_input` while a prompt is pending) |
//! | `status` | the pending permission if it is new, and `session_state` if it changed |
//! | `permission` | `permission_request` with a request id bound to query, turn and endpoint, then `session_state needs_input` |
//! | `gap` | nothing yet: a backfill of the missing cursors from the job's event log, after which held deltas continue |
//!
//! ## Coalescing
//!
//! RC commits every frame, so a token per frame is a commit per token.
//! Consecutive text chunks of the same kind and turn are merged into one
//! chunk until [`DEFAULT_FLUSH_TEXT_BYTES`] of text or the caller's flush
//! tick, and consecutive `tool_call_update`s for one tool call are merged
//! field by field (a later update's fields replace an earlier one's, which
//! is what applying them in order would have done). Anything else — and
//! any event that is not an update — flushes what is pending first, so the
//! order a controller sees is the order the owner published.
//!
//! ## Exactly one frame per cursor
//!
//! The uplink keeps a watermark per stream identity: the highest cursor
//! already turned into a frame. A replay after a reconnect, or a backfill
//! that overlaps the live stream, never produces a second frame for a
//! cursor, and every frame's id is its cursor range — so what a resend
//! repeats, RC drops.

use std::sync::Arc;

use rebon_bridge::session_stream::{SessionFrame, SessionRunState};
use rebon_proto::PermissionOptionKind;
use rebon_session_host::{
    BackgroundPermissionQuerySnapshot, SessionEvent, SessionStatusSnapshot, TurnStreamState,
};
use serde_json::{json, Value};

use super::ids::{self, StreamIdentity};
use super::limits::{self, fit_message, split_text, FRAME_BUDGET_BYTES, TEXT_PIECE_BYTES};
use super::state::{reported_for_status, reported_for_turn, Reported, StateTracker};

/// How much text a merged chunk collects before it is sent without
/// waiting for the flush tick.
pub const DEFAULT_FLUSH_TEXT_BYTES: usize = 16 * 1024;

/// ACP `session/request_permission` params for one prompt. Injected: the
/// projection is the session runtime's, and one implementation of it is
/// enough.
pub type PermissionProjection =
    Arc<dyn Fn(&BackgroundPermissionQuerySnapshot) -> Value + Send + Sync>;

/// What the uplink wants done.
#[derive(Debug, Clone, PartialEq)]
pub enum UplinkOut {
    /// Send this frame.
    Frame(SessionFrame),
    /// Read the job's event log for the updates stamped `epoch` with a
    /// cursor strictly between `after` and `before`, and hand them to
    /// [`Uplink::on_backfill`].
    Backfill { epoch: u64, after: u64, before: u64 },
}

/// The permission prompt the owner is parked on, as it was announced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermission {
    pub request_id: String,
    pub query_id: u64,
}

/// The per-work-item uplink state machine.
pub struct Uplink {
    projection: PermissionProjection,
    coalescer: Coalescer,
    generation: String,
    identity: StreamIdentity,
    epoch: u64,
    /// The highest cursor of `identity` already accounted for.
    applied: u64,
    /// A gap is being backfilled up to (not including) this cursor; live
    /// deltas wait in `held` until it is.
    catch_up: Option<u64>,
    held: Vec<(u64, Value)>,
    held_bytes: usize,
    state: StateTracker,
    pending: Option<PendingPermission>,
    announced: Option<SessionFrame>,
}

impl Uplink {
    pub fn new(projection: PermissionProjection) -> Self {
        Self::with_flush_bytes(projection, DEFAULT_FLUSH_TEXT_BYTES)
    }

    pub fn with_flush_bytes(projection: PermissionProjection, flush_text_bytes: usize) -> Self {
        Self {
            projection,
            coalescer: Coalescer::new(flush_text_bytes),
            generation: String::new(),
            identity: StreamIdentity::unannounced(""),
            epoch: 0,
            applied: 0,
            catch_up: None,
            held: Vec::new(),
            held_bytes: 0,
            state: StateTracker::default(),
            pending: None,
            announced: None,
        }
    }

    /// The runner's own state (opening, stopped, failed to open).
    pub fn report(&mut self, reported: Reported) -> Vec<UplinkOut> {
        let mut out = self.flush();
        out.extend(self.state.observe(reported).map(UplinkOut::Frame));
        out
    }

    /// A subscription to the owner endpoint `generation` opened. Its
    /// `hello` says which numbering follows.
    pub fn on_attached(&mut self, generation: &str) -> Vec<UplinkOut> {
        let out = self.flush();
        self.generation = generation.to_string();
        out
    }

    /// The subscription ended: send what is pending. What the owner is
    /// doing is known again from the next `hello`.
    pub fn on_detached(&mut self) -> Vec<UplinkOut> {
        self.flush()
    }

    pub fn on_event(&mut self, event: SessionEvent) -> Vec<UplinkOut> {
        match event {
            SessionEvent::Hello {
                cursor,
                status,
                epoch,
                ..
            } => self.on_hello(cursor, epoch, &status),
            SessionEvent::SessionUpdate { cursor, update } => self.on_update(cursor, update),
            SessionEvent::Turn {
                state,
                stop_refused,
                ..
            } => {
                let mut out = self.flush();
                if state == TurnStreamState::Idle {
                    // A turn that ended is not waiting on anyone.
                    self.pending = None;
                    self.announced = None;
                }
                let reported =
                    reported_for_turn(state, stop_refused.as_deref(), self.pending.is_some());
                out.extend(self.state.observe(reported).map(UplinkOut::Frame));
                out
            }
            SessionEvent::Status { snapshot, .. } => {
                let mut out = self.flush();
                out.extend(self.apply_status(&snapshot));
                out
            }
            SessionEvent::Permission { query, .. } => {
                let mut out = self.flush();
                out.extend(self.announce(&query));
                out.extend(
                    self.state
                        .observe(Reported::new(SessionRunState::NeedsInput))
                        .map(UplinkOut::Frame),
                );
                out
            }
            SessionEvent::Gap { to, .. } => self.on_gap(to),
        }
    }

    /// The flush tick: send whatever the coalescer is holding.
    pub fn flush(&mut self) -> Vec<UplinkOut> {
        self.coalescer
            .flush(&self.identity)
            .into_iter()
            .map(UplinkOut::Frame)
            .collect()
    }

    /// The lines a [`UplinkOut::Backfill`] asked for, `(cursor, update)`,
    /// for the range ending at `before`. An empty list (the log had
    /// nothing, or could not be read) settles the range all the same: the
    /// transcript on the machine is the authority for what is missing.
    pub fn on_backfill(&mut self, before: u64, mut lines: Vec<(u64, Value)>) -> Vec<UplinkOut> {
        if self.catch_up != Some(before) {
            // Answer to a backfill a newer numbering made irrelevant.
            return Vec::new();
        }
        lines.sort_by_key(|(cursor, _)| *cursor);
        let mut out = Vec::new();
        for (cursor, update) in lines {
            if cursor > self.applied && cursor < before {
                out.extend(self.coalesce(cursor, update));
            }
        }
        self.applied = self.applied.max(before.saturating_sub(1));
        out.extend(self.flush());
        self.catch_up = None;
        self.held_bytes = 0;
        for (cursor, update) in std::mem::take(&mut self.held) {
            out.extend(self.on_update(cursor, update));
        }
        out
    }

    /// Whether a backfill is outstanding.
    pub fn catching_up(&self) -> bool {
        self.catch_up.is_some()
    }

    /// Bytes of live deltas waiting behind a backfill.
    pub fn held_bytes(&self) -> usize {
        self.held_bytes
    }

    /// What to say again on a fresh stream connection: the last state and
    /// the prompt still pending. Both are keyed or idempotent in effect.
    pub fn resend_on_reconnect(&self) -> Vec<SessionFrame> {
        self.announced
            .iter()
            .cloned()
            .chain(self.state.current())
            .collect()
    }

    /// The state last reported.
    pub fn state_now(&self) -> Option<SessionRunState> {
        self.state.last().map(|reported| reported.state.clone())
    }

    pub fn pending_permission(&self) -> Option<&PendingPermission> {
        self.pending.as_ref()
    }

    pub fn identity(&self) -> &StreamIdentity {
        &self.identity
    }

    pub fn applied(&self) -> u64 {
        self.applied
    }

    fn on_hello(
        &mut self,
        cursor: u64,
        epoch: u64,
        status: &SessionStatusSnapshot,
    ) -> Vec<UplinkOut> {
        let identity = StreamIdentity::of(epoch, &self.generation);
        let mut out = Vec::new();
        if identity != self.identity {
            out.extend(self.flush());
            self.identity = identity;
            self.epoch = epoch;
            // `hello.cursor` is the cursor just before the first event this
            // subscription delivers, so everything after it is new.
            self.applied = cursor;
            self.catch_up = None;
            self.held.clear();
            self.held_bytes = 0;
        }
        out.extend(self.apply_status(status));
        out
    }

    fn on_update(&mut self, cursor: u64, update: Value) -> Vec<UplinkOut> {
        if self.catch_up.is_some() {
            self.held_bytes += limits::json_len(&update);
            self.held.push((cursor, update));
            return Vec::new();
        }
        if cursor <= self.applied {
            return Vec::new();
        }
        self.coalesce(cursor, update)
    }

    fn coalesce(&mut self, cursor: u64, update: Value) -> Vec<UplinkOut> {
        self.applied = cursor;
        self.coalescer
            .push(&self.identity, cursor, update)
            .into_iter()
            .map(UplinkOut::Frame)
            .collect()
    }

    fn on_gap(&mut self, to: u64) -> Vec<UplinkOut> {
        if to <= self.applied.saturating_add(1) {
            return Vec::new();
        }
        let mut out = self.flush();
        if self.epoch == 0 || self.catch_up.is_some() {
            // An owner that does not stamp its log cannot be backfilled
            // from it, and a second gap while one is being filled is the
            // same missing range seen again. Either way the range is
            // settled; the transcript has it.
            if self.catch_up.is_none() {
                self.applied = to - 1;
            }
            tracing::debug!(
                identity = self.identity.as_str(),
                to,
                "rebon rc: missed updates that cannot be backfilled"
            );
            return out;
        }
        self.catch_up = Some(to);
        out.push(UplinkOut::Backfill {
            epoch: self.epoch,
            after: self.applied,
            before: to,
        });
        out
    }

    fn apply_status(&mut self, status: &SessionStatusSnapshot) -> Vec<UplinkOut> {
        let mut out = Vec::new();
        match &status.pending_permission {
            Some(query) => out.extend(self.announce(query)),
            None => {
                self.pending = None;
                self.announced = None;
            }
        }
        out.extend(
            self.state
                .observe(reported_for_status(status))
                .map(UplinkOut::Frame),
        );
        out
    }

    fn announce(&mut self, query: &BackgroundPermissionQuerySnapshot) -> Vec<UplinkOut> {
        let request_id = ids::permission_request_id(query);
        if self.pending.as_ref().map(|pending| &pending.request_id) == Some(&request_id) {
            return Vec::new();
        }
        let (request, _) = fit_message(
            permission_request_payload(query, &self.projection),
            FRAME_BUDGET_BYTES,
        );
        let frame = SessionFrame::PermissionRequest {
            request_id: request_id.clone(),
            request,
        };
        self.pending = Some(PendingPermission {
            request_id,
            query_id: query.query_id,
        });
        self.announced = Some(frame.clone());
        vec![UplinkOut::Frame(frame)]
    }
}

/// The frame type that answers a tool approval.
const PERMISSION_RESPONSE: &str = "permission_response";
/// The frame type that answers a question.
const QUESTION_RESPONSE: &str = "question_response";

/// The permission kinds a remote answer can select under the one-shot
/// policy. Anything that would leave a rule behind is not offered.
pub fn remotely_answerable(kind: &str) -> bool {
    matches!(
        rebon_session_host::parse_permission_option_kind(kind),
        Some(PermissionOptionKind::AllowOnce | PermissionOptionKind::RejectOnce)
    )
}

/// The `request` of a `permission_request`: the ACP
/// `session/request_permission` params, less the options a remote answer
/// cannot select, plus what the runner can do with an answer.
///
/// `_meta.rebonRc` is `{kind, answerable, oneShot, answerWith}`;
/// `answerWith` names the frame that answers the prompt.
///
/// A question (`AskUserQuestion`) carries its questions in `questions`
/// and is answered with a `question_response`, so it offers no options:
/// an allow means nothing to it, and a `permission_response` deny
/// declines it whatever it offered.
pub fn permission_request_payload(
    query: &BackgroundPermissionQuerySnapshot,
    projection: &PermissionProjection,
) -> Value {
    let mut value = projection(query);
    let questions = rebon_session_host::ask_user_questions_from_permission(query);
    let answerable_options: Vec<&str> = if questions.is_some() {
        Vec::new()
    } else {
        query
            .options
            .iter()
            .filter(|option| remotely_answerable(&option.kind))
            .map(|option| option.option_id.as_str())
            .collect()
    };
    if let Some(options) = value.get_mut("options").and_then(Value::as_array_mut) {
        options.retain(|option| {
            option
                .get("optionId")
                .and_then(Value::as_str)
                .is_some_and(|id| answerable_options.contains(&id))
        });
    }
    let (kind, answerable, answer_with) = if questions.is_some() {
        ("question", true, QUESTION_RESPONSE)
    } else {
        (
            "permission",
            !answerable_options.is_empty(),
            PERMISSION_RESPONSE,
        )
    };
    if let (Some(object), Some(questions)) = (value.as_object_mut(), questions.as_ref()) {
        object.insert(
            "questions".to_string(),
            serde_json::to_value(questions).expect("questions serialize"),
        );
    }
    if let Some(object) = value.as_object_mut() {
        let meta = object.entry("_meta").or_insert_with(|| json!({}));
        if let Some(meta) = meta.as_object_mut() {
            meta.insert(
                "rebonRc".to_string(),
                json!({
                    "kind": kind,
                    "answerable": answerable,
                    "oneShot": true,
                    "answerWith": answer_with,
                }),
            );
        }
    }
    value
}

// ─── Coalescing ────────────────────────────────────────────────────────

const TEXT_CHUNKS: [&str; 3] = [
    "agent_message_chunk",
    "agent_thought_chunk",
    "user_message_chunk",
];

#[derive(Debug, Clone, PartialEq)]
enum MergeKey {
    Text {
        kind: String,
        session: Value,
        turn: Value,
    },
    ToolCall {
        id: String,
        session: Value,
        turn: Value,
    },
    Alone,
}

fn merge_key(update: &Value) -> MergeKey {
    let inner = &update["update"];
    let session = update.get("sessionId").cloned().unwrap_or(Value::Null);
    let turn = update.get("turnGeneration").cloned().unwrap_or(Value::Null);
    let kind = inner.get("sessionUpdate").and_then(Value::as_str);
    match kind {
        Some(kind)
            if TEXT_CHUNKS.contains(&kind)
                && inner["content"]["type"] == "text"
                && inner["content"]["text"].is_string() =>
        {
            MergeKey::Text {
                kind: kind.to_string(),
                session,
                turn,
            }
        }
        Some("tool_call_update") => match inner.get("toolCallId").and_then(Value::as_str) {
            Some(id) => MergeKey::ToolCall {
                id: id.to_string(),
                session,
                turn,
            },
            None => MergeKey::Alone,
        },
        _ => MergeKey::Alone,
    }
}

fn text_of(message: &Value) -> Option<&str> {
    message["update"]["content"]["text"].as_str()
}

struct PendingUpdate {
    first: u64,
    last: u64,
    message: Value,
    key: MergeKey,
}

/// Merges runs of updates into fewer frames. See the module docs.
pub struct Coalescer {
    flush_text_bytes: usize,
    pending: Option<PendingUpdate>,
}

impl Coalescer {
    pub fn new(flush_text_bytes: usize) -> Self {
        Self {
            flush_text_bytes,
            pending: None,
        }
    }

    /// Take one update. Returns whatever this made ready to send.
    pub fn push(
        &mut self,
        identity: &StreamIdentity,
        cursor: u64,
        update: Value,
    ) -> Vec<SessionFrame> {
        let key = merge_key(&update);
        let mut out = Vec::new();
        if let Some(pending) = self.pending.as_mut() {
            if pending.key == key && key != MergeKey::Alone && merge_into(pending, &update) {
                pending.last = cursor;
                if self.is_full() {
                    out.extend(self.flush(identity));
                }
                return out;
            }
            out.extend(self.flush(identity));
        }
        let pending = PendingUpdate {
            first: cursor,
            last: cursor,
            message: update,
            key,
        };
        if pending.key == MergeKey::Alone {
            out.extend(frames_for(identity, pending));
        } else {
            self.pending = Some(pending);
            if self.is_full() {
                out.extend(self.flush(identity));
            }
        }
        out
    }

    /// Everything pending, as frames.
    pub fn flush(&mut self, identity: &StreamIdentity) -> Vec<SessionFrame> {
        self.pending
            .take()
            .map(|pending| frames_for(identity, pending))
            .unwrap_or_default()
    }

    fn is_full(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| match pending.key {
                MergeKey::Text { .. } => {
                    text_of(&pending.message).map_or(0, str::len) >= self.flush_text_bytes
                }
                _ => limits::json_len(&pending.message) >= FRAME_BUDGET_BYTES / 2,
            })
    }
}

/// Merge `update` into `pending` if that keeps it sendable. `false` leaves
/// `pending` untouched.
fn merge_into(pending: &mut PendingUpdate, update: &Value) -> bool {
    match pending.key {
        MergeKey::Text { .. } => {
            let Some(addition) = text_of(update) else {
                return false;
            };
            let Some(text) = pending.message["update"]["content"]["text"].as_str() else {
                return false;
            };
            let merged = format!("{text}{addition}");
            pending.message["update"]["content"]["text"] = Value::String(merged);
            true
        }
        MergeKey::ToolCall { .. } => {
            if limits::json_len(&pending.message) + limits::json_len(update)
                > FRAME_BUDGET_BYTES / 2
            {
                return false;
            }
            let (Some(target), Some(source)) = (
                pending.message["update"].as_object_mut(),
                update["update"].as_object(),
            ) else {
                return false;
            };
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
            true
        }
        MergeKey::Alone => false,
    }
}

fn frames_for(identity: &StreamIdentity, pending: PendingUpdate) -> Vec<SessionFrame> {
    let id = ids::message_id(identity, pending.first, pending.last);
    if let MergeKey::Text { .. } = pending.key {
        let text = text_of(&pending.message).unwrap_or_default().to_string();
        if text.len() > TEXT_PIECE_BYTES {
            return split_text(&text, TEXT_PIECE_BYTES)
                .into_iter()
                .enumerate()
                .map(|(index, piece)| {
                    let mut message = pending.message.clone();
                    message["update"]["content"]["text"] = Value::String(piece.to_string());
                    let (message, _) = fit_message(message, FRAME_BUDGET_BYTES);
                    SessionFrame::message_with_id(ids::message_piece_id(&id, index), message)
                })
                .collect();
        }
    }
    let (message, fit) = fit_message(pending.message, FRAME_BUDGET_BYTES);
    if fit != limits::Fit::Unchanged {
        tracing::debug!(message_id = %id, ?fit, "rebon rc: a session update was cut to fit a frame");
    }
    vec![SessionFrame::message_with_id(id, message)]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::state::tests::status;
    use rebon_session_host::{
        BackgroundIpcEndpoint, BackgroundJobStatus, BackgroundPermissionOptionSnapshot,
    };

    pub(crate) fn query(query_id: u64, turn: u64) -> BackgroundPermissionQuerySnapshot {
        BackgroundPermissionQuerySnapshot {
            query_id,
            turn_generation: turn,
            endpoint: Some(BackgroundIpcEndpoint {
                pid: 7,
                port: 4000,
                token: "tok".into(),
            }),
            tool: Some("Bash".into()),
            tool_call_id: Some("call-1".into()),
            session_id: Some("s-1".into()),
            title: Some("Run a command".into()),
            message: None,
            tool_input: Some(json!({"command": "ls"})),
            metadata: None,
            options: vec![
                option("allow", "allow_once"),
                option("always", "allow_always"),
                option("deny", "reject_once"),
                option("never", "reject_always"),
            ],
        }
    }

    /// A two-question `AskUserQuestion` prompt: a single-select one and
    /// a multi-select one, each with three options.
    pub(crate) fn question(query_id: u64, turn: u64) -> BackgroundPermissionQuerySnapshot {
        let mut question = query(query_id, turn);
        question.tool = Some("AskUserQuestion".into());
        question.tool_input = Some(json!({"questions": [
            {
                "header": "Pick", "question": "Which?", "multiSelect": false,
                "options": [
                    {"label": "A", "description": "a"},
                    {"label": "B", "description": "b"},
                    {"label": "C", "description": "c"}
                ]
            },
            {
                "header": "Days", "question": "When?", "multiSelect": true,
                "options": [
                    {"label": "Mon", "description": "monday"},
                    {"label": "Tue", "description": "tuesday"},
                    {"label": "Wed", "description": "wednesday"}
                ]
            }
        ]}));
        question.options = vec![option("allow", "allow_once")];
        question
    }

    fn option(id: &str, kind: &str) -> BackgroundPermissionOptionSnapshot {
        BackgroundPermissionOptionSnapshot {
            option_id: id.into(),
            label: id.into(),
            kind: kind.into(),
        }
    }

    /// The projection the session runtime provides, reduced to what these
    /// tests look at.
    pub(crate) fn projection() -> PermissionProjection {
        Arc::new(|query: &BackgroundPermissionQuerySnapshot| {
            json!({
                "sessionId": query.session_id,
                "toolCall": {"toolCallId": query.tool_call_id},
                "toolName": query.tool,
                "toolInput": query.tool_input,
                "options": query.options.iter().map(|option| json!({
                    "optionId": option.option_id,
                    "name": option.label,
                    "kind": option.kind,
                })).collect::<Vec<_>>(),
            })
        })
    }

    fn chunk(kind: &str, text: &str, turn: u64) -> Value {
        json!({
            "sessionId": "s-1",
            "turnGeneration": turn,
            "update": {"sessionUpdate": kind, "content": {"type": "text", "text": text}}
        })
    }

    fn tool_update(id: &str, fields: Value) -> Value {
        let mut update = json!({"sessionUpdate": "tool_call_update", "toolCallId": id});
        for (key, value) in fields.as_object().unwrap() {
            update[key] = value.clone();
        }
        json!({"sessionId": "s-1", "turnGeneration": 1, "update": update})
    }

    fn hello(cursor: u64, epoch: u64) -> SessionEvent {
        SessionEvent::Hello {
            cursor,
            turn_generation: 1,
            status: Box::new(status(BackgroundJobStatus::Idle, false)),
            epoch,
        }
    }

    fn update(cursor: u64, update: Value) -> SessionEvent {
        SessionEvent::SessionUpdate { cursor, update }
    }

    fn frames(out: Vec<UplinkOut>) -> Vec<SessionFrame> {
        out.into_iter()
            .map(|out| match out {
                UplinkOut::Frame(frame) => frame,
                other => panic!("expected only frames, got {other:?}"),
            })
            .collect()
    }

    fn attached(epoch: u64) -> Uplink {
        let mut uplink = Uplink::new(projection());
        assert!(uplink.on_attached("gen1").is_empty());
        let out = frames(uplink.on_event(hello(10, epoch)));
        assert_eq!(out, vec![Reported::new(SessionRunState::Idle).frame()]);
        uplink
    }

    fn message_ids(frames: &[SessionFrame]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|frame| match frame {
                SessionFrame::SessionMessage { message_id, .. } => message_id.clone(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn text_chunks_merge_until_flushed() {
        let mut uplink = attached(0x5);
        assert!(uplink
            .on_event(update(11, chunk("agent_message_chunk", "Hel", 1)))
            .is_empty());
        assert!(uplink
            .on_event(update(12, chunk("agent_message_chunk", "lo", 1)))
            .is_empty());
        let flushed = frames(uplink.flush());
        assert_eq!(
            flushed,
            vec![SessionFrame::message_with_id(
                "e5.11-12",
                chunk("agent_message_chunk", "Hello", 1)
            )]
        );
        assert!(uplink.flush().is_empty());
    }

    #[test]
    fn a_different_kind_or_turn_starts_a_new_frame() {
        let mut uplink = attached(0x5);
        uplink.on_event(update(11, chunk("agent_thought_chunk", "think", 1)));
        let out = frames(uplink.on_event(update(12, chunk("agent_message_chunk", "say", 1))));
        assert_eq!(message_ids(&out), vec!["e5.11-11"]);
        let out = frames(uplink.on_event(update(13, chunk("agent_message_chunk", "again", 2))));
        assert_eq!(message_ids(&out), vec!["e5.12-12"]);
        let out = frames(uplink.flush());
        assert_eq!(
            out,
            vec![SessionFrame::message_with_id(
                "e5.13-13",
                chunk("agent_message_chunk", "again", 2)
            )]
        );
    }

    #[test]
    fn enough_text_is_sent_without_waiting_for_the_tick() {
        let mut uplink = Uplink::with_flush_bytes(projection(), 8);
        uplink.on_attached("g");
        uplink.on_event(hello(0, 3));
        assert!(uplink
            .on_event(update(1, chunk("agent_message_chunk", "1234", 1)))
            .is_empty());
        let out = frames(uplink.on_event(update(2, chunk("agent_message_chunk", "5678", 1))));
        assert_eq!(
            out,
            vec![SessionFrame::message_with_id(
                "e3.1-2",
                chunk("agent_message_chunk", "12345678", 1)
            )]
        );
    }

    #[test]
    fn tool_call_updates_merge_field_by_field() {
        let mut uplink = attached(0x5);
        uplink.on_event(update(
            11,
            tool_update("call-1", json!({"status": "in_progress"})),
        ));
        uplink.on_event(update(12, tool_update("call-1", json!({"content": [1]}))));
        uplink.on_event(update(
            13,
            tool_update("call-1", json!({"status": "completed"})),
        ));
        let out = frames(uplink.on_event(update(
            14,
            tool_update("call-2", json!({"status": "pending"})),
        )));
        assert_eq!(
            out,
            vec![SessionFrame::message_with_id(
                "e5.11-13",
                tool_update("call-1", json!({"status": "completed", "content": [1]}))
            )]
        );
    }

    #[test]
    fn other_updates_go_out_one_by_one_and_in_order() {
        let mut uplink = attached(0x5);
        uplink.on_event(update(11, chunk("agent_message_chunk", "a", 1)));
        let tool_call = json!({"sessionId": "s-1", "update": {"sessionUpdate": "tool_call", "toolCallId": "c"}});
        let out = frames(uplink.on_event(update(12, tool_call.clone())));
        assert_eq!(message_ids(&out), vec!["e5.11-11", "e5.12-12"]);
        assert_eq!(out[1], SessionFrame::message_with_id("e5.12-12", tool_call));
    }

    #[test]
    fn a_replayed_cursor_never_makes_a_second_frame() {
        let mut uplink = attached(0x5);
        let plan = json!({"update": {"sessionUpdate": "plan"}});
        assert_eq!(frames(uplink.on_event(update(11, plan.clone()))).len(), 1);
        // The same subscription replayed after a reconnect.
        uplink.on_detached();
        uplink.on_attached("gen1");
        let out = frames(uplink.on_event(hello(10, 0x5)));
        assert!(out.is_empty(), "same state, nothing to say: {out:?}");
        assert!(uplink.on_event(update(11, plan.clone())).is_empty());
        assert_eq!(
            message_ids(&frames(uplink.on_event(update(12, plan)))),
            vec!["e5.12-12"]
        );
        // Anything at or below the hello of a fresh numbering is old.
        assert!(uplink.on_event(update(3, json!({}))).is_empty());
    }

    #[test]
    fn a_new_epoch_starts_a_new_numbering() {
        let mut uplink = attached(0x5);
        let plan = json!({"update": {"sessionUpdate": "plan"}});
        uplink.on_event(update(40, plan.clone()));
        uplink.on_attached("gen2");
        uplink.on_event(hello(0, 0x6));
        assert_eq!(uplink.identity().as_str(), "e6");
        // Cursor 1 of the new worker is new, and its id cannot collide with
        // the old worker's cursor 1.
        assert_eq!(
            message_ids(&frames(uplink.on_event(update(1, plan)))),
            vec!["e6.1-1"]
        );
    }

    #[test]
    fn an_unstamped_owner_is_numbered_by_its_endpoint() {
        let mut uplink = Uplink::new(projection());
        uplink.on_attached("abc");
        uplink.on_event(hello(0, 0));
        let plan = json!({"update": {"sessionUpdate": "plan"}});
        assert_eq!(
            message_ids(&frames(uplink.on_event(update(1, plan.clone())))),
            vec!["gabc.1-1"]
        );
        // A replaced worker with the same (absent) epoch is still different.
        uplink.on_attached("def");
        uplink.on_event(hello(0, 0));
        assert_eq!(
            message_ids(&frames(uplink.on_event(update(1, plan)))),
            vec!["gdef.1-1"]
        );
    }

    #[test]
    fn a_gap_is_backfilled_before_live_deltas_continue() {
        let mut uplink = attached(0x5);
        let plan = |n: u64| json!({"update": {"sessionUpdate": "plan", "n": n}});
        uplink.on_event(update(11, plan(11)));
        let out = uplink.on_event(SessionEvent::Gap { from: 11, to: 20 });
        assert_eq!(
            out,
            vec![UplinkOut::Backfill {
                epoch: 5,
                after: 11,
                before: 20
            }]
        );
        assert!(uplink.catching_up());
        // Live deltas past the gap wait.
        assert!(uplink.on_event(update(20, plan(20))).is_empty());
        assert!(uplink.on_event(update(21, plan(21))).is_empty());
        assert!(uplink.held_bytes() > 0);
        // An answer for some other range is ignored.
        assert!(uplink.on_backfill(99, vec![(12, plan(12))]).is_empty());
        // The log had 12, 13 and (already sent) 11, out of order.
        let out = frames(uplink.on_backfill(
            20,
            vec![
                (13, plan(13)),
                (11, plan(11)),
                (12, plan(12)),
                (25, plan(25)),
            ],
        ));
        assert_eq!(
            message_ids(&out),
            vec!["e5.12-12", "e5.13-13", "e5.20-20", "e5.21-21"]
        );
        assert!(!uplink.catching_up());
        assert_eq!(uplink.held_bytes(), 0);
        assert_eq!(uplink.applied(), 21);
    }

    #[test]
    fn a_gap_that_cannot_be_backfilled_is_settled() {
        let mut uplink = Uplink::new(projection());
        uplink.on_attached("g");
        uplink.on_event(hello(0, 0));
        assert!(uplink
            .on_event(SessionEvent::Gap { from: 0, to: 9 })
            .is_empty());
        assert_eq!(uplink.applied(), 8);
        assert!(!uplink.catching_up());
        // A gap that is not one.
        let mut uplink = attached(0x5);
        assert!(uplink
            .on_event(SessionEvent::Gap { from: 10, to: 11 })
            .is_empty());
        // A second gap while one is outstanding asks for nothing more.
        assert_eq!(
            uplink
                .on_event(SessionEvent::Gap { from: 10, to: 30 })
                .len(),
            1
        );
        assert!(uplink
            .on_event(SessionEvent::Gap { from: 10, to: 40 })
            .is_empty());
        // An empty backfill settles the range.
        assert!(uplink.on_backfill(30, Vec::new()).is_empty());
        assert_eq!(uplink.applied(), 29);
    }

    #[test]
    fn a_permission_is_announced_once_with_one_shot_options() {
        let mut uplink = attached(0x5);
        let out = frames(uplink.on_event(SessionEvent::Permission {
            cursor: 11,
            query: Box::new(query(3, 1)),
        }));
        assert_eq!(out.len(), 2);
        let SessionFrame::PermissionRequest {
            request_id,
            request,
        } = &out[0]
        else {
            panic!("expected a permission request, got {:?}", out[0]);
        };
        assert_eq!(request_id, &ids::permission_request_id(&query(3, 1)));
        let offered: Vec<&str> = request["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["optionId"].as_str().unwrap())
            .collect();
        assert_eq!(offered, vec!["allow", "deny"]);
        assert_eq!(request["_meta"]["rebonRc"]["kind"], "permission");
        assert_eq!(request["_meta"]["rebonRc"]["answerable"], true);
        assert!(
            !request.to_string().contains("tok"),
            "the endpoint token stays home"
        );
        assert_eq!(out[1], Reported::new(SessionRunState::NeedsInput).frame());
        assert_eq!(
            uplink.pending_permission(),
            Some(&PendingPermission {
                request_id: request_id.clone(),
                query_id: 3
            })
        );

        // The same prompt reported again by a status is not announced again.
        let mut waiting = status(BackgroundJobStatus::NeedsInput, true);
        waiting.pending_permission = Some(query(3, 1));
        assert!(uplink
            .on_event(SessionEvent::Status {
                cursor: 12,
                snapshot: Box::new(waiting.clone())
            })
            .is_empty());
        // A reconnect says it again, with the state.
        assert_eq!(
            uplink.resend_on_reconnect(),
            vec![
                out[0].clone(),
                Reported::new(SessionRunState::NeedsInput).frame()
            ]
        );

        // A replaced prompt with the same query id is a new request.
        waiting.pending_permission = Some(query(3, 2));
        let out = frames(uplink.on_event(SessionEvent::Status {
            cursor: 13,
            snapshot: Box::new(waiting),
        }));
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], SessionFrame::PermissionRequest { request_id, .. }
            if request_id == &ids::permission_request_id(&query(3, 2)))
        );

        // Resolved: the status no longer has it, and the state moves on.
        let out = frames(uplink.on_event(SessionEvent::Status {
            cursor: 14,
            snapshot: Box::new(status(BackgroundJobStatus::Running, true)),
        }));
        assert_eq!(out, vec![Reported::new(SessionRunState::Running).frame()]);
        assert!(uplink.pending_permission().is_none());
        assert_eq!(
            uplink.resend_on_reconnect(),
            vec![Reported::new(SessionRunState::Running).frame()]
        );
    }

    #[test]
    fn a_question_is_answered_with_a_question_response() {
        let payload = permission_request_payload(&question(4, 1), &projection());
        assert_eq!(
            payload["_meta"]["rebonRc"],
            json!({
                "kind": "question",
                "answerable": true,
                "oneShot": true,
                "answerWith": "question_response",
            })
        );
        // No options: a question is answered, or denied, not allowed.
        assert_eq!(payload["options"], json!([]));
        assert_eq!(payload["questions"][0]["question"], "Which?");
        assert_eq!(payload["questions"][1]["multiSelect"], true);
        assert_eq!(payload["questions"][1]["options"][2]["label"], "Wed");
        assert_eq!(
            QUESTION_RESPONSE,
            rebon_bridge::session_stream::SessionFrame::QuestionResponse {
                request_id: String::new(),
                answers: Vec::new(),
            }
            .frame_type()
        );
    }

    #[test]
    fn a_malformed_question_is_a_permission_prompt() {
        // Not the shape a question takes: shown as the tool call it is.
        let mut broken = question(4, 1);
        broken.tool_input = Some(json!({"questions": "Which?"}));
        let payload = permission_request_payload(&broken, &projection());
        assert_eq!(payload["_meta"]["rebonRc"]["kind"], "permission");
        assert_eq!(
            payload["_meta"]["rebonRc"]["answerWith"],
            "permission_response"
        );
        assert!(payload.get("questions").is_none());
    }

    #[test]
    fn turns_move_the_state_and_an_idle_turn_clears_the_prompt() {
        let mut uplink = attached(0x5);
        let out = frames(uplink.on_event(SessionEvent::Turn {
            cursor: 11,
            state: TurnStreamState::Running,
            stop_reason: None,
            stop_refused: None,
        }));
        assert_eq!(out, vec![Reported::new(SessionRunState::Running).frame()]);
        uplink.on_event(SessionEvent::Permission {
            cursor: 12,
            query: Box::new(query(1, 1)),
        });
        // Still running, but waiting: the prompt decides.
        let out = frames(uplink.on_event(SessionEvent::Turn {
            cursor: 13,
            state: TurnStreamState::Running,
            stop_reason: None,
            stop_refused: None,
        }));
        assert!(out.is_empty());
        let out = frames(uplink.on_event(SessionEvent::Turn {
            cursor: 14,
            state: TurnStreamState::Idle,
            stop_reason: Some("end_turn".into()),
            stop_refused: None,
        }));
        assert_eq!(out, vec![Reported::new(SessionRunState::Idle).frame()]);
        assert!(uplink.pending_permission().is_none());
    }

    #[test]
    fn a_non_update_event_flushes_first() {
        let mut uplink = attached(0x5);
        uplink.on_event(update(11, chunk("agent_message_chunk", "done", 1)));
        let out = frames(uplink.on_event(SessionEvent::Turn {
            cursor: 12,
            state: TurnStreamState::Idle,
            stop_reason: None,
            stop_refused: None,
        }));
        assert_eq!(message_ids(&out), vec!["e5.11-11"]);
        // The turn itself did not change the (idle) state.
        assert_eq!(out.len(), 1);
        // The runner's own states go through the same tracker, flushed first.
        uplink.on_event(update(13, chunk("agent_message_chunk", "x", 1)));
        let out = frames(uplink.report(Reported::with_detail(SessionRunState::Stopped, "bye")));
        assert_eq!(message_ids(&out), vec!["e5.13-13"]);
        assert_eq!(
            out[1],
            Reported::with_detail(SessionRunState::Stopped, "bye").frame()
        );
    }

    #[test]
    fn a_huge_text_chunk_is_split_into_pieces_that_join_back() {
        let mut uplink = attached(0x5);
        let text = "y".repeat(TEXT_PIECE_BYTES * 2 + 5);
        let out = frames(uplink.on_event(update(11, chunk("agent_message_chunk", &text, 1))));
        assert_eq!(out.len(), 3);
        assert_eq!(
            message_ids(&out),
            vec!["e5.11-11.p0", "e5.11-11.p1", "e5.11-11.p2"]
        );
        let joined: String = out
            .iter()
            .map(|frame| match frame {
                SessionFrame::SessionMessage { message, .. } => message["update"]["content"]
                    ["text"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(joined, text);
    }

    #[test]
    fn every_frame_fits_the_cap() {
        let mut uplink = attached(0x5);
        let huge = tool_update(
            "c",
            json!({"rawOutput": "z".repeat(2 * limits::MAX_FRAME_BYTES)}),
        );
        let out = frames(uplink.on_event(update(11, huge)));
        assert_eq!(out.len(), 1);
        let text = serde_json::to_string(&out[0]).unwrap();
        assert!(text.len() < limits::MAX_FRAME_BYTES, "{}", text.len());
    }
}
