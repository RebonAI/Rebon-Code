//! The session stream's frame envelope.
//!
//! `GET /v1/sessions/{id}/stream` is a WebSocket carrying JSON text
//! frames in both directions. The envelope is defined in this crate alone,
//! so the server and the worker client both deserialize *this* type — a
//! change to a variant breaks both builds at once.
//!
//! ## Frames
//!
//! One enum, not two: direction is a property of the variant rather than
//! of the type, which is what lets a single [`SessionFrame`] be the
//! inbound *and* outbound type on either end. [`SessionFrame::origin`]
//! names the side a frame is allowed to come from, and the ingress
//! rejects a frame whose origin does not match the sender's role.
//!
//! | Origin | `type` | Payload |
//! |---|---|---|
//! | worker | `session_message` | `message` — opaque projection; optional `message_id` |
//! | worker | `session_state` | `state` — [`SessionRunState`], optional `detail` |
//! | worker | `permission_request` | `request_id`, `request` — opaque |
//! | worker | `control_response` | `response` — [`ControlResponseBody`] |
//! | worker | `session_bound` | `rebon_session_id` — the machine-local session this RC session runs as |
//! | controller | `prompt` | `text`, optional `attachments` — opaque |
//! | controller | `cancel` | — |
//! | controller | `permission_response` | `response` — [`crate::config::PermissionResponseBody`] |
//! | controller | `question_response` | `request_id`, `answers` — one [`QuestionAnswer`] per question |
//! | controller | `control_request` | `request_id`, `subtype`, optional `params` |
//! | server | `stream_error` | `code`, `message`; optional `request_id`, `answered_by` ([`AnsweredBy`]) |
//!
//! `session_message`, `request`, `attachments` and `params` are
//! deliberately `serde_json::Value`: the session runner picks their shape,
//! and this crate must not grow SDK message types — the
//! `dependency_contract` test in `lib.rs` is what keeps it a leaf crate.
//!
//! ## Frame identity
//!
//! Two ids, owned by the two ends that can make them up:
//!
//! * **`event_id` — the server's.** Every frame RC persists is stored
//!   under an event id, the same id [`crate::history`] pages by. When RC
//!   delivers a persisted frame — live or in the attach-time replay — it
//!   adds that id as a top-level `event_id` key, so a controller that
//!   reads history pages *and* the live socket can drop the overlap.
//!   The stored payload is still the frame exactly as sent; the id is
//!   added on the way out ([`stamp_event_id`]) and read by
//!   [`DeliveredFrame`]. A frame that was never persisted (a
//!   `stream_error`) carries none. `event_id` is therefore **reserved**:
//!   no frame type may define it, and RC closes a peer that sends it
//!   with [`close_code::PROTOCOL_ERROR`].
//! * **`message_id` — the worker's.** A `session_message` may carry a
//!   worker-chosen id. RC stores at most one frame per
//!   [`SessionFrame::idempotency_key`] per session, so a worker that
//!   reconnects and resends what it is unsure about does not duplicate
//!   the history. `permission_request` is keyed by its `request_id` the
//!   same way. A resend of something already stored is **acknowledged by
//!   silence**: it is not stored again, not delivered again, and the
//!   socket stays open — the history already holds the frame exactly
//!   once, which is the state the worker asked for. An id is 1 to
//!   [`MAX_FRAME_ID_BYTES`] bytes; anything else is a protocol error.
//!
//! ## Which local session an RC session is
//!
//! A runner that opens or resumes a Rebon session for a work item says
//! which one with a `session_bound` frame. RC records the id on the
//! session, so work queued for the session later — a `reconnect`, or a
//! prompt queued while no worker was attached — continues *that* local
//! session instead of starting a new one. The id is validated like
//! [`crate::config::SessionWork::resume_rebon_session_id`] (it ends up
//! naming something on the machine's disk), and the frame is keyed on it,
//! so a runner may resend it after every reconnect.
//!
//! ## Answering a question
//!
//! A `permission_request` may be a *question* the agent asks
//! (`AskUserQuestion`) rather than a tool approval. An allow / deny
//! decision cannot answer it, so a controller answers with a
//! `question_response` instead: the prompt's `request_id`, and one
//! [`QuestionAnswer`] per question, in the order the request listed them.
//!
//! ```json
//! {"type": "question_response", "request_id": "perm-…",
//!  "answers": [{"selected_options": [1]}, {"other_text": "on Fridays"}]}
//! ```
//!
//! The request says which frame answers it. The runner puts
//! `_meta.rebonRc = {kind, answerable, oneShot, answerWith}` in the
//! request payload; a question is `kind: "question"` with the questions
//! in a top-level `questions` array, and `answerWith` names the frame
//! type (`question_response`, or `permission_response` for a tool
//! approval). A runner from before this frame marks questions
//! `answerable: false` and names no `answerWith`, so a controller that
//! only sends `question_response` when asked to never sends one to a
//! runner that would drop it. Declining a question is a
//! `permission_response` with `behavior: deny`.
//!
//! The frame is added without breaking older peers, because an unknown
//! `type` is not a failure (below): an older runner reads it as
//! [`SessionFrame::Other`] and ignores it, and an older RC server routes
//! it by the sender's role, which is the controller's.
//!
//! A refused answer — the prompt is no longer pending, the answers do not
//! fit the questions, the session is not reachable — comes back as a
//! `control_response` error echoing the `request_id`, exactly as a
//! refused permission decision does; the prompt stays pending. An answer
//! that lands produces no reply of its own: the session leaving
//! `needs_input` is the acknowledgement.
//!
//! ## The first answer wins
//!
//! Several controllers may be attached to one session, and every one of
//! them sees the same `permission_request`. RC takes the first answer to
//! a `request_id` — a `permission_response` or a `question_response`,
//! [`SessionFrame::answered_request_id`] — and refuses the ones that
//! come after it from any other connection with a `stream_error` whose
//! `code` is [`error_code::ALREADY_ANSWERED`]. That refusal names the
//! `request_id` and says who answered in `answered_by`
//! ([`AnsweredBy`]: a device id and label, never a credential). The
//! winning answer is mirrored to the other controllers like any other
//! controller frame, so their prompt can close as it arrives.
//!
//! A claim is not final until the runner has had its say. When it
//! refuses the answer (a `control_response` error echoing the
//! `request_id`), RC drops the claim and the next answer is taken. The
//! connection holding a claim may send again, so a controller can retry
//! without being told it lost to itself.
//!
//! Both fields are optional on the wire and read as absent when missing,
//! so a controller built before them still parses the refusal as the
//! `stream_error` it always knew, and a newer one reads an older
//! server's `stream_error` without them.
//!
//! ## An unknown `type` is not a failure
//!
//! A frame whose `type` this build does not know deserializes into
//! [`SessionFrame::Other`], which keeps the object verbatim and
//! re-serializes it byte-for-byte. The ingress has to survive a peer
//! that is one release ahead; dropping the connection because a newer
//! controller sent a frame we have no variant for would make every
//! protocol addition a flag day. A frame that *is* known but malformed
//! still fails — that is a bug on the wire, not a version skew.

use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::config::PermissionResponseBody;
use crate::control_request::{
    ControlApplied, ControlEffect, ServerControlRequestSubtype, ServerControlResponsePlan,
};

/// The top-level key RC adds to a persisted frame when it delivers it.
/// Reserved: see the module docs.
pub const EVENT_ID_KEY: &str = "event_id";

/// Longest `message_id` (and keyed `request_id`) RC accepts, in bytes.
pub const MAX_FRAME_ID_BYTES: usize = 256;

/// WebSocket close codes the session stream defines.
///
/// They live here rather than in the server because the *client* is what
/// has to act on them, and a runner that cannot tell "superseded" from
/// "your lease is gone" would reconnect into a loop. 1000 is the
/// ordinary close and needs no constant.
pub mod close_code {
    /// Liveness: no pong inside the deadline, or no traffic at all for
    /// the idle window. Transient — reconnect.
    pub const TIMEOUT: u16 = 4408;
    /// The work item backing this session token stopped being leased
    /// while the worker was attached. Permanent for this credential:
    /// stand down, do not reconnect with the same token.
    pub const LEASE_GONE: u16 = 4409;
    /// A frame over the size cap, or an outbound queue that overflowed
    /// because this peer stopped reading. Transient.
    pub const RESOURCE_LIMIT: u16 = 4413;
    /// A non-text frame, malformed JSON, or a frame whose origin does
    /// not match the sender's role. A bug, not a version skew.
    pub const PROTOCOL_ERROR: u16 = 4422;
    /// A newer worker connection took this session's worker slot.
    /// Transient from the session's point of view, terminal for *this*
    /// connection: reconnecting would evict the worker that replaced us.
    pub const SUPERSEDED: u16 = 4426;
}

/// The `code` values of a `stream_error` frame.
///
/// Stable wire words a controller branches on; the `message` next to
/// them is for people.
pub mod error_code {
    /// No worker is attached, so a controller frame had nobody to run
    /// it. It was not stored.
    pub const NO_WORKER: &str = "no_worker";
    /// Another connection already answered this `request_id`; the
    /// frame's `answered_by` says who. The answer was not stored or
    /// forwarded. RC's counterpart of an HTTP 409.
    pub const ALREADY_ANSWERED: &str = "already_answered";
}

/// Who answered a prompt first: the `answered_by` of an
/// [`error_code::ALREADY_ANSWERED`] refusal.
///
/// Identifies a control surface without handing out anything that
/// authenticates it: the device id is what `GET /v1/devices` lists, the
/// label is the one the device was issued with, and the connection id is
/// an opaque per-socket tag that tells two surfaces on one device apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnsweredBy {
    /// The device whose access token the answering connection used.
    pub device_id: String,
    /// That device's label, when RC knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Opaque tag of the answering connection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// The `event_id` the winning answer was stored under, for a
    /// controller that wants to find it in the history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<u64>,
}

impl AnsweredBy {
    /// The name to show a person: the label, else the device id.
    pub fn display_name(&self) -> &str {
        self.label
            .as_deref()
            .filter(|label| !label.is_empty())
            .unwrap_or(&self.device_id)
    }
}

/// What a worker says its session is doing: the `state` of a
/// `session_state` frame, and what the session list shows as the
/// session's reported state.
///
/// A closed set with a forward-compatible fallback. The wire form is the
/// snake_case word; a word this build does not know is kept verbatim in
/// [`SessionRunState::Other`] rather than refused, so a newer worker's
/// state still reaches an older controller as text. An empty word is
/// malformed.
///
/// | Word | Meaning |
/// |---|---|
/// | `starting` | the runner took the work and is opening or resuming the session; no turn yet |
/// | `running` | a turn is in progress |
/// | `idle` | the session is waiting for its next prompt |
/// | `needs_input` | a turn is blocked on a controller — a permission request or a question is pending |
/// | `stopped` | the session ended normally (stopped, archived, finished); terminal |
/// | `failed` | the session ended with an error, which `detail` describes; terminal |
///
/// This is the worker's view. RC's own lifecycle (`queued`, `running`,
/// `archived` on a session row) is a separate, server-owned field.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionRunState {
    /// `starting`.
    Starting,
    /// `running`.
    Running,
    /// `idle`.
    Idle,
    /// `needs_input`.
    NeedsInput,
    /// `stopped`.
    Stopped,
    /// `failed`.
    Failed,
    /// A word this build does not know, kept as received.
    Other(String),
}

impl SessionRunState {
    /// The words this build knows, in lifecycle order.
    pub const KNOWN: [Self; 6] = [
        Self::Starting,
        Self::Running,
        Self::Idle,
        Self::NeedsInput,
        Self::Stopped,
        Self::Failed,
    ];

    /// The wire word.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::NeedsInput => "needs_input",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Other(word) => word,
        }
    }

    /// Read a wire word. Never fails: an unknown word is
    /// [`SessionRunState::Other`].
    pub fn parse(word: &str) -> Self {
        Self::KNOWN
            .into_iter()
            .find(|state| state.as_str() == word)
            .unwrap_or_else(|| Self::Other(word.to_string()))
    }

    /// Whether the session has ended, as far as the worker is concerned.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Stopped | Self::Failed)
    }

    /// Whether this is a word this build knows.
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Other(_))
    }
}

impl From<&str> for SessionRunState {
    fn from(word: &str) -> Self {
        Self::parse(word)
    }
}

impl fmt::Display for SessionRunState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PartialEq<str> for SessionRunState {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SessionRunState {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl Serialize for SessionRunState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SessionRunState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        if word.is_empty() {
            return Err(de::Error::custom("a session state is never empty"));
        }
        Ok(Self::parse(&word))
    }
}

/// Which side of the stream a frame may legitimately come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOrigin {
    /// The bridge worker running the session.
    Worker,
    /// A control surface attached to the session.
    Controller,
    /// The ingress itself — never a peer.
    Server,
}

/// A frame whose `type` this build does not know.
///
/// The whole object is kept, `type` included, so re-serializing returns
/// exactly what arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownFrame {
    /// The `type` string as it arrived.
    pub frame_type: String,
    /// The verbatim object, including its `type` entry.
    pub payload: Map<String, Value>,
}

/// Inner `response` body of a `control_response` frame.
///
/// One shape covers both users of the envelope: the
/// [`ServerControlResponsePlan`] a worker computes for a server-initiated
/// control request, and the permission decision
/// [`crate::config::PermissionResponseEvent`] already posts over HTTP.
/// They were always the same wire object — `{subtype, request_id}` plus
/// either a success `response` or an `error` — and keeping them one type
/// is what lets the HTTP route's stored payload replay as a stream frame
/// without a translation step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlResponseBody {
    /// `"success"` or `"error"`.
    pub subtype: String,
    /// Echoes the `request_id` of the request being answered.
    pub request_id: String,
    /// Success payload, when the subtype carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    /// Error text, when the subtype is `"error"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponseBody {
    /// A success response, with or without a body.
    pub fn success(request_id: impl Into<String>, response: Option<Value>) -> Self {
        Self {
            subtype: "success".to_string(),
            request_id: request_id.into(),
            response,
            error: None,
        }
    }

    /// When the change this success answers takes effect: the
    /// [`ControlApplied`] body of a response to `set_model`,
    /// `set_max_thinking_tokens`, `set_permission_mode` or `interrupt`.
    /// `None` for an error, for `initialize`, and for a permission
    /// decision.
    pub fn applied(&self) -> Option<ControlEffect> {
        if self.subtype != "success" {
            return None;
        }
        let body = self.response.clone()?;
        serde_json::from_value::<ControlApplied>(body)
            .ok()
            .map(|applied| applied.applies)
    }

    /// An error response.
    pub fn error(request_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            subtype: "error".to_string(),
            request_id: request_id.into(),
            response: None,
            error: Some(error.into()),
        }
    }
}

impl From<ServerControlResponsePlan> for ControlResponseBody {
    fn from(plan: ServerControlResponsePlan) -> Self {
        match plan {
            ServerControlResponsePlan::SuccessApplied { request_id, effect } => Self::success(
                request_id,
                Some(
                    serde_json::to_value(ControlApplied { applies: effect })
                        .expect("an applied body is serializable"),
                ),
            ),
            ServerControlResponsePlan::SuccessInitialize { request_id, body } => Self::success(
                request_id,
                Some(serde_json::to_value(body).expect("initialize body is serializable")),
            ),
            ServerControlResponsePlan::Error { request_id, error } => {
                Self::error(request_id, error)
            }
        }
    }
}

impl From<PermissionResponseBody> for ControlResponseBody {
    fn from(body: PermissionResponseBody) -> Self {
        Self {
            subtype: body.subtype,
            request_id: body.request_id,
            response: Some(body.response),
            error: None,
        }
    }
}

/// A controller's answer to one question of a `question_response`.
///
/// `selected_options` are indices into that question's options. With no
/// option selected, `other_text` is the answer ("Other"); with options
/// selected, it is a note alongside them. Whether the answer fits the
/// question (in range, one option for a single-select question, not
/// empty) is the runner's to judge against the prompt it holds, and a
/// misfit is refused there rather than failing the frame.
///
/// The wire names are snake_case like the rest of the envelope; the
/// camelCase spellings the question payload itself uses
/// (`selectedOptions`, `otherText`) are read too.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    /// Chosen option indices; empty when the answer is free text.
    #[serde(default, alias = "selectedOptions")]
    pub selected_options: Vec<usize>,
    /// Free-form text: the answer itself, or a note on the chosen options.
    #[serde(default, alias = "otherText", skip_serializing_if = "Option::is_none")]
    pub other_text: Option<String>,
}

impl QuestionAnswer {
    /// An answer that picks these options.
    pub fn options(selected_options: impl Into<Vec<usize>>) -> Self {
        Self {
            selected_options: selected_options.into(),
            other_text: None,
        }
    }

    /// A free-text answer.
    pub fn text(other_text: impl Into<String>) -> Self {
        Self {
            selected_options: Vec::new(),
            other_text: Some(other_text.into()),
        }
    }
}

/// One frame on the session stream.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionFrame {
    /// worker → controllers: a projection of one session message. The
    /// shape is the runner's to choose, so it stays opaque here.
    SessionMessage {
        /// Worker-chosen id that makes a resend idempotent; see the
        /// module docs. Absent means "store every copy".
        message_id: Option<String>,
        /// Opaque message projection.
        message: Value,
    },
    /// worker → controllers: a coarse lifecycle state, plus optional
    /// detail (an error string, a model name, whatever the runner has).
    SessionState {
        /// What the session is doing.
        state: SessionRunState,
        /// Human-readable detail, when there is any.
        detail: Option<String>,
    },
    /// worker → controllers: a permission prompt awaiting a decision.
    PermissionRequest {
        /// Correlates with the `request_id` of the answering
        /// `permission_response`.
        request_id: String,
        /// Opaque request payload.
        request: Value,
    },
    /// worker → controllers: the answer to a `control_request`.
    ControlResponse {
        /// The response body.
        response: ControlResponseBody,
    },
    /// worker → RC: this RC session runs as the machine-local Rebon
    /// session `rebon_session_id`. See the module docs.
    SessionBound {
        /// The Rebon session id; satisfies
        /// [`crate::config::valid_rebon_session_id`].
        rebon_session_id: String,
    },
    /// controller → worker: run this prompt.
    Prompt {
        /// Prompt text.
        text: String,
        /// Opaque attachments; empty when there are none.
        attachments: Vec<Value>,
    },
    /// controller → worker: interrupt whatever is running.
    Cancel,
    /// controller → worker: a permission decision.
    PermissionResponse {
        /// The decision, in the shape the HTTP events route already uses.
        response: PermissionResponseBody,
    },
    /// controller → worker: the answers to a question prompt. See the
    /// module docs.
    QuestionResponse {
        /// The `request_id` of the `permission_request` that asked.
        request_id: String,
        /// One answer per question, in the order they were asked.
        answers: Vec<QuestionAnswer>,
    },
    /// controller → worker: one of the server-facing control requests.
    ControlRequest {
        /// Echoed back in the `control_response`.
        request_id: String,
        /// Wire subtype; parse it with [`SessionFrame::control_subtype`].
        subtype: String,
        /// Opaque parameters; `null` when the subtype takes none.
        params: Value,
    },
    /// server → either side: this frame could not be routed. The socket
    /// stays open; the sender decides what to do about it.
    StreamError {
        /// Stable machine-readable code, one of [`error_code`].
        code: String,
        /// Human-readable explanation.
        message: String,
        /// The prompt a refused answer was for; set with
        /// [`error_code::ALREADY_ANSWERED`].
        request_id: Option<String>,
        /// Who answered that prompt first; set with
        /// [`error_code::ALREADY_ANSWERED`].
        answered_by: Option<AnsweredBy>,
    },
    /// A `type` this build does not know, kept verbatim.
    Other(UnknownFrame),
}

impl SessionFrame {
    /// The wire `type` string.
    pub fn frame_type(&self) -> &str {
        match self {
            Self::SessionMessage { .. } => "session_message",
            Self::SessionState { .. } => "session_state",
            Self::PermissionRequest { .. } => "permission_request",
            Self::ControlResponse { .. } => "control_response",
            Self::SessionBound { .. } => "session_bound",
            Self::Prompt { .. } => "prompt",
            Self::Cancel => "cancel",
            Self::PermissionResponse { .. } => "permission_response",
            Self::QuestionResponse { .. } => "question_response",
            Self::ControlRequest { .. } => "control_request",
            Self::StreamError { .. } => "stream_error",
            Self::Other(unknown) => &unknown.frame_type,
        }
    }

    /// The side this frame may come from, or `None` for a `type` this
    /// build does not know — the ingress cannot judge the origin of a
    /// frame it cannot interpret, so it routes such a frame by the
    /// sender's role instead.
    pub fn origin(&self) -> Option<FrameOrigin> {
        match self {
            Self::SessionMessage { .. }
            | Self::SessionState { .. }
            | Self::PermissionRequest { .. }
            | Self::ControlResponse { .. }
            | Self::SessionBound { .. } => Some(FrameOrigin::Worker),
            Self::Prompt { .. }
            | Self::Cancel
            | Self::PermissionResponse { .. }
            | Self::QuestionResponse { .. }
            | Self::ControlRequest { .. } => Some(FrameOrigin::Controller),
            Self::StreamError { .. } => Some(FrameOrigin::Server),
            Self::Other(_) => None,
        }
    }

    /// Parsed subtype of a `control_request` frame.
    ///
    /// The wire carries a plain string so an unknown subtype survives the
    /// trip; [`ServerControlRequestSubtype::parse`] is what turns it into
    /// the five well-known variants plus `Other`.
    pub fn control_subtype(&self) -> Option<ServerControlRequestSubtype> {
        match self {
            Self::ControlRequest { subtype, .. } => {
                Some(ServerControlRequestSubtype::parse(subtype))
            }
            _ => None,
        }
    }

    /// A `session_message` frame without an id.
    pub fn message(message: Value) -> Self {
        Self::SessionMessage {
            message_id: None,
            message,
        }
    }

    /// A `session_message` frame with a worker-chosen id.
    pub fn message_with_id(message_id: impl Into<String>, message: Value) -> Self {
        Self::SessionMessage {
            message_id: Some(message_id.into()),
            message,
        }
    }

    /// The key RC deduplicates this frame's storage on, within one
    /// session: `session_message:<message_id>` for a message that has an
    /// id, `permission_request:<request_id>` for a permission request,
    /// `session_bound:<rebon_session_id>` for a binding, and `None` —
    /// store every copy — for everything else.
    ///
    /// Prefixed by type so a message id can never collide with a request
    /// id.
    pub fn idempotency_key(&self) -> Option<String> {
        match self {
            Self::SessionMessage {
                message_id: Some(message_id),
                ..
            } => Some(format!("session_message:{message_id}")),
            Self::PermissionRequest { request_id, .. } => {
                Some(format!("permission_request:{request_id}"))
            }
            Self::SessionBound { rebon_session_id } => {
                Some(format!("session_bound:{rebon_session_id}"))
            }
            _ => None,
        }
    }

    /// The worker-chosen id [`Self::idempotency_key`] is built from, when
    /// the frame has one — what RC checks against [`MAX_FRAME_ID_BYTES`].
    pub fn idempotency_id(&self) -> Option<&str> {
        match self {
            Self::SessionMessage {
                message_id: Some(message_id),
                ..
            } => Some(message_id),
            Self::PermissionRequest { request_id, .. } => Some(request_id),
            Self::SessionBound { rebon_session_id } => Some(rebon_session_id),
            _ => None,
        }
    }

    /// A `session_bound` frame.
    pub fn bound(rebon_session_id: impl Into<String>) -> Self {
        Self::SessionBound {
            rebon_session_id: rebon_session_id.into(),
        }
    }

    /// A `prompt` frame with no attachments.
    pub fn prompt(text: impl Into<String>) -> Self {
        Self::Prompt {
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    /// A `stream_error` frame.
    pub fn stream_error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::StreamError {
            code: code.into(),
            message: message.into(),
            request_id: None,
            answered_by: None,
        }
    }

    /// The refusal of an answer to `request_id`, which `answered_by`
    /// answered first. See the module docs.
    pub fn already_answered(request_id: impl Into<String>, answered_by: AnsweredBy) -> Self {
        let request_id = request_id.into();
        Self::StreamError {
            code: error_code::ALREADY_ANSWERED.to_string(),
            message: format!(
                "{request_id} was already answered by {}",
                answered_by.display_name()
            ),
            request_id: Some(request_id),
            answered_by: Some(answered_by),
        }
    }

    /// The prompt this frame answers: the `request_id` of a
    /// `permission_response` or a `question_response`, and `None` for
    /// every other frame. What RC's first-answer-wins rule is keyed on.
    pub fn answered_request_id(&self) -> Option<&str> {
        match self {
            Self::PermissionResponse { response } => Some(&response.request_id),
            Self::QuestionResponse { request_id, .. } => Some(request_id),
            _ => None,
        }
    }
}

impl Serialize for SessionFrame {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // `Other` re-serializes the object it arrived as, `type` and all,
        // so a frame this build does not understand survives a relay
        // through it unchanged.
        if let Self::Other(unknown) = self {
            return unknown.payload.serialize(serializer);
        }
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("type", self.frame_type())?;
        match self {
            Self::SessionMessage {
                message_id,
                message,
            } => {
                if let Some(message_id) = message_id {
                    map.serialize_entry("message_id", message_id)?;
                }
                map.serialize_entry("message", message)?;
            }
            Self::SessionState { state, detail } => {
                map.serialize_entry("state", state)?;
                if let Some(detail) = detail {
                    map.serialize_entry("detail", detail)?;
                }
            }
            Self::PermissionRequest {
                request_id,
                request,
            } => {
                map.serialize_entry("request_id", request_id)?;
                map.serialize_entry("request", request)?;
            }
            Self::ControlResponse { response } => map.serialize_entry("response", response)?,
            Self::SessionBound { rebon_session_id } => {
                map.serialize_entry("rebon_session_id", rebon_session_id)?
            }
            Self::Prompt { text, attachments } => {
                map.serialize_entry("text", text)?;
                if !attachments.is_empty() {
                    map.serialize_entry("attachments", attachments)?;
                }
            }
            Self::Cancel => {}
            Self::PermissionResponse { response } => map.serialize_entry("response", response)?,
            Self::QuestionResponse {
                request_id,
                answers,
            } => {
                map.serialize_entry("request_id", request_id)?;
                map.serialize_entry("answers", answers)?;
            }
            Self::ControlRequest {
                request_id,
                subtype,
                params,
            } => {
                map.serialize_entry("request_id", request_id)?;
                map.serialize_entry("subtype", subtype)?;
                if !params.is_null() {
                    map.serialize_entry("params", params)?;
                }
            }
            Self::StreamError {
                code,
                message,
                request_id,
                answered_by,
            } => {
                map.serialize_entry("code", code)?;
                map.serialize_entry("message", message)?;
                if let Some(request_id) = request_id {
                    map.serialize_entry("request_id", request_id)?;
                }
                if let Some(answered_by) = answered_by {
                    map.serialize_entry("answered_by", answered_by)?;
                }
            }
            Self::Other(_) => unreachable!("handled above"),
        }
        map.end()
    }
}

/// Take a required string field.
fn take_string<E: de::Error>(
    map: &mut Map<String, Value>,
    field: &str,
    kind: &str,
) -> Result<String, E> {
    match map.remove(field) {
        Some(Value::String(value)) => Ok(value),
        Some(_) => Err(E::custom(format!(
            "`{field}` of a {kind} frame is not a string"
        ))),
        None => Err(E::custom(format!("{kind} frame has no `{field}`"))),
    }
}

/// Take an optional string field. Present-but-null counts as absent.
fn take_optional_string<E: de::Error>(
    map: &mut Map<String, Value>,
    field: &str,
    kind: &str,
) -> Result<Option<String>, E> {
    match map.remove(field) {
        Some(Value::String(value)) => Ok(Some(value)),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(E::custom(format!(
            "`{field}` of a {kind} frame is not a string"
        ))),
    }
}

/// Take an opaque field. Absent is `null`, which is what the constructors
/// produce for "no payload".
fn take_value(map: &mut Map<String, Value>, field: &str) -> Value {
    map.remove(field).unwrap_or(Value::Null)
}

/// Take a field and decode it into `T`.
fn take_typed<T: for<'a> Deserialize<'a>, E: de::Error>(
    map: &mut Map<String, Value>,
    field: &str,
    kind: &str,
) -> Result<T, E> {
    let value = map
        .remove(field)
        .ok_or_else(|| E::custom(format!("{kind} frame has no `{field}`")))?;
    serde_json::from_value(value)
        .map_err(|error| E::custom(format!("`{field}` of a {kind} frame: {error}")))
}

/// Take an optional field and decode it into `T`. Present-but-null
/// counts as absent.
fn take_optional_typed<T: for<'a> Deserialize<'a>, E: de::Error>(
    map: &mut Map<String, Value>,
    field: &str,
    kind: &str,
) -> Result<Option<T>, E> {
    match map.remove(field) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => serde_json::from_value(value)
            .map(Some)
            .map_err(|error| E::custom(format!("`{field}` of a {kind} frame: {error}"))),
    }
}

impl SessionFrame {
    /// Build a frame from an already-parsed object.
    ///
    /// Split out of the visitor so both the `Deserialize` impl and
    /// callers holding a `serde_json::Map` use one decision table.
    fn from_object<E: de::Error>(mut map: Map<String, Value>) -> Result<Self, E> {
        let kind = match map.get("type") {
            Some(Value::String(kind)) => kind.clone(),
            Some(_) => return Err(E::custom("session frame `type` is not a string")),
            None => return Err(E::custom("session frame has no `type`")),
        };
        let frame = match kind.as_str() {
            "session_message" => Self::SessionMessage {
                message_id: take_optional_string(&mut map, "message_id", &kind)?,
                message: take_value(&mut map, "message"),
            },
            "session_state" => Self::SessionState {
                state: take_typed(&mut map, "state", &kind)?,
                detail: take_optional_string(&mut map, "detail", &kind)?,
            },
            "permission_request" => Self::PermissionRequest {
                request_id: take_string(&mut map, "request_id", &kind)?,
                request: take_value(&mut map, "request"),
            },
            "control_response" => Self::ControlResponse {
                response: take_typed(&mut map, "response", &kind)?,
            },
            "session_bound" => {
                let rebon_session_id = take_string(&mut map, "rebon_session_id", &kind)?;
                // The id names something on the machine's disk; a frame
                // carrying anything else is malformed, not a version skew.
                if !crate::config::valid_rebon_session_id(&rebon_session_id) {
                    return Err(E::custom(
                        "`rebon_session_id` of a session_bound frame is not a plain session id",
                    ));
                }
                Self::SessionBound { rebon_session_id }
            }
            "prompt" => Self::Prompt {
                text: take_string(&mut map, "text", &kind)?,
                attachments: match map.remove("attachments") {
                    Some(Value::Null) | None => Vec::new(),
                    Some(value) => serde_json::from_value(value).map_err(|error| {
                        E::custom(format!("`attachments` of a prompt frame: {error}"))
                    })?,
                },
            },
            "cancel" => Self::Cancel,
            "permission_response" => Self::PermissionResponse {
                response: take_typed(&mut map, "response", &kind)?,
            },
            // `answers` is required: a question response without it has
            // nothing to say. An empty list is well-formed and refused by
            // the runner, which knows how many questions were asked.
            "question_response" => Self::QuestionResponse {
                request_id: take_string(&mut map, "request_id", &kind)?,
                answers: take_typed(&mut map, "answers", &kind)?,
            },
            "control_request" => Self::ControlRequest {
                request_id: take_string(&mut map, "request_id", &kind)?,
                subtype: take_string(&mut map, "subtype", &kind)?,
                params: take_value(&mut map, "params"),
            },
            "stream_error" => Self::StreamError {
                code: take_string(&mut map, "code", &kind)?,
                message: take_string(&mut map, "message", &kind)?,
                request_id: take_optional_string(&mut map, "request_id", &kind)?,
                answered_by: take_optional_typed(&mut map, "answered_by", &kind)?,
            },
            // Not a failure: see the module docs.
            _ => Self::Other(UnknownFrame {
                frame_type: kind,
                payload: map,
            }),
        };
        Ok(frame)
    }
}

impl<'de> Deserialize<'de> for SessionFrame {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FrameVisitor;

        impl<'de> Visitor<'de> for FrameVisitor {
            type Value = SessionFrame;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a session frame object with a `type` field")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut map = Map::new();
                while let Some((key, value)) = access.next_entry::<String, Value>()? {
                    map.insert(key, value);
                }
                SessionFrame::from_object(map)
            }
        }

        deserializer.deserialize_map(FrameVisitor)
    }
}

/// A frame as RC delivers it: the frame, plus the `event_id` it was
/// persisted under when it was.
///
/// On the wire this is the frame object with one more top-level key,
/// [`EVENT_ID_KEY`], so a reader that only wants the frame can still
/// parse the text as a [`SessionFrame`] (known types ignore the key; an
/// [`SessionFrame::Other`] would keep it in its payload, which is why
/// readers should go through this type).
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveredFrame {
    /// The event id RC stored the frame under; `None` for a frame that
    /// was never persisted, such as a `stream_error`.
    pub event_id: Option<u64>,
    /// The frame itself.
    pub frame: SessionFrame,
}

impl DeliveredFrame {
    /// A frame with no event id — what a peer sends.
    pub fn unstamped(frame: SessionFrame) -> Self {
        Self {
            event_id: None,
            frame,
        }
    }
}

impl Serialize for DeliveredFrame {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(&self.frame).map_err(serde::ser::Error::custom)?;
        if let (Some(event_id), Value::Object(map)) = (self.event_id, &mut value) {
            map.insert(EVENT_ID_KEY.to_string(), Value::from(event_id));
        }
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DeliveredFrame {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut map = Map::<String, Value>::deserialize(deserializer)?;
        // Present means a non-negative integer. `null` is not "absent":
        // a key that is there is a key someone set, and the server has to
        // be able to refuse it from a peer.
        let event_id =
            match map.remove(EVENT_ID_KEY) {
                None => None,
                Some(value) => Some(value.as_u64().ok_or_else(|| {
                    de::Error::custom("`event_id` is not a non-negative integer")
                })?),
            };
        Ok(Self {
            event_id,
            frame: SessionFrame::from_object(map)?,
        })
    }
}

/// Add `event_id` to a persisted frame's JSON text without re-encoding
/// the rest of it.
///
/// `frame_json` is a stored payload: a JSON object with a `type`, which
/// RC only ever stores after parsing it as a [`SessionFrame`] with no
/// `event_id` of its own. The key is spliced in front of the object's
/// first member, so every other byte is delivered exactly as it was
/// received. `None` for anything that is not a non-empty object, which
/// a stored frame never is.
pub fn stamp_event_id(frame_json: &str, event_id: u64) -> Option<String> {
    let body = frame_json.trim_start().strip_prefix('{')?;
    if body.trim_start().starts_with('}') || !frame_json.trim_end().ends_with('}') {
        return None;
    }
    let mut stamped = String::with_capacity(frame_json.len() + 32);
    stamped.push_str("{\"");
    stamped.push_str(EVENT_ID_KEY);
    stamped.push_str("\":");
    stamped.push_str(&event_id.to_string());
    stamped.push(',');
    stamped.push_str(body);
    Some(stamped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_request::InitializeResponseBody;
    use serde_json::json;

    fn round_trip(frame: &SessionFrame) -> SessionFrame {
        let text = serde_json::to_string(frame).expect("serializes");
        serde_json::from_str(&text).expect("deserializes")
    }

    fn every_variant() -> Vec<SessionFrame> {
        vec![
            SessionFrame::message(json!({"role": "assistant", "text": "hi"})),
            SessionFrame::message_with_id("msg-1", json!({"text": "once"})),
            SessionFrame::SessionState {
                state: "running".into(),
                detail: Some("model warmed".into()),
            },
            SessionFrame::SessionState {
                state: "idle".into(),
                detail: None,
            },
            SessionFrame::PermissionRequest {
                request_id: "req-1".into(),
                request: json!({"tool": "Bash"}),
            },
            SessionFrame::ControlResponse {
                response: ControlResponseBody::success("req-2", Some(json!({"pid": 7}))),
            },
            SessionFrame::ControlResponse {
                response: ControlResponseBody::error("req-3", "nope"),
            },
            SessionFrame::bound("0192f3c4-5e6f"),
            SessionFrame::Prompt {
                text: "explain leases".into(),
                attachments: vec![json!({"path": "a.rs"})],
            },
            SessionFrame::prompt("no attachments"),
            SessionFrame::Cancel,
            SessionFrame::PermissionResponse {
                response: PermissionResponseBody::success("req-1", json!({"behavior": "allow"})),
            },
            SessionFrame::QuestionResponse {
                request_id: "req-1".into(),
                answers: vec![
                    QuestionAnswer::options([0, 2]),
                    QuestionAnswer::text("Fridays"),
                ],
            },
            SessionFrame::ControlRequest {
                request_id: "req-4".into(),
                subtype: "set_model".into(),
                params: json!({"model": "opus"}),
            },
            SessionFrame::ControlRequest {
                request_id: "req-5".into(),
                subtype: "interrupt".into(),
                params: Value::Null,
            },
            SessionFrame::stream_error("no_worker", "no worker is attached"),
        ]
    }

    #[test]
    fn every_variant_round_trips() {
        for frame in every_variant() {
            assert_eq!(round_trip(&frame), frame, "{}", frame.frame_type());
        }
    }

    #[test]
    fn the_type_tag_is_the_wire_name() {
        let variants = every_variant();
        let types: Vec<&str> = variants.iter().map(SessionFrame::frame_type).collect();
        assert_eq!(
            types,
            vec![
                "session_message",
                "session_message",
                "session_state",
                "session_state",
                "permission_request",
                "control_response",
                "control_response",
                "session_bound",
                "prompt",
                "prompt",
                "cancel",
                "permission_response",
                "question_response",
                "control_request",
                "control_request",
                "stream_error",
            ]
        );
        for frame in every_variant() {
            let value: Value = serde_json::to_value(&frame).expect("serializes");
            assert_eq!(value["type"], frame.frame_type(), "{value}");
        }
    }

    #[test]
    fn origin_partitions_the_variants_by_direction() {
        let variants = every_variant();
        let mut by_origin: Vec<(&str, Option<FrameOrigin>)> = variants
            .iter()
            .map(|frame| (frame.frame_type(), frame.origin()))
            .collect();
        by_origin.dedup();
        assert_eq!(
            by_origin,
            vec![
                ("session_message", Some(FrameOrigin::Worker)),
                ("session_state", Some(FrameOrigin::Worker)),
                ("permission_request", Some(FrameOrigin::Worker)),
                ("control_response", Some(FrameOrigin::Worker)),
                ("session_bound", Some(FrameOrigin::Worker)),
                ("prompt", Some(FrameOrigin::Controller)),
                ("cancel", Some(FrameOrigin::Controller)),
                ("permission_response", Some(FrameOrigin::Controller)),
                ("question_response", Some(FrameOrigin::Controller)),
                ("control_request", Some(FrameOrigin::Controller)),
                ("stream_error", Some(FrameOrigin::Server)),
            ]
        );
    }

    #[test]
    fn an_unknown_type_falls_back_instead_of_failing() {
        let raw = r#"{"type":"telemetry","sequence":9,"body":{"cpu":0.5}}"#;
        let frame: SessionFrame = serde_json::from_str(raw).expect("unknown type is tolerated");
        match &frame {
            SessionFrame::Other(unknown) => {
                assert_eq!(unknown.frame_type, "telemetry");
                assert_eq!(unknown.payload["sequence"], json!(9));
            }
            other => panic!("expected Other, got {other:?}"),
        }
        assert_eq!(frame.frame_type(), "telemetry");
        assert_eq!(frame.origin(), None);
        // Re-serializing returns what arrived, so a relay through this
        // build does not strip fields it did not understand.
        assert_eq!(
            serde_json::to_value(&frame).expect("serializes"),
            serde_json::from_str::<Value>(raw).expect("raw json")
        );
        assert_eq!(round_trip(&frame), frame);
    }

    #[test]
    fn a_known_type_that_is_malformed_still_fails() {
        // Version skew is tolerated; a broken frame of a type we do know
        // is a bug on the wire and must not be silently swallowed.
        for raw in [
            r#"{"type":"prompt"}"#,
            r#"{"type":"prompt","text":42}"#,
            r#"{"type":"session_state"}"#,
            r#"{"type":"control_request","request_id":"r"}"#,
            r#"{"type":"control_response","response":{"request_id":"r"}}"#,
            r#"{"type":"permission_response","response":{"subtype":"success"}}"#,
            r#"{"type":"stream_error","code":"x"}"#,
            r#"{"type":"session_bound"}"#,
            r#"{"type":"session_bound","rebon_session_id":7}"#,
            r#"{"type":"session_bound","rebon_session_id":""}"#,
            r#"{"type":"session_bound","rebon_session_id":"../etc"}"#,
            r#"{"type":"session_bound","rebon_session_id":".hidden"}"#,
            r#"{"type":"question_response","request_id":"r"}"#,
            r#"{"type":"question_response","answers":[]}"#,
            r#"{"type":"question_response","request_id":"r","answers":{}}"#,
            r#"{"type":"question_response","request_id":"r","answers":[{"selected_options":[-1]}]}"#,
            r#"{"type":"question_response","request_id":"r","answers":[{"other_text":3}]}"#,
        ] {
            assert!(
                serde_json::from_str::<SessionFrame>(raw).is_err(),
                "{raw} should not parse"
            );
        }
    }

    #[test]
    fn a_frame_without_a_type_is_rejected() {
        for raw in [r#"{}"#, r#"{"type":7}"#, r#"[]"#, r#""text""#] {
            assert!(
                serde_json::from_str::<SessionFrame>(raw).is_err(),
                "{raw} should not parse"
            );
        }
    }

    #[test]
    fn optional_fields_are_omitted_rather_than_sent_as_null() {
        let text = serde_json::to_string(&SessionFrame::SessionState {
            state: "idle".into(),
            detail: None,
        })
        .expect("serializes");
        assert_eq!(text, r#"{"type":"session_state","state":"idle"}"#);

        let text = serde_json::to_string(&SessionFrame::prompt("hi")).expect("serializes");
        assert_eq!(text, r#"{"type":"prompt","text":"hi"}"#);

        let text = serde_json::to_string(&SessionFrame::Cancel).expect("serializes");
        assert_eq!(text, r#"{"type":"cancel"}"#);

        let text = serde_json::to_string(&SessionFrame::ControlRequest {
            request_id: "r".into(),
            subtype: "interrupt".into(),
            params: Value::Null,
        })
        .expect("serializes");
        assert_eq!(
            text,
            r#"{"type":"control_request","request_id":"r","subtype":"interrupt"}"#
        );
    }

    #[test]
    fn a_missing_optional_field_deserializes_to_its_empty_value() {
        let frame: SessionFrame =
            serde_json::from_str(r#"{"type":"prompt","text":"hi","attachments":null}"#)
                .expect("parses");
        assert_eq!(frame, SessionFrame::prompt("hi"));

        let frame: SessionFrame =
            serde_json::from_str(r#"{"type":"session_state","state":"idle","detail":null}"#)
                .expect("parses");
        assert_eq!(
            frame,
            SessionFrame::SessionState {
                state: "idle".into(),
                detail: None
            }
        );

        let frame: SessionFrame =
            serde_json::from_str(r#"{"type":"session_message"}"#).expect("parses");
        assert_eq!(frame, SessionFrame::message(Value::Null));
    }

    #[test]
    fn control_subtype_parses_only_on_a_control_request() {
        let frame = SessionFrame::ControlRequest {
            request_id: "r".into(),
            subtype: "set_permission_mode".into(),
            params: json!({"mode": "auto"}),
        };
        assert_eq!(
            frame.control_subtype(),
            Some(ServerControlRequestSubtype::SetPermissionMode)
        );
        let unknown = SessionFrame::ControlRequest {
            request_id: "r".into(),
            subtype: "eject_warp_core".into(),
            params: Value::Null,
        };
        assert_eq!(
            unknown.control_subtype(),
            Some(ServerControlRequestSubtype::Other("eject_warp_core".into()))
        );
        assert_eq!(SessionFrame::Cancel.control_subtype(), None);
    }

    #[test]
    fn a_response_plan_becomes_a_control_response_body() {
        let applied = ControlResponseBody::from(ServerControlResponsePlan::SuccessApplied {
            request_id: "r1".into(),
            effect: ControlEffect::NextTurn,
        });
        assert_eq!(
            applied,
            ControlResponseBody::success("r1", Some(json!({"applies": "next_turn"})))
        );
        assert_eq!(applied.applied(), Some(ControlEffect::NextTurn));
        assert_eq!(
            ControlResponseBody::from(ServerControlResponsePlan::SuccessApplied {
                request_id: "r1".into(),
                effect: ControlEffect::Now,
            })
            .applied(),
            Some(ControlEffect::Now)
        );
        assert_eq!(
            ControlResponseBody::from(ServerControlResponsePlan::Error {
                request_id: "r2".into(),
                error: "denied".into()
            }),
            ControlResponseBody::error("r2", "denied")
        );

        let initialize = ControlResponseBody::from(ServerControlResponsePlan::SuccessInitialize {
            request_id: "r3".into(),
            body: InitializeResponseBody {
                commands: Vec::new(),
                output_style: "normal",
                available_output_styles: vec!["normal"],
                models: Vec::new(),
                pid: 1234,
            },
        });
        assert_eq!(initialize.subtype, "success");
        assert_eq!(initialize.applied(), None);
        assert_eq!(ControlResponseBody::error("r4", "no").applied(), None);
        assert_eq!(
            ControlResponseBody::success("r5", Some(json!({"behavior": "allow"}))).applied(),
            None
        );
        // An error that happens to carry the key is still not a success.
        let mut lying = ControlResponseBody::error("r6", "no");
        lying.response = Some(json!({"applies": "now"}));
        assert_eq!(lying.applied(), None);
        let body = initialize.response.as_ref().expect("initialize body");
        assert_eq!(body["pid"], json!(1234));
        assert_eq!(body["output_style"], json!("normal"));
        assert_eq!(body["available_output_styles"], json!(["normal"]));
    }

    #[test]
    fn a_permission_decision_is_the_same_wire_object_on_both_surfaces() {
        // `PermissionResponseEvent` (the HTTP events route) and a
        // `control_response` frame have to serialize identically, or a
        // decision posted over HTTP would replay as a different frame
        // than one sent over the socket.
        let body = PermissionResponseBody::success("req-1", json!({"behavior": "allow"}));
        let over_http =
            serde_json::to_value(crate::config::PermissionResponseEvent::new(body.clone()))
                .expect("serializes");
        let over_socket = serde_json::to_value(SessionFrame::ControlResponse {
            response: ControlResponseBody::from(body),
        })
        .expect("serializes");
        assert_eq!(over_http, over_socket);
    }

    #[test]
    fn a_message_id_is_optional_on_the_wire() {
        // A frame from before message ids.
        let old: SessionFrame =
            serde_json::from_str(r#"{"type":"session_message","message":{"n":1}}"#).unwrap();
        assert_eq!(old, SessionFrame::message(json!({"n": 1})));
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            r#"{"type":"session_message","message":{"n":1}}"#
        );

        let keyed = SessionFrame::message_with_id("m-1", json!({"n": 1}));
        let text = serde_json::to_string(&keyed).unwrap();
        assert_eq!(
            text,
            r#"{"type":"session_message","message_id":"m-1","message":{"n":1}}"#
        );
        assert_eq!(serde_json::from_str::<SessionFrame>(&text).unwrap(), keyed);
        // `null` is absent; a non-string is malformed.
        assert_eq!(
            serde_json::from_str::<SessionFrame>(
                r#"{"type":"session_message","message_id":null,"message":1}"#
            )
            .unwrap(),
            SessionFrame::message(json!(1))
        );
        assert!(serde_json::from_str::<SessionFrame>(
            r#"{"type":"session_message","message_id":7,"message":1}"#
        )
        .is_err());
    }

    #[test]
    fn only_identified_content_frames_have_an_idempotency_key() {
        let keys: Vec<(String, Option<String>)> = every_variant()
            .iter()
            .map(|frame| (frame.frame_type().to_string(), frame.idempotency_key()))
            .collect();
        for (frame_type, key) in &keys {
            match frame_type.as_str() {
                "session_message" | "permission_request" | "session_bound" => {}
                _ => assert_eq!(key, &None, "{frame_type}"),
            }
        }
        let bound = SessionFrame::bound("m-1");
        assert_eq!(bound.idempotency_key(), Some("session_bound:m-1".into()));
        assert_eq!(bound.idempotency_id(), Some("m-1"));
        assert_eq!(
            serde_json::to_string(&bound).unwrap(),
            r#"{"type":"session_bound","rebon_session_id":"m-1"}"#
        );
        assert_eq!(
            SessionFrame::message_with_id("m-1", Value::Null).idempotency_key(),
            Some("session_message:m-1".into())
        );
        assert_eq!(SessionFrame::message(Value::Null).idempotency_key(), None);
        let request = SessionFrame::PermissionRequest {
            request_id: "m-1".into(),
            request: Value::Null,
        };
        // Same id, different type: different keys.
        assert_eq!(
            request.idempotency_key(),
            Some("permission_request:m-1".into())
        );
        assert_eq!(request.idempotency_id(), Some("m-1"));
        assert_eq!(SessionFrame::Cancel.idempotency_id(), None);
    }

    #[test]
    fn a_delivered_frame_is_the_frame_plus_its_event_id() {
        for frame in every_variant() {
            let delivered = DeliveredFrame {
                event_id: Some(42),
                frame: frame.clone(),
            };
            let value = serde_json::to_value(&delivered).unwrap();
            assert_eq!(value[EVENT_ID_KEY], json!(42));
            let mut without = value.clone();
            without.as_object_mut().unwrap().remove(EVENT_ID_KEY);
            assert_eq!(without, serde_json::to_value(&frame).unwrap());
            assert_eq!(
                serde_json::from_value::<DeliveredFrame>(value).unwrap(),
                delivered
            );

            // Unstamped, it is exactly the frame.
            let unstamped = DeliveredFrame::unstamped(frame.clone());
            let value = serde_json::to_value(&unstamped).unwrap();
            assert_eq!(value, serde_json::to_value(&frame).unwrap());
            assert_eq!(
                serde_json::from_value::<DeliveredFrame>(value).unwrap(),
                unstamped
            );
        }
    }

    #[test]
    fn an_unknown_frame_does_not_swallow_the_event_id() {
        let delivered: DeliveredFrame =
            serde_json::from_str(r#"{"event_id":9,"type":"telemetry","x":1}"#).unwrap();
        assert_eq!(delivered.event_id, Some(9));
        let SessionFrame::Other(unknown) = &delivered.frame else {
            panic!("expected Other");
        };
        assert!(!unknown.payload.contains_key(EVENT_ID_KEY));
    }

    #[test]
    fn a_malformed_event_id_is_refused_even_when_null() {
        for raw in [
            r#"{"event_id":null,"type":"cancel"}"#,
            r#"{"event_id":-1,"type":"cancel"}"#,
            r#"{"event_id":"7","type":"cancel"}"#,
            r#"{"event_id":1.5,"type":"cancel"}"#,
        ] {
            assert!(
                serde_json::from_str::<DeliveredFrame>(raw).is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn stamping_splices_the_id_in_and_keeps_every_other_byte() {
        // Key order and spacing a re-encode would change.
        let stored = r#"{"type":"session_message", "message":{"z":1,"a":2}}"#;
        let stamped = stamp_event_id(stored, 17).unwrap();
        assert_eq!(
            stamped,
            r#"{"event_id":17,"type":"session_message", "message":{"z":1,"a":2}}"#
        );
        let delivered: DeliveredFrame = serde_json::from_str(&stamped).unwrap();
        assert_eq!(delivered.event_id, Some(17));
        assert_eq!(
            delivered.frame,
            serde_json::from_str::<SessionFrame>(stored).unwrap()
        );

        // Leading whitespace is fine; the id is the largest there is.
        let stamped = stamp_event_id("  {\"type\":\"cancel\"}", u64::MAX).unwrap();
        let delivered: DeliveredFrame = serde_json::from_str(&stamped).unwrap();
        assert_eq!(delivered.event_id, Some(u64::MAX));
        assert_eq!(delivered.frame, SessionFrame::Cancel);

        for not_a_frame in ["", "{}", "{ }", "[1]", "\"x\"", "{\"type\":\"cancel\""] {
            assert_eq!(stamp_event_id(not_a_frame, 1), None, "{not_a_frame}");
        }
    }

    #[test]
    fn the_state_vocabulary_is_closed_with_a_fallback() {
        let words: Vec<&str> = SessionRunState::KNOWN
            .iter()
            .map(SessionRunState::as_str)
            .collect();
        assert_eq!(
            words,
            vec![
                "starting",
                "running",
                "idle",
                "needs_input",
                "stopped",
                "failed"
            ]
        );
        for state in SessionRunState::KNOWN {
            assert!(state.is_known());
            assert_eq!(SessionRunState::parse(state.as_str()), state);
            let json = serde_json::to_value(&state).unwrap();
            assert_eq!(json, json!(state.as_str()));
            assert_eq!(
                serde_json::from_value::<SessionRunState>(json).unwrap(),
                state
            );
        }
        assert_eq!(
            SessionRunState::KNOWN.map(|state| state.is_terminal()),
            [false, false, false, false, true, true]
        );

        // A newer worker's word survives, verbatim.
        let future = SessionRunState::parse("compacting");
        assert_eq!(future, SessionRunState::Other("compacting".into()));
        assert!(!future.is_known() && !future.is_terminal());
        assert_eq!(serde_json::to_value(&future).unwrap(), json!("compacting"));
        assert_eq!(future, "compacting");
        assert_eq!(future.to_string(), "compacting");
        // Case matters: this is a wire word, not prose.
        assert!(!SessionRunState::parse("Idle").is_known());

        assert!(serde_json::from_value::<SessionRunState>(json!("")).is_err());
        assert!(serde_json::from_value::<SessionRunState>(json!(3)).is_err());
    }

    #[test]
    fn a_session_state_frame_carries_the_vocabulary() {
        let frame: SessionFrame = serde_json::from_str(
            r#"{"type":"session_state","state":"needs_input","detail":"Bash"}"#,
        )
        .unwrap();
        assert_eq!(
            frame,
            SessionFrame::SessionState {
                state: SessionRunState::NeedsInput,
                detail: Some("Bash".into())
            }
        );
        let future: SessionFrame =
            serde_json::from_str(r#"{"type":"session_state","state":"compacting"}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&future).unwrap(),
            r#"{"type":"session_state","state":"compacting"}"#
        );
        assert!(
            serde_json::from_str::<SessionFrame>(r#"{"type":"session_state","state":""}"#).is_err()
        );
    }

    #[test]
    fn a_question_response_has_the_documented_wire_shape() {
        let frame = SessionFrame::QuestionResponse {
            request_id: "perm-1".into(),
            answers: vec![
                QuestionAnswer::options([1]),
                QuestionAnswer::text("Fridays"),
            ],
        };
        assert_eq!(
            serde_json::to_string(&frame).unwrap(),
            r#"{"type":"question_response","request_id":"perm-1","answers":[{"selected_options":[1]},{"selected_options":[],"other_text":"Fridays"}]}"#
        );
        assert_eq!(frame.origin(), Some(FrameOrigin::Controller));
        // Stored every time, like a permission decision.
        assert_eq!(frame.idempotency_key(), None);
        assert_eq!(frame.idempotency_id(), None);

        // Missing members are their empty values; `null` text is absent.
        let sparse: SessionFrame = serde_json::from_str(
            r#"{"type":"question_response","request_id":"perm-1","answers":[{},{"other_text":null}]}"#,
        )
        .unwrap();
        assert_eq!(
            sparse,
            SessionFrame::QuestionResponse {
                request_id: "perm-1".into(),
                answers: vec![QuestionAnswer::default(), QuestionAnswer::default()],
            }
        );
        // An empty list is well-formed; the runner is the one to refuse it.
        assert!(serde_json::from_str::<SessionFrame>(
            r#"{"type":"question_response","request_id":"perm-1","answers":[]}"#
        )
        .is_ok());
    }

    #[test]
    fn a_question_answer_reads_the_camel_case_spelling_and_ignores_extras() {
        let frame: SessionFrame = serde_json::from_str(
            r#"{"type":"question_response","request_id":"p","answers":[{"selectedOptions":[0,1],"otherText":"and more","future":true}]}"#,
        )
        .unwrap();
        assert_eq!(
            frame,
            SessionFrame::QuestionResponse {
                request_id: "p".into(),
                answers: vec![QuestionAnswer {
                    selected_options: vec![0, 1],
                    other_text: Some("and more".into()),
                }],
            }
        );
    }

    #[test]
    fn a_build_that_does_not_know_question_response_keeps_it_verbatim() {
        // What an older runner or server sees: an unknown type, not an
        // error, and the object survives a relay unchanged.
        let text = serde_json::to_string(&SessionFrame::QuestionResponse {
            request_id: "perm-1".into(),
            answers: vec![QuestionAnswer::options([0])],
        })
        .unwrap();
        let mut object: Map<String, Value> = serde_json::from_str(&text).unwrap();
        object.insert("type".into(), json!("question_response_from_the_future"));
        let unknown = SessionFrame::from_object::<serde_json::Error>(object.clone()).unwrap();
        assert!(matches!(unknown, SessionFrame::Other(_)));
        assert_eq!(unknown.origin(), None);
        assert_eq!(
            serde_json::to_value(&unknown).unwrap(),
            Value::Object(object)
        );
    }

    fn holder() -> AnsweredBy {
        AnsweredBy {
            device_id: "dev_phone".into(),
            label: Some("pixel".into()),
            connection_id: Some("c-00000000000000ff".into()),
            event_id: Some(41),
        }
    }

    #[test]
    fn an_already_answered_refusal_has_the_documented_wire_shape() {
        let frame = SessionFrame::already_answered("perm-1", holder());
        assert_eq!(
            serde_json::to_value(&frame).unwrap(),
            json!({
                "type": "stream_error",
                "code": "already_answered",
                "message": "perm-1 was already answered by pixel",
                "request_id": "perm-1",
                "answered_by": {
                    "device_id": "dev_phone",
                    "label": "pixel",
                    "connection_id": "c-00000000000000ff",
                    "event_id": 41
                }
            })
        );
        assert_eq!(round_trip(&frame), frame);
        assert_eq!(frame.origin(), Some(FrameOrigin::Server));
        assert_eq!(frame.idempotency_key(), None);

        // Without a label the device id is what a person is shown, and
        // the absent members are left out rather than sent as null.
        let bare = AnsweredBy {
            device_id: "dev_laptop".into(),
            label: None,
            connection_id: None,
            event_id: None,
        };
        let frame = SessionFrame::already_answered("perm-2", bare.clone());
        let SessionFrame::StreamError { message, .. } = &frame else {
            unreachable!()
        };
        assert_eq!(message, "perm-2 was already answered by dev_laptop");
        assert_eq!(
            serde_json::to_value(&frame).unwrap()["answered_by"],
            json!({"device_id": "dev_laptop"})
        );
        assert_eq!(round_trip(&frame), frame);
        let empty_label = AnsweredBy {
            label: Some(String::new()),
            ..bare
        };
        assert_eq!(empty_label.display_name(), "dev_laptop");
    }

    #[test]
    fn a_stream_error_reads_with_and_without_the_answer_fields() {
        // What an older server sends: just a code and a message.
        let old: SessionFrame =
            serde_json::from_str(r#"{"type":"stream_error","code":"no_worker","message":"m"}"#)
                .unwrap();
        assert_eq!(old, SessionFrame::stream_error("no_worker", "m"));
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            r#"{"type":"stream_error","code":"no_worker","message":"m"}"#
        );
        // `null` members are absent.
        let nulls: SessionFrame = serde_json::from_str(
            r#"{"type":"stream_error","code":"no_worker","message":"m","request_id":null,"answered_by":null}"#,
        )
        .unwrap();
        assert_eq!(nulls, old);
        // A newer holder with members this build does not know still
        // reads, which is the same tolerance an older controller gives
        // the whole `answered_by` object.
        let newer: SessionFrame = serde_json::from_str(
            r#"{"type":"stream_error","code":"already_answered","message":"m","request_id":"p","answered_by":{"device_id":"d","platform":"ios"},"retry_after":3}"#,
        )
        .unwrap();
        let SessionFrame::StreamError {
            request_id,
            answered_by,
            ..
        } = newer
        else {
            panic!("expected a stream error");
        };
        assert_eq!(request_id.as_deref(), Some("p"));
        assert_eq!(answered_by.unwrap().device_id, "d");
        // Malformed members of a known frame are still a bug.
        for raw in [
            r#"{"type":"stream_error","code":"x","message":"m","request_id":7}"#,
            r#"{"type":"stream_error","code":"x","message":"m","answered_by":"dev"}"#,
            r#"{"type":"stream_error","code":"x","message":"m","answered_by":{}}"#,
        ] {
            assert!(
                serde_json::from_str::<SessionFrame>(raw).is_err(),
                "{raw} should not parse"
            );
        }
    }

    #[test]
    fn only_answers_name_the_prompt_they_answer() {
        let answered: Vec<(String, Option<String>)> = every_variant()
            .iter()
            .map(|frame| {
                (
                    frame.frame_type().to_string(),
                    frame.answered_request_id().map(str::to_string),
                )
            })
            .collect();
        for (frame_type, request_id) in answered {
            match frame_type.as_str() {
                "permission_response" | "question_response" => {
                    assert_eq!(request_id.as_deref(), Some("req-1"), "{frame_type}")
                }
                // A control request has a request id too, but it names
                // the request itself, not a prompt it answers.
                _ => assert_eq!(request_id, None, "{frame_type}"),
            }
        }
    }

    #[test]
    fn close_codes_are_distinct() {
        let codes = [
            close_code::TIMEOUT,
            close_code::LEASE_GONE,
            close_code::RESOURCE_LIMIT,
            close_code::PROTOCOL_ERROR,
            close_code::SUPERSEDED,
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len());
        // All inside the 4000-4999 application range.
        assert!(codes.iter().all(|code| (4000..5000).contains(code)));
    }
}
