//! The `_session/*` ACP extension: what a client and a session owner say to
//! each other once the internal control plane speaks JSON-RPC.
//!
//! ACP covers the four things every agent protocol has — prompt, cancel, the
//! permission round trip, and the update stream. It does not cover what a
//! *hosted* session needs beside those: a lease, a status snapshot, a session
//! option, a cursor to resume a stream from, a way to give up on a call that
//! is still running. Those are the methods here, named `_session/*` the way
//! `_session/steering` already is, so a reader can tell an
//! extension from the standard at a glance.
//!
//! What is deliberately **not** here:
//!
//! - `_session/steering`, which already exists as
//!   `rebon_proto::types::SessionSteeringParams`. The old `Steer` request maps
//!   onto it unchanged; a second declaration of the same method would be the
//!   drift this module exists to avoid.
//! - Anything the standard already covers. A prompt is `session/prompt`, a
//!   cancel is `session/cancel`, a permission is `session/request_permission`
//!   in the owner-to-client direction, and a delta is `session/update`. The
//!   rebon-specific facts those four have nowhere to put — a cursor, a cancel
//!   fence, which pending permission an answer is about — ride in
//!   [`RebonMeta`] instead of bending a standard shape. `_session/enqueue`
//!   separately acknowledges queued delivery without waiting for execution;
//!   it does not replace the standard prompt's turn result.
//!
//! **Why this module is in `rebon-session-host` and not `rebon-proto`.** It does
//! not work: every payload these methods carry — [`SessionStatusSnapshot`],
//! not work: every payload these methods carry — [`SessionStatusSnapshot`],
//! [`ClientLeaseKind`], [`RewindScopeWire`], [`CommandOutput`], the cancel
//! fence, the question answers — is declared in this crate, which sits *above*
//! `rebon-proto`. Moving that closure down to reach `rebon-proto` would touch
//! this crate, the runtime, the binary and the app to gain nothing: the reason
//! `web_api` lives in `rebon-proto` is that the *web page* consumes it, and
//! nothing below this crate consumes `_session/*`. So the types live beside
//! the payloads they carry, and the schema exporter reaches them through the
//! `schema` feature this crate already had.
//!
//! Every type here is wire, so every field is `camelCase` and every optional
//! is skipped when absent. The tests at the bottom pin that per type.

use serde::{Deserialize, Serialize};

use crate::legacy_foreground::ForegroundQuestionAnswer;
use crate::protocol::{
    BackgroundIpcCancelFence, ClientLeaseKind, CommandOutput, RewindScopeWire,
    SessionStatusSnapshot, TurnStreamState,
};

/// The key rebon's own facts hang under inside a message's `_meta`.
///
/// Namespaced rather than flat because `_meta` is shared: an ACP peer may put
/// its own keys there — `initialize` already carries `steering` — and a bare
/// `cursor` at the top of `_meta` would be a claim on a name nobody agreed to.
pub const META_NAMESPACE: &str = "rebon";

/// What rebon hangs at `_meta.rebon` on standard ACP messages.
///
/// The standard shapes are fixed, and several rebon facts have to ride along
/// anyway: which cursor a `session/update` carries, which pending permission a
/// `session/request_permission` is about, the fence a `session/cancel` is only
/// valid against. ACP reserves `_meta` for exactly this, so nothing standard
/// is bent to fit.
///
/// Every field is optional and skipped when absent, so a message that needs
/// one fact does not carry the other nine as nulls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RebonMeta {
    /// The token a client presents on `initialize`. Checked once, when the
    /// connection is established, because the connection is what it
    /// authenticates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Where in the owner's numbering a `session/update` sits.
    ///
    /// In `_meta` rather than in the standard params because the client's
    /// de-duplication reads it (`crate::client::stream_watermark`) and the
    /// standard shape has nowhere to put it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
    /// Which numbering that cursor belongs to. An owner's cursors restart when
    /// it does, so a cursor from a previous epoch is not comparable with this
    /// one and must not be used to skip anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    /// Which pending permission a `session/request_permission` is about.
    ///
    /// Carried because the owner re-sends still-pending permissions to a
    /// client that attached after they were raised, so the answer is merged by
    /// this id rather than by which connection it arrived on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_id: Option<u64>,
    /// The turn a permission, an answer or a cancel belongs to, so a stale one
    /// is refused rather than applied to whatever is running now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_generation: Option<u64>,
    /// Free-form text alongside a permission answer.
    ///
    /// The standard result carries an option id and an updated input and has
    /// nowhere for this, and it is not decoration: with no option selected it
    /// *is* the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_text: Option<String>,
    /// What a `session/cancel` is only valid against.
    ///
    /// Standard `session/cancel` is a bare notification, which is fine when a
    /// client and an agent share one process and cannot disagree about what is
    /// running. Across a socket they can, so the cancel says which turn it
    /// meant and the owner drops it if that is no longer the turn in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence: Option<BackgroundIpcCancelFence>,
    /// The job this addresses. A fence, not a routing key: a worker that has
    /// been replaced must not act on a command meant for its predecessor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// The session this addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Makes a retry idempotent: the owner answers a repeat of the same id
    /// from its recent-results memory instead of running the command twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
}

impl RebonMeta {
    /// True when there is nothing to send, so a caller can leave `_meta` off
    /// entirely rather than write `{"rebon":{}}`.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// This, wrapped as the `_meta` object it belongs in.
    ///
    /// `None` when empty, because an absent `_meta` and an empty one mean the
    /// same thing to every reader and only one of them is worth sending.
    pub fn to_meta(&self) -> Option<serde_json::Value> {
        if self.is_empty() {
            return None;
        }
        Some(serde_json::json!({ META_NAMESPACE: self }))
    }

    /// The rebon half of a message's `_meta`.
    ///
    /// A missing `_meta`, an `_meta` from a peer that put only its own keys
    /// there, and an `_meta.rebon` that fails to decode all read as "no rebon
    /// facts": none of them is a reason to reject a message whose standard
    /// half is well formed.
    pub fn from_meta(meta: Option<&serde_json::Value>) -> Self {
        meta.and_then(|meta| meta.get(META_NAMESPACE))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }
}

/// The method names, so the server and the client cannot drift.
pub mod method {
    /// Liveness. Answers even mid-turn.
    pub const PING: &str = "_session/ping";
    /// One status snapshot, the same one the stream pushes on change.
    pub const STATUS: &str = "_session/status";
    /// Enqueue a noninterrupting reply and acknowledge delivery, without waiting
    /// for its turn. Params use the standard `SessionPromptParams` shape.
    pub const ENQUEUE: &str = "_session/enqueue";
    /// Run a slash command on the session's turn loop.
    pub const RUN_COMMAND: &str = "_session/run_command";
    /// Switch the live permission mode.
    pub const SET_PERMISSION_MODE: &str = "_session/set_permission_mode";
    /// Set one session option: model, effort, agent.
    pub const SET_OPTION: &str = "_session/set_option";
    /// Rewind the conversation, the code, or both.
    pub const REWIND: &str = "_session/rewind";
    /// Compact the history now.
    pub const COMPACT: &str = "_session/compact";
    /// Re-read the plugin switches.
    pub const RECONCILE_PLUGINS: &str = "_session/reconcile_plugins";
    /// Answer an interactive question (AskUserQuestion), which has no standard
    /// counterpart.
    pub const ANSWER_QUESTIONS: &str = "_session/answer_questions";
    /// Send a message to one running task.
    pub const TASK_REPLY: &str = "_session/task_reply";
    /// Stop named tasks.
    pub const CANCEL_TASKS: &str = "_session/cancel_tasks";
    /// Take or renew this client's lease on the session.
    pub const LEASE: &str = "_session/lease";
    /// Give one up.
    pub const RELEASE_LEASE: &str = "_session/release_lease";
    /// Open the event stream, optionally from a cursor.
    pub const SUBSCRIBE: &str = "_session/subscribe";
    /// Stop waiting for a call that is still running.
    pub const CANCEL_CALL: &str = "_session/cancel_call";

    /// Owner to client: the first message on a new subscription.
    pub const HELLO: &str = "_session/hello";
    /// Owner to client: a turn started or ended.
    pub const TURN: &str = "_session/turn";
    /// Owner to client: the status snapshot changed.
    pub const STATUS_CHANGED: &str = "_session/status_changed";
    /// Owner to client: deltas were dropped from the ring; read the file.
    pub const GAP: &str = "_session/gap";

    /// Every method this module names, in the order they are declared above.
    /// The dispatcher and the docs read this rather than each keeping a list
    /// that can fall behind.
    pub const ALL: &[&str] = &[
        PING,
        STATUS,
        ENQUEUE,
        RUN_COMMAND,
        SET_PERMISSION_MODE,
        SET_OPTION,
        REWIND,
        COMPACT,
        RECONCILE_PLUGINS,
        ANSWER_QUESTIONS,
        TASK_REPLY,
        CANCEL_TASKS,
        LEASE,
        RELEASE_LEASE,
        SUBSCRIBE,
        CANCEL_CALL,
        HELLO,
        TURN,
        STATUS_CHANGED,
        GAP,
    ];
}

/// A method that takes nothing: [`method::PING`], [`method::STATUS`],
/// [`method::RECONCILE_PLUGINS`].
///
/// A struct rather than `null` so adding a field later is not a wire break: an
/// old peer sending `{}` still decodes, and a new peer's extra key is ignored
/// rather than refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct NoParams {}

/// A method that answers nothing but "done".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Ack {}

/// [`method::RUN_COMMAND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RunCommandParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

/// What a slash command produced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RunCommandResult {
    /// Absent for a command that ran and printed nothing. A command that
    /// *failed* is a JSON-RPC error instead, so this is never an error channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<CommandOutput>,
}

/// [`method::STATUS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct StatusResult {
    pub snapshot: SessionStatusSnapshot,
}

/// [`method::SET_PERMISSION_MODE`]. `mode` is a `PermissionMode` wire value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SetPermissionModeParams {
    pub mode: String,
}

/// [`method::SET_OPTION`].
///
/// Separate from running the slash command a human would type, because a
/// client that wants to *set* a value should not have to compose that command
/// and the owner should not have to parse it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SetOptionParams {
    pub key: String,
    pub value: String,
}

/// [`method::REWIND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RewindParams {
    pub user_message_uuid: String,
    #[serde(default)]
    pub scope: RewindScopeWire,
}

/// [`method::COMPACT`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CompactParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// [`method::ANSWER_QUESTIONS`]: the answer to an AskUserQuestion.
///
/// Its own method rather than the standard permission answer because
/// AskUserQuestion is several questions with several selected options each,
/// and the standard result carries one option id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AnswerQuestionsParams {
    pub query_id: u64,
    /// Fences the answer against the turn that asked. Defaulted for the wire
    /// so a peer from before this field reads as turn zero, which is what it
    /// always meant.
    #[serde(default)]
    pub turn_generation: u64,
    pub answers: Vec<ForegroundQuestionAnswer>,
}

/// [`method::TASK_REPLY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TaskReplyParams {
    pub task_id: String,
    pub message: String,
}

/// [`method::CANCEL_TASKS`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CancelTasksParams {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_ids: Vec<String>,
}

/// [`method::LEASE`]. While any lease is live the owner stays up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct LeaseParams {
    pub client_id: String,
    pub kind: ClientLeaseKind,
}

/// [`method::RELEASE_LEASE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ReleaseLeaseParams {
    pub client_id: String,
    /// Whether the user meant to be done, as opposed to merely detaching.
    ///
    /// Only decides how long the owner lingers, and only when this was the
    /// last lease. Defaulted for the wire: a client from before this field
    /// reads as `false`, which is what every release did until it existed.
    #[serde(default)]
    pub deliberate: bool,
}

/// [`method::SUBSCRIBE`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SubscribeParams {
    /// Resume from just after this cursor. Absent means "from wherever the
    /// owner's ring starts", and the [`method::HELLO`] that follows says where
    /// that turned out to be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

/// [`method::CANCEL_CALL`].
///
/// Not a cancel of the *turn*: that is `session/cancel`. This releases one
/// call, because a client timeout that only drops its own receiver leaves the
/// owner working on something nobody will read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CancelCallParams {
    pub command_id: String,
}

/// Whether a waiter was actually let go.
///
/// Always an ok answer: cancelling a call that is not running is not a
/// failure, it is "nothing to release" — which is what a client that raced its
/// own timeout needs to hear.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CancelCallResult {
    pub released: bool,
}

/// [`method::HELLO`], the first message on a new subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct HelloParams {
    /// The owner's numbering. It restarts each time an owner starts, so a
    /// cursor alone cannot be compared with one written by its predecessor.
    pub epoch: u64,
    /// Where the stream is resuming from.
    pub cursor: u64,
    pub status: SessionStatusSnapshot,
}

/// [`method::TURN`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TurnParams {
    pub state: TurnStreamState,
    /// Why a turn ended. Absent on the one that started, and on an end whose
    /// reason the owner does not have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Why a cancel did not stop this turn.
    ///
    /// The turn is still running when this is set -- `state` says so. It is
    /// here rather than in the answer to whoever asked, because every client
    /// watching is looking at a turn that did not stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_refused: Option<String>,
}

/// [`method::STATUS_CHANGED`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct StatusChangedParams {
    pub snapshot: SessionStatusSnapshot,
}

/// [`method::GAP`]: the owner dropped deltas from its ring.
///
/// What is missing between `from` and `to` is in the events file, which is the
/// authority; the client reads it from there and resumes. A gap is never an
/// error — an owner that outran a slow client is doing the right thing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct GapParams {
    pub from: u64,
    pub to: u64,
}

/// The session options an owner accepts, and how each one takes effect.
///
/// Down here rather than beside the owner because both halves read it: the
/// owner decides whether a `_session/set_option` runs on the turn loop, and
/// the client has to know the same thing to read the answer back -- one shape
/// wraps a command's output, the other does not. Two copies of this would
/// disagree about a key with a stray slash or a capital letter, and the
/// symptom would be a client mis-reading an answer it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOptionKey {
    /// Read out of the job record when the next turn is built.
    Effort,
    /// Read out of the job record when the session is next built.
    Model,
    /// Switched on the live session by the turn loop.
    Agent,
    /// Same, for which kernel loop runs it.
    Kernel,
    Unknown,
}

/// Which option a key names, however it was spelled.
///
/// Lenient on purpose: these arrive from a slash command a person typed as
/// well as from a client that composed one, so a leading slash, surrounding
/// space and capitals all mean what they look like.
pub fn session_option_key(key: &str) -> SessionOptionKey {
    match key
        .trim()
        .trim_start_matches('/')
        .to_ascii_lowercase()
        .as_str()
    {
        "effort" => SessionOptionKey::Effort,
        "model" => SessionOptionKey::Model,
        "agent" | "backend" => SessionOptionKey::Agent,
        "kernel" => SessionOptionKey::Kernel,
        _ => SessionOptionKey::Unknown,
    }
}

/// The method and params one internal request is spelled as on the ACP wire.
///
/// The exact inverse of the owner's own routing, and the two are pinned
/// against each other: the server's tests translate every one of these back
/// and assert it is the request it started as. That is the only way two
/// spellings of one protocol stay one protocol.
///
/// `None` for requests whose spelling needs connection/session context. Reply
/// uses `_session/enqueue` when advertised (old workers use `session/prompt`)
/// with the standard prompt params. Steer and cancel use `_session/steering`
/// and `session/cancel`; permission answers respond to a pending server call.
/// The shared OwnerHandle supplies these shapes, not individual frontends.
pub fn method_and_params(
    request: &crate::protocol::BackgroundIpcRequest,
) -> Option<(&'static str, serde_json::Value)> {
    use crate::protocol::BackgroundIpcRequest as R;
    let (name, params) = match request {
        R::Ping => (method::PING, serde_json::json!({})),
        R::Status => (method::STATUS, serde_json::json!({})),
        R::ReconcilePlugins => (method::RECONCILE_PLUGINS, serde_json::json!({})),
        R::RunCommand { name, args } => (
            method::RUN_COMMAND,
            serde_json::json!({ "name": name, "args": args }),
        ),
        R::SetPermissionMode { mode } => (
            method::SET_PERMISSION_MODE,
            serde_json::json!({ "mode": mode }),
        ),
        R::SetSessionOption { key, value } => (
            method::SET_OPTION,
            serde_json::json!({ "key": key, "value": value }),
        ),
        R::Rewind {
            user_message_uuid,
            scope,
        } => (
            method::REWIND,
            serde_json::json!({ "userMessageUuid": user_message_uuid, "scope": scope }),
        ),
        R::Compact { instructions } => (
            method::COMPACT,
            serde_json::json!({ "instructions": instructions }),
        ),
        R::AnswerQuestions {
            query_id,
            turn_generation,
            answers,
        } => (
            method::ANSWER_QUESTIONS,
            serde_json::json!({
                "queryId": query_id,
                "turnGeneration": turn_generation,
                "answers": answers,
            }),
        ),
        R::ReplyTask { task_id, message } => (
            method::TASK_REPLY,
            serde_json::json!({ "taskId": task_id, "message": message }),
        ),
        R::CancelTasks { task_ids } => (
            method::CANCEL_TASKS,
            serde_json::json!({ "taskIds": task_ids }),
        ),
        R::Lease { client_id, kind } => (
            method::LEASE,
            serde_json::json!({ "clientId": client_id, "kind": kind }),
        ),
        R::ReleaseLease {
            client_id,
            deliberate,
        } => (
            method::RELEASE_LEASE,
            serde_json::json!({ "clientId": client_id, "deliberate": deliberate }),
        ),
        R::CancelCall { command_id } => (
            method::CANCEL_CALL,
            serde_json::json!({ "commandId": command_id }),
        ),
        R::Subscribe { since } => (method::SUBSCRIBE, serde_json::json!({ "since": since })),
        // Covered by the standard; see the note on this function.
        R::Reply { .. } | R::Steer { .. } | R::Cancel { .. } | R::PermissionAnswer { .. } => {
            return None
        }
    };
    Some((name, params))
}

/// Whether this request's answer is a command output rather than a payload.
///
/// The owner wraps those in `RunCommandResult`, so a client has to know which
/// it asked for to read the answer back. It is a property of the request, not
/// something to guess from the reply: `{}` is a valid answer either way.
pub fn answer_is_command_output(request: &crate::protocol::BackgroundIpcRequest) -> bool {
    use crate::protocol::BackgroundIpcRequest as R;
    match request {
        R::RunCommand { .. } | R::Compact { .. } | R::Rewind { .. } => true,
        // Two options are performed on the turn loop and answer with what the
        // slash command printed; the rest change the job record. Read from the
        // one table the owner reads, so a key with a stray slash or a capital
        // is classified the same on both sides.
        R::SetSessionOption { key, .. } => matches!(
            session_option_key(key),
            SessionOptionKey::Agent | SessionOptionKey::Kernel
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Absent optionals are absent, not `null`: a peer that has not learned a
    /// field yet must see the same bytes it always did.
    #[test]
    fn meta_writes_only_what_it_carries() {
        let meta = RebonMeta {
            cursor: Some(7),
            epoch: Some(2),
            ..RebonMeta::default()
        };
        assert_eq!(
            serde_json::to_value(&meta).unwrap(),
            json!({"cursor": 7, "epoch": 2})
        );
        assert_eq!(
            serde_json::to_value(RebonMeta::default()).unwrap(),
            json!({})
        );
    }

    /// Every field name on the wire is camelCase, including the ones whose
    /// Rust names are two words.
    #[test]
    fn meta_field_names_are_camel_case() {
        let meta = RebonMeta {
            query_id: Some(4),
            turn_generation: Some(9),
            extra_text: Some("because".into()),
            job_id: Some("bg-1".into()),
            session_id: Some("s-1".into()),
            command_id: Some("c-1".into()),
            token: Some("t".into()),
            ..RebonMeta::default()
        };
        assert_eq!(
            serde_json::to_value(&meta).unwrap(),
            json!({
                "token": "t",
                "queryId": 4,
                "turnGeneration": 9,
                "extraText": "because",
                "jobId": "bg-1",
                "sessionId": "s-1",
                "commandId": "c-1",
            })
        );
    }

    /// The facts hang under a namespace, and an empty one is left off entirely
    /// rather than sent as `{"rebon":{}}`.
    #[test]
    fn meta_hangs_under_the_rebon_key() {
        let meta = RebonMeta {
            cursor: Some(11),
            ..RebonMeta::default()
        };
        assert_eq!(meta.to_meta(), Some(json!({"rebon": {"cursor": 11}})));
        assert_eq!(RebonMeta::default().to_meta(), None);
    }

    /// A peer's own `_meta` keys, a missing `_meta`, and a malformed
    /// `_meta.rebon` all read as "no rebon facts" rather than as a reason to
    /// reject a message whose standard half is fine.
    #[test]
    fn meta_reads_back_leniently() {
        let wire = json!({"rebon": {"cursor": 5}, "someoneElse": {"x": 1}});
        assert_eq!(RebonMeta::from_meta(Some(&wire)).cursor, Some(5));
        assert_eq!(RebonMeta::from_meta(None), RebonMeta::default());
        assert_eq!(
            RebonMeta::from_meta(Some(&json!({"steering": {"supported": true}}))),
            RebonMeta::default()
        );
        assert_eq!(
            RebonMeta::from_meta(Some(&json!({"rebon": "not an object"}))),
            RebonMeta::default()
        );
    }

    /// The fence rides in `_meta` because standard `session/cancel` is a bare
    /// notification with nowhere to put it.
    #[test]
    fn a_cancel_carries_its_fence_in_meta() {
        let fence = BackgroundIpcCancelFence {
            status: crate::state::BackgroundJobStatus::Running,
            turn_generation: 3,
            updated_at_ms: 1_700_000_000_000,
            pending_permission_query_id: Some(8),
        };
        let meta = RebonMeta {
            fence: Some(fence),
            ..RebonMeta::default()
        };
        assert_eq!(
            meta.to_meta(),
            Some(json!({"rebon": {"fence": {
                "status": "running",
                "turnGeneration": 3,
                "updatedAtMs": 1_700_000_000_000u64,
                "pendingPermissionQueryId": 8,
            }}}))
        );
    }

    /// A method that takes nothing still takes an object, so a later field is
    /// not a wire break.
    #[test]
    fn the_empty_shapes_are_objects_not_null() {
        assert_eq!(serde_json::to_value(NoParams {}).unwrap(), json!({}));
        assert_eq!(serde_json::to_value(Ack {}).unwrap(), json!({}));
        assert_eq!(
            serde_json::from_value::<NoParams>(json!({"laterField": 1})).unwrap(),
            NoParams {}
        );
    }

    #[test]
    fn run_command_omits_empty_args() {
        assert_eq!(
            serde_json::to_value(RunCommandParams {
                name: "compact".into(),
                args: Vec::new(),
            })
            .unwrap(),
            json!({"name": "compact"})
        );
        assert_eq!(
            serde_json::to_value(RunCommandParams {
                name: "hooks".into(),
                args: vec!["list".into()],
            })
            .unwrap(),
            json!({"name": "hooks", "args": ["list"]})
        );
    }

    #[test]
    fn a_silent_command_answers_an_empty_object() {
        assert_eq!(
            serde_json::to_value(RunCommandResult::default()).unwrap(),
            json!({})
        );
        assert_eq!(
            serde_json::to_value(RunCommandResult {
                output: Some(CommandOutput {
                    text: "ok".into(),
                    tone: "info".into(),
                }),
            })
            .unwrap(),
            json!({"output": {"text": "ok", "tone": "info"}})
        );
    }

    #[test]
    fn rewind_defaults_to_the_conversation() {
        assert_eq!(
            serde_json::from_value::<RewindParams>(json!({"userMessageUuid": "u1"}))
                .unwrap()
                .scope,
            RewindScopeWire::Conversation
        );
        assert_eq!(
            serde_json::to_value(RewindParams {
                user_message_uuid: "u1".into(),
                scope: RewindScopeWire::Both,
            })
            .unwrap(),
            json!({"userMessageUuid": "u1", "scope": "both"})
        );
    }

    #[test]
    fn answering_questions_fences_on_the_turn_that_asked() {
        let params = AnswerQuestionsParams {
            query_id: 2,
            turn_generation: 6,
            answers: vec![ForegroundQuestionAnswer {
                selected_options: vec![1],
                other_text: None,
            }],
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            json!({
                "queryId": 2,
                "turnGeneration": 6,
                "answers": [{"selectedOptions": [1]}],
            })
        );
        // A peer from before the fence existed reads as turn zero, which is
        // what its answers always meant.
        assert_eq!(
            serde_json::from_value::<AnswerQuestionsParams>(json!({"queryId": 2, "answers": []}))
                .unwrap()
                .turn_generation,
            0
        );
    }

    #[test]
    fn a_lease_says_which_surface_holds_it() {
        assert_eq!(
            serde_json::to_value(LeaseParams {
                client_id: "tui-1".into(),
                kind: ClientLeaseKind::Tui,
            })
            .unwrap(),
            json!({"clientId": "tui-1", "kind": "tui"})
        );
    }

    /// Merely detaching is the default, so a client that vanished without
    /// sending anything gets the benefit of the doubt.
    #[test]
    fn releasing_a_lease_is_not_deliberate_unless_it_says_so() {
        assert!(
            !serde_json::from_value::<ReleaseLeaseParams>(json!({"clientId": "tui-1"}))
                .unwrap()
                .deliberate
        );
        assert_eq!(
            serde_json::to_value(ReleaseLeaseParams {
                client_id: "tui-1".into(),
                deliberate: true,
            })
            .unwrap(),
            json!({"clientId": "tui-1", "deliberate": true})
        );
    }

    #[test]
    fn subscribe_without_a_cursor_says_nothing_rather_than_null() {
        assert_eq!(
            serde_json::to_value(SubscribeParams::default()).unwrap(),
            json!({})
        );
        assert_eq!(
            serde_json::to_value(SubscribeParams { since: Some(3) }).unwrap(),
            json!({"since": 3})
        );
    }

    #[test]
    fn a_cancelled_call_answers_whether_it_released_one() {
        assert_eq!(
            serde_json::to_value(CancelCallResult { released: true }).unwrap(),
            json!({"released": true})
        );
        assert_eq!(
            serde_json::to_value(CancelCallResult::default()).unwrap(),
            json!({"released": false})
        );
    }

    #[test]
    fn a_turn_that_started_carries_no_stop_reason() {
        assert_eq!(
            serde_json::to_value(TurnParams {
                state: TurnStreamState::Running,
                stop_reason: None,
                stop_refused: None,
            })
            .unwrap(),
            json!({"state": "running"})
        );
        assert_eq!(
            serde_json::to_value(TurnParams {
                state: TurnStreamState::Idle,
                stop_reason: Some("end_turn".into()),
                stop_refused: None,
            })
            .unwrap(),
            json!({"state": "idle", "stopReason": "end_turn"})
        );
    }

    #[test]
    fn a_gap_names_both_ends() {
        assert_eq!(
            serde_json::to_value(GapParams { from: 4, to: 9 }).unwrap(),
            json!({"from": 4, "to": 9})
        );
    }

    /// The method names are the contract between the server and the client;
    /// all of them are extensions and say so.
    #[test]
    fn every_method_name_is_an_extension() {
        for name in method::ALL {
            assert!(
                name.starts_with("_session/"),
                "{name} is not marked as an extension"
            );
        }
    }

    /// `ALL` is what the dispatcher and the docs read, so a name declared and
    /// left out of it would be a method nobody routes.
    #[test]
    fn the_method_list_has_no_duplicates_and_holds_every_name() {
        let mut sorted = method::ALL.to_vec();
        sorted.sort_unstable();
        let mut unique = sorted.clone();
        unique.dedup();
        assert_eq!(sorted, unique, "a method name is listed twice");
        assert_eq!(method::ALL.len(), 20);
    }

    /// `_session/steering` is not declared here on purpose: it already exists
    /// in `rebon-proto`, and a second declaration is the drift this module is
    /// meant to prevent.
    #[test]
    fn steering_is_not_redeclared_here() {
        assert!(!method::ALL.contains(&"_session/steering"));
    }
}
