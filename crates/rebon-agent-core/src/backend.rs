//! `AgentBackend` — which agent actually runs a turn.
//!
//! [`PromptExecutor`] answers "run this turn". It says nothing about
//! *who* runs it, because for a long time there was only one answer:
//! the local engine. Pointing a Rebon session at a third-party agent
//! CLI adds a second answer with a different shape — a child process
//! with sessions of its own, that must be started before it can be
//! prompted and told to stop out of band.
//!
//! [`AgentBackend`] is the interface both answers fit:
//!
//! - **Session lifecycle.** [`AgentBackend::start_session`] runs
//!   before the first prompt of a session. The local backend has
//!   nothing to start — the Rebon session *is* the session — so it
//!   reports [`SessionResumeMode::Reused`]. A remote agent uses this
//!   to reuse a live connection, replay a stored session, or mint a
//!   new one, in that order of preference.
//! - **The turn itself.** [`AgentBackend::prompt`] is
//!   [`PromptExecutor::execute`] — same request, same outcome, so the
//!   ACP server and the TUI drive either backend unchanged.
//! - **Out-of-band cancel.** The host fires its own
//!   [`crate::PromptCancel`] either way; [`AgentBackend::cancel`] is the
//!   extra hop needed when the work is happening in somebody else's
//!   process.
//! - **Honest capabilities.** [`AgentCapabilities`] is how a backend
//!   admits what it cannot do, so the UI can say so rather than
//!   letting a feature silently mean nothing. The load-bearing one is
//!   [`AgentCapabilities::writes_through_host_fs`]: an agent that
//!   writes to disk directly leaves no file-history snapshot behind,
//!   which makes `/rewind` a promise Rebon cannot keep.

use async_trait::async_trait;

use rebon_proto::types::McpServerConfig;

use crate::prompt_executor::{PromptExecutor, PromptExecutorError, PromptOutcome, PromptRequest};

/// Which family a backend belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentBackendKind {
    /// Rebon's own engine: model client, tool dispatch, transcript.
    Local,
    /// A third-party agent CLI driven over the Agent Client Protocol.
    Acp,
    /// An agent loop running inside Rebon's embedded kernel runtime —
    /// in-process, no child process and no wire protocol.
    Kernel,
}

impl AgentBackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Acp => "acp",
            Self::Kernel => "kernel",
        }
    }
}

/// What a backend can and cannot do.
///
/// Every field is a promise the host makes to the user somewhere in
/// the UI. A backend that answers `false` is not broken — it is
/// telling the host to stop advertising something it cannot deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCapabilities {
    /// File writes go through Rebon's tool pipeline, so pre-write
    /// snapshots exist and `/rewind` can undo them.
    ///
    /// An agent that writes to disk itself must set this to `false`.
    /// Rebon cannot rewind what it never saw, and a rewind UI that
    /// silently restores nothing is worse than one that says it is
    /// unavailable.
    pub writes_through_host_fs: bool,
    /// Tool permission prompts reach the host, so the user approves
    /// them in Rebon's UI under Rebon's permission modes.
    pub host_permission_prompts: bool,
    /// A session survives the turn it was created in and can be
    /// resumed later.
    pub resumable_sessions: bool,
    /// Per-turn token usage is reported back in [`PromptOutcome`].
    ///
    /// A remote agent generally cannot report the prompt-cache hit
    /// rate of a provider it talks to privately, so consumers should
    /// treat a `false` here as "usage numbers will be zero", not as
    /// "this agent used no tokens".
    pub reports_usage: bool,
}

impl AgentCapabilities {
    /// What the local engine can do: everything.
    pub const LOCAL: Self = Self {
        writes_through_host_fs: true,
        host_permission_prompts: true,
        resumable_sessions: true,
        reports_usage: true,
    };

    /// The floor a backend is assumed to sit at before it says
    /// otherwise: it can run a turn, and nothing more is promised.
    pub const MINIMAL: Self = Self {
        writes_through_host_fs: false,
        host_permission_prompts: false,
        resumable_sessions: false,
        reports_usage: false,
    };

    /// Whether `/rewind` can honestly be offered for this backend.
    pub fn supports_rewind(&self) -> bool {
        self.writes_through_host_fs
    }
}

impl Default for AgentCapabilities {
    fn default() -> Self {
        Self::MINIMAL
    }
}

/// What the host knows about a session before the first prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionSpec {
    /// Rebon's session id. A remote backend keeps its own id and
    /// correlates the two; it must not assume the host id is
    /// meaningful on its side.
    pub session_id: String,
    /// Resolved working directory for the session.
    pub cwd: String,
    /// MCP servers the session was activated with.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Extra directories the session is allowed to reach.
    pub additional_working_directories: Vec<String>,
    /// The backend's own session id from a previous run, when the host
    /// persisted one.
    ///
    /// This is what makes resume possible across a Rebon restart: the
    /// backend's id means nothing to the host, but handing it back is
    /// the difference between the agent replaying its stored session
    /// and starting from nothing. `None` for a session the host has
    /// never seen, and always `None` for the local backend, which has
    /// no second id to remember.
    pub resume_session_id: Option<String>,
}

impl AgentSessionSpec {
    pub fn new(session_id: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            cwd: cwd.into(),
            mcp_servers: Vec::new(),
            additional_working_directories: Vec::new(),
            resume_session_id: None,
        }
    }

    /// Offer a previously stored backend session id to resume from.
    pub fn with_resume_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.resume_session_id = Some(session_id.into());
        self
    }
}

/// How a backend satisfied [`AgentBackend::start_session`].
///
/// Ordered by how much of the previous session survived, best first.
/// For a remote agent this is the metric that matters: a reused live
/// session keeps the other side's prompt cache warm, a loaded one
/// keeps the history but pays for the context again, and a new one
/// starts from nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionResumeMode {
    /// A live session was still open and was reused as-is.
    Reused,
    /// A stored session was replayed into the agent.
    Loaded,
    /// Nothing could be recovered; a fresh session was created.
    Created,
}

impl SessionResumeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reused => "reused",
            Self::Loaded => "loaded",
            Self::Created => "created",
        }
    }

    /// Whether this start preserved the agent-side session state.
    pub fn preserved_session(self) -> bool {
        matches!(self, Self::Reused | Self::Loaded)
    }
}

/// Result of starting (or recovering) a session on a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionStart {
    /// The backend's own id for the session. For the local backend
    /// this is the host session id unchanged.
    pub session_id: String,
    pub mode: SessionResumeMode,
}

impl AgentSessionStart {
    pub fn reused(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            mode: SessionResumeMode::Reused,
        }
    }
}

/// Failures that belong to the backend rather than to a turn.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AgentBackendError {
    /// The agent process could not be started or has died.
    #[error("agent backend unavailable: {0}")]
    Unavailable(String),
    /// The backend rejected the session (bad cwd, unsupported
    /// capability negotiation, protocol mismatch).
    #[error("agent backend rejected the session: {0}")]
    SessionRejected(String),
    /// The named session is not known to this backend.
    #[error("unknown session: {0}")]
    UnknownSession(String),
    /// Transport or protocol failure while talking to the agent.
    #[error("agent backend transport failed: {0}")]
    Transport(String),
    /// The backend has no way to deliver this operation — steering on
    /// an agent that never advertised it, for instance. Callers treat
    /// this as "fall back", not as a failure worth surfacing.
    #[error("agent backend does not support this operation: {0}")]
    Unsupported(String),
}

/// What became of a steering attempt — a message pushed into a turn
/// that is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerOutcome {
    /// Delivered into the running turn; the turn's ongoing output will
    /// reflect it.
    Injected,
    /// The turn had already finished. The backend has cleaned up
    /// whatever the agent started on its own; the caller should send
    /// the message as an ordinary new prompt instead.
    TurnAlreadyOver,
}

/// The agent behind a Rebon session.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Which family this backend belongs to.
    fn kind(&self) -> AgentBackendKind;

    /// Stable identifier shown in the UI and written to logs.
    fn name(&self) -> &str;

    /// What this backend can actually deliver. See
    /// [`AgentCapabilities`] — answering `true` here is a promise the
    /// UI will make on the backend's behalf.
    fn capabilities(&self) -> AgentCapabilities;

    /// Prepare a session for prompting.
    ///
    /// Called before the first [`Self::prompt`] of a session, and
    /// again after anything that may have dropped the agent-side
    /// session. Implementations should prefer the cheapest option
    /// that preserves state: reuse, then load, then create.
    async fn start_session(
        &self,
        spec: AgentSessionSpec,
    ) -> Result<AgentSessionStart, AgentBackendError>;

    /// Run one turn. Same contract as [`PromptExecutor::execute`].
    async fn prompt(&self, request: PromptRequest) -> Result<PromptOutcome, PromptExecutorError>;

    /// Forward a cancel to the agent.
    ///
    /// The host has already fired the turn's [`crate::PromptCancel`], which
    /// is enough for work happening in this process. Backends running
    /// the turn elsewhere use this to tell that side to stop too.
    async fn cancel(&self, _session_id: &str) -> Result<(), AgentBackendError> {
        Ok(())
    }

    /// Push a user message into the turn currently running on
    /// `session_id`, instead of queueing it behind the turn.
    ///
    /// `user_message_uuid` is the host's id for the message, so the
    /// record the backend writes and the row the UI shows are the same
    /// message. The default refuses: a backend that never said it can
    /// steer cannot, and the caller's fallback (queue until the turn
    /// ends) is always correct.
    async fn steer(
        &self,
        _session_id: &str,
        _blocks: Vec<rebon_types::ContentBlock>,
        _user_message_uuid: &str,
    ) -> Result<SteerOutcome, AgentBackendError> {
        Err(AgentBackendError::Unsupported(
            "this backend cannot steer a running turn".to_string(),
        ))
    }

    /// Release whatever the backend holds for a session.
    ///
    /// Best-effort: a backend that has nothing to release, or whose
    /// agent already exited, reports success.
    async fn close_session(&self, _session_id: &str) -> Result<(), AgentBackendError> {
        Ok(())
    }

    /// Offer an agent-side id persisted by the host before the next session start.
    /// Backends without external session identities have nothing to remember.
    fn remember_agent_session(&self, _host_session_id: &str, _agent_session_id: &str) {}

    /// Servers injected into every session by its host, if any.
    fn injected_mcp_servers(&self) -> &[McpServerConfig] {
        &[]
    }

    /// Stop an owned connection before its registry builds a replacement.
    /// In-process backends without a connection need no shutdown action.
    async fn shutdown(&self) {}
}

/// The local engine as an [`AgentBackend`].
///
/// A pure adapter: it forwards [`Self::prompt`] to the injected
/// [`PromptExecutor`] and answers the session questions the way the
/// local leg has always implicitly answered them — there is nothing
/// to start, because the Rebon session already is the session, and
/// nothing to cancel out of band, because the loop being cancelled is
/// running right here.
pub struct LocalAgentBackend {
    executor: std::sync::Arc<dyn PromptExecutor>,
    name: String,
}

impl LocalAgentBackend {
    pub fn new(executor: std::sync::Arc<dyn PromptExecutor>) -> Self {
        Self {
            executor,
            name: "local".to_string(),
        }
    }

    /// Override the name shown in the UI (an agent definition that
    /// runs locally may want its own label).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// The executor this backend forwards to.
    pub fn executor(&self) -> &std::sync::Arc<dyn PromptExecutor> {
        &self.executor
    }
}

impl std::fmt::Debug for LocalAgentBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalAgentBackend")
            .field("name", &self.name)
            .finish()
    }
}

#[async_trait]
impl AgentBackend for LocalAgentBackend {
    fn kind(&self) -> AgentBackendKind {
        AgentBackendKind::Local
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::LOCAL
    }

    async fn start_session(
        &self,
        spec: AgentSessionSpec,
    ) -> Result<AgentSessionStart, AgentBackendError> {
        // Nothing to start: the engine reads the session's transcript
        // on every turn, so the session it would "create" already
        // exists by the time anyone asks.
        Ok(AgentSessionStart::reused(spec.session_id))
    }

    async fn prompt(&self, request: PromptRequest) -> Result<PromptOutcome, PromptExecutorError> {
        self.executor.execute(request).await
    }
}

/// A [`PromptExecutor`] view of any [`AgentBackend`].
///
/// Lets a backend be dropped into the ACP server and the TUI, both of
/// which take an `Arc<dyn PromptExecutor>`, without either learning
/// what a backend is.
pub struct BackendPromptExecutor {
    backend: std::sync::Arc<dyn AgentBackend>,
}

impl BackendPromptExecutor {
    pub fn new(backend: std::sync::Arc<dyn AgentBackend>) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> &std::sync::Arc<dyn AgentBackend> {
        &self.backend
    }
}

#[async_trait]
impl PromptExecutor for BackendPromptExecutor {
    async fn execute(&self, request: PromptRequest) -> Result<PromptOutcome, PromptExecutorError> {
        self.backend.prompt(request).await
    }
}

/// The backend a session is currently running on, swappable in place.
///
/// Everything that drives turns — the ACP server handler, the TUI's
/// submit path — holds an `Arc<dyn PromptExecutor>` it acquired at
/// startup. Pointing the session at a different agent therefore cannot
/// mean handing those callers a new executor; it has to mean changing
/// what the one they already hold delegates to. That is this type: one
/// stable `PromptExecutor`, one swappable backend behind it.
///
/// A turn already in flight keeps running on the backend it started on,
/// because [`Self::execute`] resolves the backend once, before
/// awaiting. Switching mid-turn affects the next turn, not the one on
/// screen — which is the only coherent answer, since the other agent
/// has no idea what this turn was doing.
pub struct AgentBackendSwitch {
    current: std::sync::RwLock<std::sync::Arc<dyn AgentBackend>>,
}

impl AgentBackendSwitch {
    pub fn new(initial: std::sync::Arc<dyn AgentBackend>) -> Self {
        Self {
            current: std::sync::RwLock::new(initial),
        }
    }

    /// The backend turns are currently going to.
    pub fn current(&self) -> std::sync::Arc<dyn AgentBackend> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Point the session at `next`, returning the backend it replaced
    /// so the caller can close out whatever that one was holding.
    pub fn switch(
        &self,
        next: std::sync::Arc<dyn AgentBackend>,
    ) -> std::sync::Arc<dyn AgentBackend> {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::replace(&mut *current, next)
    }

    /// What the current backend can deliver, for a UI deciding which
    /// features to offer.
    pub fn capabilities(&self) -> AgentCapabilities {
        self.current().capabilities()
    }

    /// Steer the turn running on whichever backend is current. Same
    /// resolution rule as [`Self::execute`]: resolved once, before the
    /// await, so the message goes to the backend the turn started on.
    pub async fn steer(
        &self,
        session_id: &str,
        blocks: Vec<rebon_types::ContentBlock>,
        user_message_uuid: &str,
    ) -> Result<SteerOutcome, AgentBackendError> {
        self.current()
            .steer(session_id, blocks, user_message_uuid)
            .await
    }
}

impl std::fmt::Debug for AgentBackendSwitch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let current = self.current();
        f.debug_struct("AgentBackendSwitch")
            .field("current", &current.name())
            .field("kind", &current.kind().as_str())
            .finish()
    }
}

#[async_trait]
impl PromptExecutor for AgentBackendSwitch {
    async fn execute(&self, request: PromptRequest) -> Result<PromptOutcome, PromptExecutorError> {
        // Resolved before the await: the lock is never held across it,
        // and the turn stays on the backend it started on.
        self.current().prompt(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_executor::{PromptCancel, StubPromptExecutor};
    use std::sync::Arc;

    fn request(session_id: &str) -> PromptRequest {
        PromptRequest {
            user_prompt: None,
            effort_is_session_default: false,
            session_id: session_id.into(),
            cwd: "/tmp".into(),
            prompt: Vec::new(),
            mcp_servers: Vec::new(),
            update_publisher: None,
            permission_publisher: None,
            cancel: PromptCancel::new(),
            thinking_budget: None,
            max_tokens: None,
            reasoning_effort_ordinal: None,
            additional_working_directories: Vec::new(),
            coordinator_mode: None,
            coordinator_report_paths: Vec::new(),
            user_message_uuid: None,
            background_agent_system: None,
            background_agent_tool_filter: None,
            execution_policy: None,
            replay_requests: Vec::new(),
            skill_invocations: Vec::new(),
        }
    }

    #[tokio::test]
    async fn local_backend_forwards_the_turn_to_its_executor() {
        let backend = LocalAgentBackend::new(Arc::new(StubPromptExecutor));
        let outcome = backend.prompt(request("sess-1")).await.unwrap();
        assert_eq!(outcome.stop_reason, rebon_types::StopReason::EndTurn);
        assert_eq!(backend.kind(), AgentBackendKind::Local);
        assert_eq!(backend.name(), "local");
    }

    #[tokio::test]
    async fn local_backend_has_nothing_to_start_and_nothing_to_cancel() {
        let backend = LocalAgentBackend::new(Arc::new(StubPromptExecutor));
        let start = backend
            .start_session(AgentSessionSpec::new("sess-1", "/tmp"))
            .await
            .unwrap();
        assert_eq!(start.mode, SessionResumeMode::Reused);
        assert_eq!(start.session_id, "sess-1");
        // The host's own PromptCancel already stopped the loop.
        backend.cancel("sess-1").await.unwrap();
        backend.close_session("sess-1").await.unwrap();
    }

    #[test]
    fn local_capabilities_keep_every_host_promise() {
        let caps = LocalAgentBackend::new(Arc::new(StubPromptExecutor)).capabilities();
        assert!(caps.writes_through_host_fs);
        assert!(caps.supports_rewind());
        assert!(caps.host_permission_prompts);
        assert!(caps.resumable_sessions);
        assert!(caps.reports_usage);
    }

    #[test]
    fn a_backend_that_writes_its_own_files_cannot_offer_rewind() {
        let caps = AgentCapabilities {
            writes_through_host_fs: false,
            ..AgentCapabilities::LOCAL
        };
        assert!(!caps.supports_rewind());
        assert_eq!(AgentCapabilities::default(), AgentCapabilities::MINIMAL);
    }

    #[tokio::test]
    async fn a_backend_can_stand_in_for_a_prompt_executor() {
        let backend: Arc<dyn AgentBackend> =
            Arc::new(LocalAgentBackend::new(Arc::new(StubPromptExecutor)).with_name("renamed"));
        assert_eq!(backend.name(), "renamed");
        let executor: Arc<dyn PromptExecutor> = Arc::new(BackendPromptExecutor::new(backend));
        let outcome = executor.execute(request("sess-2")).await.unwrap();
        assert_eq!(outcome.stop_reason, rebon_types::StopReason::EndTurn);
    }

    /// A backend that reports a different name and refuses to run, so
    /// a test can tell which one a turn actually reached.
    struct NamedBackend(&'static str);

    #[async_trait]
    impl AgentBackend for NamedBackend {
        fn kind(&self) -> AgentBackendKind {
            AgentBackendKind::Acp
        }
        fn name(&self) -> &str {
            self.0
        }
        fn capabilities(&self) -> AgentCapabilities {
            AgentCapabilities::MINIMAL
        }
        async fn start_session(
            &self,
            spec: AgentSessionSpec,
        ) -> Result<AgentSessionStart, AgentBackendError> {
            Ok(AgentSessionStart::reused(spec.session_id))
        }
        async fn prompt(
            &self,
            _request: PromptRequest,
        ) -> Result<PromptOutcome, PromptExecutorError> {
            Err(PromptExecutorError::Execution(self.0.to_string()))
        }
    }

    #[tokio::test]
    async fn coverage_matrix_acp_remains_an_agent_backend_prompt_executor() {
        // Capability seats route tools and model providers only. ACP keeps its
        // established orchestration boundary: AgentBackend adapted to the
        // host-facing PromptExecutor contract.
        let backend: Arc<dyn AgentBackend> = Arc::new(NamedBackend("acp-backend"));
        assert_eq!(backend.kind(), AgentBackendKind::Acp);
        let executor: Arc<dyn PromptExecutor> = Arc::new(BackendPromptExecutor::new(backend));
        let err = executor
            .execute(request("acp-session"))
            .await
            .expect_err("named ACP backend error crosses the PromptExecutor boundary");
        assert!(err.to_string().contains("acp-backend"), "{err}");
    }

    #[tokio::test]
    async fn switching_changes_where_the_next_turn_goes() {
        let local: Arc<dyn AgentBackend> =
            Arc::new(LocalAgentBackend::new(Arc::new(StubPromptExecutor)));
        let switch = AgentBackendSwitch::new(local);
        // The executor handed to the handler is acquired once and never
        // replaced — the swap has to be visible through it.
        let executor: Arc<dyn PromptExecutor> = Arc::new(switch);
        let outcome = executor.execute(request("sess-1")).await.unwrap();
        assert_eq!(outcome.stop_reason, rebon_types::StopReason::EndTurn);
    }

    #[tokio::test]
    async fn the_swap_returns_the_backend_it_replaced() {
        let switch = AgentBackendSwitch::new(Arc::new(NamedBackend("first")));
        assert_eq!(switch.current().name(), "first");
        assert!(!switch.capabilities().supports_rewind());

        let previous = switch.switch(Arc::new(LocalAgentBackend::new(Arc::new(
            StubPromptExecutor,
        ))));
        assert_eq!(
            previous.name(),
            "first",
            "the caller needs the old backend to close out what it held"
        );
        assert_eq!(switch.current().name(), "local");
        assert!(
            switch.capabilities().supports_rewind(),
            "capabilities follow the swap, so the UI stops lying about rewind"
        );
    }

    #[tokio::test]
    async fn turns_reach_whichever_backend_is_current() {
        let switch = Arc::new(AgentBackendSwitch::new(Arc::new(NamedBackend("alpha"))));
        let executor: Arc<dyn PromptExecutor> = switch.clone();

        let err = executor.execute(request("sess-1")).await.unwrap_err();
        assert!(err.to_string().contains("alpha"), "{err}");

        switch.switch(Arc::new(NamedBackend("beta")));
        let err = executor.execute(request("sess-1")).await.unwrap_err();
        assert!(
            err.to_string().contains("beta"),
            "the same executor must now reach the new backend: {err}"
        );
    }

    #[test]
    fn resume_modes_report_whether_state_survived() {
        assert!(SessionResumeMode::Reused.preserved_session());
        assert!(SessionResumeMode::Loaded.preserved_session());
        assert!(!SessionResumeMode::Created.preserved_session());
        assert_eq!(SessionResumeMode::Created.as_str(), "created");
        assert_eq!(AgentBackendKind::Acp.as_str(), "acp");
    }
}
