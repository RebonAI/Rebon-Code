//! The localhost control plane: one request/response wire, and one event stream.
//!
//! There is exactly one internal control plane and no second one beside it, and
//! the only permitted increment is an additive variant inside
//! [`BackgroundIpcRequest`].

use crate::*;
use serde::{Deserialize, Serialize};

/// Shape version of [`BackgroundIpcEnvelope`].
///
/// Bumped only when the *envelope* changes, never for a new
/// [`BackgroundIpcRequest`] variant: a peer that does not know a variant fails
/// to decode that one request and says so, which is the behaviour we want, and
/// bumping for every added command would force a lockstep release of the CLI
/// and the desktop app for no gain. Same rule the retired foreground mailbox
/// used.
///
/// `0` is "an envelope from before this existed".
pub const BACKGROUND_IPC_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundIpcEnvelope {
    /// Absent from every envelope written before the session-ownership work.
    #[serde(default)]
    pub protocol_version: u32,
    /// The job whose worker this is addressed to, used as a fence: a worker
    /// that has been replaced must not act on a command meant for the process
    /// it replaced.
    ///
    /// Optional on the wire so a client that reached the owner through
    /// `<sid>.owner.json` can address it by session alone — but a worker
    /// released before this field became optional requires it, so a client
    /// that knows the job id still sends it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// The session this addresses. With `job_id` absent it is the routing key;
    /// with both present the pair must agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Idempotency key. A retry after a lost connection carries the same id,
    /// and the owner answers the second one from its recent-results memory
    /// instead of running the command twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    pub token: String,
    pub request: BackgroundIpcRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BackgroundIpcCancelFence {
    pub status: BackgroundJobStatus,
    pub turn_generation: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_permission_query_id: Option<u64>,
}

impl BackgroundIpcCancelFence {
    pub fn from_state(state: &BackgroundJobState) -> Self {
        Self {
            status: state.process.status,
            turn_generation: state.process.turn_generation,
            updated_at_ms: state.process.updated_at_ms,
            pending_permission_query_id: state
                .outcome
                .pending_permission
                .as_ref()
                .map(|permission| permission.query_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CommandOutput {
    pub text: String,
    pub tone: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundIpcRequest {
    Ping,
    RunCommand {
        name: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Reply {
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<BackgroundImageAttachment>,
    },
    ReplyTask {
        task_id: String,
        message: String,
    },
    SetPermissionMode {
        mode: String,
    },
    PermissionAnswer {
        query_id: u64,
        #[serde(default)]
        turn_generation: u64,
        option_id: Option<String>,
        extra_text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_input: Option<serde_json::Value>,
    },
    AnswerQuestions {
        query_id: u64,
        #[serde(default)]
        turn_generation: u64,
        answers: Vec<ForegroundQuestionAnswer>,
    },
    CancelTasks {
        task_ids: Vec<String>,
    },
    Cancel {
        fence: BackgroundIpcCancelFence,
    },
    /// Everything a client must agree with the owner about (invariant I4),
    /// answered in one round trip. Replaces polling `<sid>.live.json`.
    Status,
    /// Change a session option the owner holds: model, effort, agent.
    ///
    /// Distinct from `RunCommand { name: "model", .. }` because a client that
    /// wants to *set* a value should not have to compose the slash command a
    /// human would type, and the owner should not have to parse it back.
    SetSessionOption {
        key: String,
        value: String,
    },
    /// Inject a message into the turn that is already running, rather than
    /// queueing one behind it the way `Reply` does.
    Steer {
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<BackgroundImageAttachment>,
    },
    /// Rewind the conversation (and optionally the files) to a user turn.
    ///
    /// The owner performs it, because the compare-and-swap this rewrites the
    /// transcript with is only sound against the chain the owner holds — a
    /// client doing it locally would leave the owner's in-memory chain
    /// pointing at uuids that no longer exist.
    Rewind {
        user_message_uuid: String,
        #[serde(default)]
        scope: RewindScopeWire,
    },
    /// Arm a one-shot compaction, as `/compact` does.
    Compact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    /// Re-read `settings.json`'s `plugins.<id>.enabled` switches and move the
    /// owner's plugin registry to them. Sent by whoever wrote a switch from
    /// another process (the desktop app, a mirror), so a running session
    /// reacts now instead of on its next start. The answer's `data` is the
    /// reconcile report: `{generation, loaded, unloaded, failed, cascaded}`.
    ReconcilePlugins,
    /// Take (or renew) this client's lease on the session. While any lease is
    /// live the owner stays up; when the last one expires it lingers and exits.
    Lease {
        client_id: String,
        kind: ClientLeaseKind,
    },
    /// Open the session's live event stream on this connection.
    ///
    /// Unlike every other request, the connection stays open afterwards and
    /// the owner writes [`SessionEvent`] lines to it until one side hangs up.
    /// `since` is the last cursor this client saw; the owner replays from
    /// there when it still has those events and answers with a
    /// [`SessionEvent::Gap`] when it does not.
    Subscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<u64>,
    },
    /// Give up this client's lease immediately instead of waiting for it to
    /// expire — what a clean `/exit` or window close does.
    ReleaseLease {
        client_id: String,
        /// Whether the user meant to be done, as opposed to merely detaching.
        ///
        /// `/exit` and a top-level Ctrl+C say yes. `/bg`, Ctrl+Z, and any
        /// other deliberate hand-off say no, because the session is supposed
        /// to outlive the terminal that started it — that is what they are
        /// for. A client that simply vanished never sends this at all, and
        /// gets the benefit of the doubt: its lease expires and the host
        /// lingers, because a dropped ssh connection is not a decision.
        ///
        /// Only decides the linger, and only when this was the last lease.
        /// Another client still holding the session keeps it up regardless,
        /// so closing one of two terminals ends nothing.
        ///
        /// Defaulted for the wire: a client from before this field sends
        /// `{client_id}` alone and reads as `false`, which is what every
        /// release did until now.
        #[serde(default)]
        deliberate: bool,
    },
    /// Stop waiting on `command_id`: the client that sent it has given up.
    ///
    /// The one permitted protocol increment, and it is additive inside this
    /// enum rather than a second protocol. It exists because a timeout that
    /// only drops the client's receiver leaves the owner working on something
    /// nobody will read, and leaves the client unable to tell a slow answer
    /// from a lost one.
    ///
    /// It is *not* [`Self::Cancel`]: that fences and interrupts the running
    /// turn, while this releases one call. An operation that has not started
    /// does not run; one that has reached a commit point it cannot be rolled
    /// back from finishes, and its late answer is dropped by id — a retry
    /// carrying the same `command_id` is then answered from the idempotency
    /// history with that same one result.
    ///
    /// An owner from before this variant existed fails to decode this one
    /// request and says so, which is exactly the right outcome: the client has
    /// already released its waiter and only sent this so the owner could
    /// release its own.
    CancelCall {
        command_id: String,
    },
}

/// One line of a session's live event stream.
///
/// This is what replaces polling: before it, a mirror re-read a file every
/// 100ms and a browser waited on the same cadence, so a token that existed in
/// the owner's memory took up to a tenth of a second to appear anywhere else,
/// and a client that fell behind had no way to say so. Here the owner pushes
/// each delta as it happens and numbers every line, so a client that drops a
/// connection says where it got to and a client that cannot keep up is told
/// exactly what it missed.
///
/// Every variant carries the `cursor` it was published at except `Gap`, which
/// describes a range rather than occupying one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SessionEvent {
    /// Always first on a subscription: where the stream is now, and everything
    /// the client must agree with the owner about before reading deltas.
    Hello {
        cursor: u64,
        turn_generation: u64,
        status: Box<SessionStatusSnapshot>,
        /// Which numbering `cursor` belongs to. An owner numbers from one
        /// each time it starts, so a cursor alone cannot be compared with
        /// one written by its predecessor; the epoch is what makes the
        /// stream's cursors and the ones stamped into the job's event log
        /// (see [`StreamStamp`]) the same sequence. Zero from an owner
        /// that does not stamp its log, whose deltas a client keeps taking
        /// from the file.
        #[serde(default)]
        epoch: u64,
    },
    /// One streaming update, exactly as the engine produced it.
    SessionUpdate {
        cursor: u64,
        update: serde_json::Value,
    },
    /// A turn started, ended, or refused to stop.
    Turn {
        cursor: u64,
        state: TurnStreamState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        /// Why a cancel did not stop this turn.
        ///
        /// The turn's Stop hook can keep a turn running, and
        /// somebody pressing stop is entitled to know that it did. It travels
        /// on the stream rather than only in the answer to whoever asked,
        /// because every client watching this session is looking at a turn
        /// that did not stop, not only the one who asked it to.
        ///
        /// Absent on every ordinary turn event, so the bytes of one are what
        /// they always were.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_refused: Option<String>,
    },
    /// Something in the shared session state changed (I4).
    Status {
        cursor: u64,
        snapshot: Box<SessionStatusSnapshot>,
    },
    /// A tool is waiting for an answer. Whichever client answers first wins;
    /// the rest see the pending permission clear on the next status.
    Permission {
        cursor: u64,
        query: Box<BackgroundPermissionQuerySnapshot>,
    },
    /// The client asked to resume from a cursor the owner no longer holds, or
    /// fell far enough behind to be dropped. Everything in `from..to` was
    /// missed; the transcript on disk is the authority for catching up.
    Gap { from: u64, to: u64 },
}

impl SessionEvent {
    /// Where this line sits in the stream. `None` for a gap, which spans a
    /// range instead of occupying a position.
    pub fn cursor(&self) -> Option<u64> {
        match self {
            Self::Hello { cursor, .. }
            | Self::SessionUpdate { cursor, .. }
            | Self::Turn { cursor, .. }
            | Self::Status { cursor, .. }
            | Self::Permission { cursor, .. } => Some(*cursor),
            Self::Gap { .. } => None,
        }
    }
}

/// Whether a turn is in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum TurnStreamState {
    Running,
    Idle,
}

/// What a [`BackgroundIpcRequest::Rewind`] should restore.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum RewindScopeWire {
    /// The transcript only; files on disk are left alone.
    #[default]
    Conversation,
    /// The files only, from the file-history snapshot.
    Code,
    /// Both, refused up front unless the code half can be applied cleanly.
    Both,
}

/// Which surface a lease belongs to. Carried so `rebon agents` and the Agent
/// View can say *who* is holding a session open rather than only that someone
/// is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ClientLeaseKind {
    Tui,
    App,
    Serve,
    Cli,
}

impl ClientLeaseKind {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::App => "app",
            Self::Serve => "serve",
            Self::Cli => "cli",
        }
    }
}

/// One client holding a session open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClientLease {
    pub client_id: String,
    pub kind: ClientLeaseKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub updated_at_ms: u64,
}

/// Where a job runs, from the point of view of who is waiting on it.
///
/// This is not about *what* the job does — it is about whether a client is
/// sitting in front of it, which is what decides how eagerly its host lets go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobPlacement {
    /// A session a terminal, window, or tab is driving. Its host exists to
    /// serve that client, so it stops sooner once the client is gone.
    Foreground,
    /// Started to run on its own, and expected to be picked up later.
    ///
    /// The default, so every job recorded before this existed reads as what it
    /// was.
    #[default]
    Background,
}

impl JobPlacement {
    /// How long this kind of host lingers after its last client leaves.
    ///
    /// A foreground session's host goes sooner than a background job's: the
    /// user closing the terminal usually meant it, and a parked owner keeps a
    /// whole plugin and MCP stack alive while it waits. An hour of that for a
    /// window somebody closed on purpose is an hour of somebody's machine.
    pub fn default_linger_ms(self) -> u64 {
        match self {
            Self::Foreground => 10 * 60 * 1000,
            Self::Background => 60 * 60 * 1000,
        }
    }

    /// Whether this job is background work rather than somebody's session.
    ///
    /// Serde skips the default so a record written before placements existed
    /// round-trips byte-identical; the surfaces that report finished work ask
    /// the same question.
    pub fn is_background(&self) -> bool {
        matches!(self, Self::Background)
    }
}

/// How long a lease stays valid without renewal.
///
/// The same 15s the supervisor's own client leases use, and for the same
/// reason: a client that renews every 5s survives two missed renewals, and a
/// client that died is noticed within one visible pause rather than one turn.
pub const CLIENT_LEASE_TTL_MS: u64 = 15_000;

/// How often a client should renew its lease.
pub const CLIENT_LEASE_RENEW_INTERVAL_MS: u64 = 5_000;

/// One MCP server the owner has configured, as the owner sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct McpServerSnapshot {
    pub name: String,
    /// `stdio`, `http` or `sse`.
    pub transport: String,
    /// Where the entry came from: the project's `.mcp.json`, the CLI, a
    /// plugin.
    pub source: String,
}

/// One tool the owner's MCP servers offer the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct McpToolSnapshot {
    /// The `mcp__<server>__<tool>` name the model calls it by.
    pub name: String,
    /// What its definition costs the context, approximately.
    pub tokens: u64,
}

/// The owner's MCP servers, for a client that hosts none of its own.
///
/// Names and states only, never a tool's schema: this rides on every
/// `status` event, and a client shows it rather than calling anything
/// through it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct McpStatusSnapshot {
    /// `loading`, `ready`, `failed` or `not configured`.
    pub loader: String,
    /// `ready`, `pending` or `not connected`.
    pub client: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<McpServerSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<McpToolSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Token spend for the session, as the status bars show it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SessionUsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
}

impl SessionUsageSnapshot {
    /// Fold one turn's usage into the running total.
    ///
    /// Saturating rather than wrapping: a session long enough to overflow a
    /// `u64` of tokens does not exist, and if the accounting ever went wrong
    /// a pinned ceiling is a far better failure than a counter that wraps to
    /// zero and reads as a fresh session.
    pub fn add_turn(&mut self, usage: &rebon_types::Usage) {
        self.input_tokens = self
            .input_tokens
            .saturating_add(u64::from(usage.input_tokens));
        self.output_tokens = self
            .output_tokens
            .saturating_add(u64::from(usage.output_tokens));
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(u64::from(usage.cache_read_input_tokens));
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(u64::from(usage.cache_creation_input_tokens));
    }
}

/// Everything a client must agree with the owner about (invariant I4).
///
/// The client keeps no authoritative copy of any of it: a permission mode, a
/// model, or a busy flag that a client believes and the owner does not is the
/// bug class this whole snapshot exists to remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusSnapshot {
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub cwd: String,
    pub status: BackgroundJobStatus,
    /// A model turn is in flight, so a client shows Stop rather than Send.
    pub busy: bool,
    /// Fences every answer and cancel against the turn it was meant for.
    pub turn_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub plan_mode: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_permission: Option<BackgroundPermissionQuerySnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_user_questions: Option<Vec<crate::legacy_foreground::ForegroundQuestion>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SessionUsageSnapshot>,
    /// The owner's MCP servers. `None` from an owner that hosts none, or one
    /// from before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpStatusSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_leases: Vec<ClientLease>,
    /// The `command_id` most recently processed, and how it went. A client that
    /// lost its connection mid-command reads the outcome here instead of
    /// guessing whether to retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command_id: Option<String>,
    #[serde(default)]
    pub last_command_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command_error: Option<String>,
    pub updated_at_ms: u64,
}

impl BackgroundIpcRequest {
    pub fn cancel_for(state: &BackgroundJobState) -> Self {
        Self::Cancel {
            fence: BackgroundIpcCancelFence::from_state(state),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundCommandResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<CommandOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundIpcResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The answer, for the requests that have one (`Status`, `Rewind`).
    ///
    /// A single response shape rather than one struct per request: a peer that
    /// does not understand the payload still reads `ok` and `error`, which is
    /// what a version skew needs, and adding the next data-returning command
    /// does not add another wire type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl BackgroundIpcResponse {
    /// An ack with no payload.
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            data: None,
        }
    }

    /// A refusal. The message is what the client shows the user, so it should
    /// say what to do about it.
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            data: None,
        }
    }

    /// An ack carrying the request's answer. A payload that cannot be
    /// serialized is reported as a failure rather than silently dropped: a
    /// client that asked for data and got a bare `ok` would read it as "there
    /// is nothing", which is a different fact.
    pub fn with_data<T: Serialize>(value: &T) -> Self {
        match serde_json::to_value(value) {
            Ok(data) => Self {
                ok: true,
                error: None,
                data: Some(data),
            },
            Err(err) => Self::failed(format!("could not serialize the response: {err}")),
        }
    }
}

#[cfg(test)]
mod session_ownership_protocol_tests {
    use super::*;

    fn runtime() -> BackgroundRuntimeFields {
        BackgroundRuntimeFields {
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

    fn lease(client_id: &str, updated_at_ms: u64) -> ClientLease {
        ClientLease {
            client_id: client_id.to_string(),
            kind: ClientLeaseKind::Tui,
            pid: None,
            updated_at_ms,
        }
    }

    fn state() -> BackgroundJobState {
        BackgroundJobState::new("prompt".to_string(), "/work".to_string(), runtime(), None)
    }

    /// A worker released before the job id became optional parses the envelope
    /// by requiring `jobId`. A new client that knows the job must therefore
    /// still send it, or every command to a worker lingering from the last
    /// release fails with a parse error.
    #[test]
    fn an_envelope_that_names_a_job_still_carries_it_on_the_wire() {
        let envelope = BackgroundIpcEnvelope {
            protocol_version: BACKGROUND_IPC_PROTOCOL_VERSION,
            job_id: Some("job-1".to_string()),
            session_id: Some("sess-1".to_string()),
            command_id: Some("cmd-1".to_string()),
            token: "token".to_string(),
            request: BackgroundIpcRequest::Ping,
        };
        let wire: serde_json::Value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(wire["jobId"], "job-1");
        assert_eq!(wire["protocolVersion"], 1);
        assert_eq!(wire["commandId"], "cmd-1");
    }

    /// And the other direction: an envelope written before any of these fields
    /// existed must still decode, or a new worker cannot serve an old client.
    #[test]
    fn an_envelope_from_before_the_new_fields_still_decodes() {
        let envelope: BackgroundIpcEnvelope =
            serde_json::from_str(r#"{"jobId":"job-1","token":"token","request":"ping"}"#)
                .expect("an envelope without the new fields is still an envelope");
        assert_eq!(envelope.protocol_version, 0);
        assert_eq!(envelope.job_id.as_deref(), Some("job-1"));
        assert!(envelope.command_id.is_none());
    }

    /// A client addressing an owner it found through `<sid>.owner.json` has no
    /// job id, and must be allowed to omit it rather than send a placeholder.
    #[test]
    fn an_envelope_addressed_by_session_omits_the_job_id_entirely() {
        let envelope = BackgroundIpcEnvelope {
            protocol_version: BACKGROUND_IPC_PROTOCOL_VERSION,
            job_id: None,
            session_id: Some("sess-1".to_string()),
            command_id: None,
            token: "token".to_string(),
            request: BackgroundIpcRequest::Status,
        };
        let wire: serde_json::Value = serde_json::to_value(&envelope).unwrap();
        assert!(
            wire.get("jobId").is_none(),
            "a null jobId would fail an older worker's parse instead of being ignored"
        );
    }

    #[test]
    fn a_response_payload_round_trips() {
        let snapshot = SessionUsageSnapshot {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        let response = BackgroundIpcResponse::with_data(&snapshot);
        assert!(response.ok);
        let back: SessionUsageSnapshot =
            serde_json::from_value(response.data.expect("payload")).unwrap();
        assert_eq!(back, snapshot);
    }

    #[test]
    fn renewing_a_lease_replaces_it_rather_than_adding_one() {
        let mut state = state();
        state.touch_client_lease(lease("tui-1", 1_000), 1_000);
        state.touch_client_lease(lease("tui-1", 2_000), 2_000);
        state.touch_client_lease(lease("app-1", 2_000), 2_000);

        assert_eq!(state.lease.client_leases.len(), 2);
        assert_eq!(state.lease.client_leases[0].updated_at_ms, 2_000);
    }

    /// The liveness rule the linger timer runs on: a client that stopped
    /// renewing is gone, and the owner must be able to see that without being
    /// told.
    #[test]
    fn a_lease_nobody_renewed_expires() {
        let mut state = state();
        state.touch_client_lease(lease("tui-1", 1_000), 1_000);

        let later = 1_000 + CLIENT_LEASE_TTL_MS + 1;
        assert!(!state.has_live_client_lease(later));
        state.expire_client_leases(later);
        assert!(state.lease.client_leases.is_empty());
    }

    /// A stamp from the future is a clock that moved, not a dead client.
    /// Expiring it would take the session away from someone who is still there.
    #[test]
    fn a_lease_stamped_in_the_future_is_kept() {
        let mut state = state();
        state.touch_client_lease(lease("tui-1", 10_000), 1_000);

        state.expire_client_leases(1_000);

        assert_eq!(state.lease.client_leases.len(), 1);
        assert!(state.has_live_client_lease(1_000));
    }

    /// The point of a lease: a client that is watching keeps its owner up.
    /// Without this the terminal that opened a session would sit there while
    /// the process hosting it timed out underneath.
    #[test]
    fn a_live_lease_pushes_the_linger_deadline_out() {
        let mut state = state();
        state.lease.linger_ms = Some(60_000);
        let idle_since = 1_000;
        assert_eq!(state.linger_deadline_ms(idle_since), 61_000);

        state.touch_client_lease(lease("tui-1", 50_000), 50_000);

        assert_eq!(
            state.linger_deadline_ms(idle_since),
            50_000 + CLIENT_LEASE_TTL_MS + 60_000,
            "the clock runs from when the lease would expire, not from when the job went idle"
        );
    }

    /// And once nobody is renewing, the deadline stops moving — a lease that
    /// has gone stale must not pin a worker forever.
    #[test]
    fn a_stale_lease_stops_extending_the_deadline() {
        let mut state = state();
        state.lease.linger_ms = Some(60_000);
        state.touch_client_lease(lease("tui-1", 1_000), 1_000);
        let deadline = state.linger_deadline_ms(1_000);

        // Some time later, with nobody renewing, the answer is unchanged.
        assert_eq!(state.linger_deadline_ms(1_000), deadline);
        assert!(deadline < 1_000 + CLIENT_LEASE_TTL_MS + 60_000 + 1);
    }

    /// A job that asked for its own linger gets it; the placement default is
    /// only a fallback.
    #[test]
    fn an_explicit_linger_overrides_the_placement_default() {
        let mut state = state();
        state.lease.linger_ms = Some(5_000);

        assert_eq!(state.linger_deadline_ms(1_000), 6_000);
    }

    /// A session someone was sitting in front of lets go sooner than a job
    /// that was started to run on its own: closing a terminal is usually
    /// deliberate, and a parked owner keeps a whole plugin stack alive.
    #[test]
    fn a_foreground_session_lingers_for_less_than_a_background_job() {
        let mut foreground = state();
        foreground.lease.placement = JobPlacement::Foreground;
        let background = state();

        assert!(
            foreground.linger_deadline_ms(0) < background.linger_deadline_ms(0),
            "a watched session's host must not outstay a background job's"
        );
        assert_eq!(
            background.lease.placement,
            JobPlacement::Background,
            "the default"
        );
    }

    /// A record written before placements existed must still read as what it
    /// was, and must not gain a field when it is written back.
    #[test]
    fn a_record_without_a_placement_is_a_background_job() {
        let state = state();
        let wire: serde_json::Value = serde_json::to_value(&state).unwrap();

        assert!(
            wire.get("placement").is_none(),
            "the default is not written, so old readers see the record they wrote"
        );
        assert_eq!(state.lease.placement, JobPlacement::Background);
    }

    #[test]
    fn releasing_a_lease_reports_whether_there_was_one() {
        let mut state = state();
        state.touch_client_lease(lease("tui-1", 1_000), 1_000);

        assert!(state.release_client_lease("tui-1", 1_000));
        assert!(!state.release_client_lease("tui-1", 1_000));
    }
}

#[cfg(test)]
mod mcp_status_snapshot_tests {
    use super::*;

    /// An owner from before the field, or one hosting no servers, says
    /// nothing about MCP — and a status without it still reads.
    #[test]
    fn a_status_without_mcp_still_reads() {
        let json = serde_json::json!({
            "jobId": "job-1",
            "cwd": ".",
            "status": serde_json::to_value(BackgroundJobStatus::Idle).unwrap(),
            "busy": false,
            "turnGeneration": 1,
            "updatedAtMs": 1,
        });
        let snapshot: SessionStatusSnapshot = serde_json::from_value(json).expect("reads");
        assert!(snapshot.mcp.is_none());
        let written = serde_json::to_value(&snapshot).expect("writes");
        assert!(
            written.get("mcp").is_none(),
            "nothing to say is left unsaid"
        );
    }

    #[test]
    fn the_mcp_snapshot_round_trips_in_camel_case() {
        let snapshot = McpStatusSnapshot {
            loader: "ready".into(),
            client: "ready".into(),
            servers: vec![McpServerSnapshot {
                name: "fixture".into(),
                transport: "stdio".into(),
                source: "project".into(),
            }],
            tools: vec![McpToolSnapshot {
                name: "mcp__fixture__ping".into(),
                tokens: 12,
            }],
            warnings: vec!["one server skipped".into()],
            error: None,
        };
        let json = serde_json::to_value(&snapshot).expect("writes");
        assert_eq!(json["servers"][0]["transport"], "stdio");
        assert_eq!(json["tools"][0]["tokens"], 12);
        assert!(json.get("error").is_none());
        let back: McpStatusSnapshot = serde_json::from_value(json).expect("reads");
        assert_eq!(back, snapshot);
    }
}
