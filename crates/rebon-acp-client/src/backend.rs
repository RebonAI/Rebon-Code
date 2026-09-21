//! A third-party agent CLI as a Rebon [`AgentBackend`].
//!
//! # Two session ids, never confused
//!
//! Rebon names a session; the agent names it something else. The
//! [`SessionRouter`] holds the mapping and rewrites every
//! `session/update` the agent emits before it reaches the host — an
//! update carrying the agent's id would address a session the host has
//! never heard of, and the TUI would drop it on the floor.
//!
//! # Three-tier resume
//!
//! [`AcpAgentBackend::start_session`] tries, in order:
//!
//! 1. **Reuse.** The connection is live and this host session already
//!    has an agent session. Nothing is sent — the agent's context and
//!    whatever prompt cache sits behind it stay warm.
//! 2. **Load.** We know the agent session id from a previous run and
//!    the agent advertised `loadSession`. History survives; the agent
//!    pays to re-read it.
//! 3. **Create.** Nothing survived. `session/new`.
//!
//! Which one happened is reported as [`SessionResumeMode`] and is the
//! number worth watching for this backend. Prompt-cache hit rate is
//! not observable here — the agent talks to its provider privately —
//! so how often sessions are reused is the closest available proxy
//! for "are we making the other side redo work".

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use rebon_agent_core::backend::{
    AgentBackend, AgentBackendError, AgentBackendKind, AgentCapabilities, AgentSessionSpec,
    AgentSessionStart, SessionResumeMode,
};
use rebon_agent_core::prompt_executor::{PromptExecutorError, PromptOutcome, PromptRequest};
use rebon_agent_core::publisher::{ChannelPermissionRequestPublisher, SessionUpdatePublisher};
use rebon_proto::types::{
    McpServerConfig, ReadTextFileParams, RequestPermissionParams, RequestPermissionResult,
    SessionUpdateParams, WriteTextFileParams,
};

use crate::client::{default_client_capabilities, AcpClient, ClientError};
use crate::connection::ClientDelegate;
use crate::connector::{AgentConnector, ProcessConnector};
use crate::host_fs::{DirectHostFs, HostFs};
use crate::journal::{NoopTurnJournal, TurnJournal};
use crate::process::AgentCommand;

/// Everything the host decides about an ACP-backed agent.
///
/// Cloneable so a registry can keep it after building a backend: reconnecting
/// an agent means building a second backend from the same description, and a
/// configuration consumed by the first build leaves nothing to reconnect with.
/// Every field is cheap to clone — the two `Arc<dyn …>` seams are shared, not
/// duplicated, which is correct: a reconnected agent writes to the same session
/// and the same filesystem as the one it replaces.
#[derive(Clone)]
pub struct AcpBackendConfig {
    /// Name shown in the UI and written to logs.
    pub name: String,
    /// How to start the agent.
    pub command: AgentCommand,
    /// Where the agent's file access lands. Also decides what
    /// [`AcpAgentBackend::snapshots_routed_writes`] reports — whether a
    /// write the agent routes through the host keeps its pre-image. It
    /// does not decide `writes_through_host_fs`, which is always
    /// `false` on this leg.
    pub host_fs: Arc<dyn HostFs>,
    /// Where the agent's turns are written down.
    ///
    /// Defaults to recording nothing, which is the right default for a
    /// caller that has no session on disk — and the wrong one for a
    /// Rebon session, where a turn nobody recorded is a turn `--resume`
    /// and `/rewind` cannot see.
    pub journal: Arc<dyn TurnJournal>,
    /// MCP servers to hand the agent when a session is created.
    pub mcp_servers: Vec<McpServerConfig>,
    /// MCP servers the *host* insists on — Rebon's own injected tools.
    ///
    /// Kept apart from [`Self::mcp_servers`] because that field loses
    /// to a non-empty spec (the caller's list replaces the config's
    /// wholesale). These are appended to whichever list wins, deduped
    /// by name with the injection winning, so no wiring combination
    /// can silently drop the tools that keep `/rewind` meaningful.
    pub injected_mcp_servers: Vec<McpServerConfig>,
    /// `_meta` for `session/new` and `session/load` — adapter-specific
    /// options the spec has no field for. What makes the injection
    /// stick on some agents: `claude-agent-acp` reads
    /// `claudeCode.options` here, and `disallowedTools` in it is how
    /// the agent's own edit tools get out of the way.
    pub session_meta: Option<std::collections::HashMap<String, serde_json::Value>>,
    /// Where the conversation so far comes from when a session starts
    /// cold on this agent. See [`HandoffProvider`].
    pub handoff_provider: Option<Arc<dyn HandoffProvider>>,
    /// The directory this agent's sessions open in, when it is not the
    /// host session's own.
    ///
    /// Exists for agents that do not share a filesystem with the host
    /// — a remote agent reached over ssh being the case that needs it.
    /// There the host's cwd names a directory on the wrong machine,
    /// and `session/new` has to carry the remote project path instead.
    /// `None` keeps the host's cwd, which is right for every agent
    /// running beside it.
    pub workspace_cwd: Option<String>,
}

impl AcpBackendConfig {
    pub fn new(name: impl Into<String>, command: AgentCommand) -> Self {
        Self {
            name: name.into(),
            command,
            host_fs: Arc::new(DirectHostFs),
            journal: Arc::new(NoopTurnJournal),
            mcp_servers: Vec::new(),
            injected_mcp_servers: Vec::new(),
            session_meta: None,
            handoff_provider: None,
            workspace_cwd: None,
        }
    }

    pub fn with_host_fs(mut self, host_fs: Arc<dyn HostFs>) -> Self {
        self.host_fs = host_fs;
        self
    }

    /// Record this backend's turns into a Rebon session.
    pub fn with_journal(mut self, journal: Arc<dyn TurnJournal>) -> Self {
        self.journal = journal;
        self
    }

    pub fn with_mcp_servers(mut self, mcp_servers: Vec<McpServerConfig>) -> Self {
        self.mcp_servers = mcp_servers;
        self
    }

    /// MCP servers every session on this backend gets, whatever else
    /// the spec or config asked for.
    pub fn with_injected_mcp_servers(mut self, servers: Vec<McpServerConfig>) -> Self {
        self.injected_mcp_servers = servers;
        self
    }

    /// `_meta` sent with `session/new` and `session/load`.
    pub fn with_session_meta(
        mut self,
        meta: Option<std::collections::HashMap<String, serde_json::Value>>,
    ) -> Self {
        self.session_meta = meta;
        self
    }

    /// Catch a cold agent session up on the conversation so far — see
    /// [`HandoffProvider`].
    pub fn with_handoff_provider(mut self, provider: Arc<dyn HandoffProvider>) -> Self {
        self.handoff_provider = Some(provider);
        self
    }

    /// Open this agent's sessions in a directory of its own — see
    /// [`Self::workspace_cwd`].
    pub fn with_workspace_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.workspace_cwd = Some(cwd.into());
        self
    }
}

/// Supplies the conversation-so-far when an agent starts cold.
///
/// An agent session that is genuinely new — the user switched agents,
/// or the old session could not be loaded — knows nothing about the
/// conversation the user is looking at. Left alone it answers the next
/// message with no idea what came before. The host implements this to
/// hand over a readable digest of the transcript, which is prepended
/// to that first prompt only.
///
/// Returning `None` means "nothing worth handing over" (an empty
/// session, or a host that would rather not).
pub trait HandoffProvider: Send + Sync {
    fn handoff_text(&self, host_session_id: &str) -> Option<String>;
}

/// One host session's view of the agent.
#[derive(Default)]
struct SessionEntry {
    /// The agent's id for this session on the *current* connection.
    /// Cleared when the connection dies, because an id from a dead
    /// process addresses nothing.
    agent_session_id: Option<String>,
    /// The last agent id we saw, kept across reconnects.
    ///
    /// This is what `session/load` asks for. Without it a restarted
    /// agent could only ever be given a fresh session, which throws
    /// away the conversation the user is still looking at.
    last_agent_session_id: Option<String>,
    /// Where updates go while a turn is running.
    update_publisher: Option<Arc<dyn SessionUpdatePublisher>>,
    /// Where permission requests go while a turn is running.
    permission_publisher: Option<ChannelPermissionRequestPublisher>,
    /// The agent session a conversation handoff was already prepended
    /// to. Keyed by agent session id rather than a flag, so a session
    /// that later starts cold on the same agent gets caught up again.
    handoff_done_for: Option<String>,
}

/// The client side of the conversation, plus the id mapping.
///
/// Lives behind an `Arc` because the connection's read loop calls into
/// it from its own task, while the backend mutates it from the turn's.
pub struct SessionRouter {
    /// Host session id → what we know about it.
    sessions: Mutex<HashMap<String, SessionEntry>>,
    /// Agent session id → host session id. The reverse direction, for
    /// rewriting inbound updates.
    agent_to_host: Mutex<HashMap<String, String>>,
    host_fs: Arc<dyn HostFs>,
    journal: Arc<dyn TurnJournal>,
}

impl SessionRouter {
    fn new(host_fs: Arc<dyn HostFs>, journal: Arc<dyn TurnJournal>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            agent_to_host: Mutex::new(HashMap::new()),
            host_fs,
            journal,
        }
    }

    fn bind(&self, host_session_id: &str, agent_session_id: &str) {
        let previous = {
            let mut sessions = lock(&self.sessions);
            let entry = sessions.entry(host_session_id.to_string()).or_default();
            entry.last_agent_session_id = Some(agent_session_id.to_string());
            entry.agent_session_id.replace(agent_session_id.to_string())
        };
        let mut reverse = lock(&self.agent_to_host);
        // A rebind (agent restarted, new session minted) must retire
        // the old mapping, or a late update addressed to the dead
        // agent session would still resolve and replay into the live
        // one.
        if let Some(previous) = previous {
            if previous != agent_session_id {
                reverse.remove(&previous);
            }
        }
        reverse.insert(agent_session_id.to_string(), host_session_id.to_string());
    }

    fn agent_session_id(&self, host_session_id: &str) -> Option<String> {
        lock(&self.sessions)
            .get(host_session_id)
            .and_then(|entry| entry.agent_session_id.clone())
    }

    /// Seed the id a previous *run* left behind, without claiming there
    /// is a live session on the current connection.
    ///
    /// Only `last_agent_session_id` is set: the agent has not been
    /// asked about this id yet, so treating it as live would let
    /// `start_session` report a reuse that never happened and skip the
    /// `session/load` that actually restores the conversation.
    fn remember(&self, host_session_id: &str, agent_session_id: &str) {
        let mut sessions = lock(&self.sessions);
        let entry = sessions.entry(host_session_id.to_string()).or_default();
        entry.last_agent_session_id = Some(agent_session_id.to_string());
    }

    fn remembered_agent_session_id(&self, host_session_id: &str) -> Option<String> {
        lock(&self.sessions)
            .get(host_session_id)
            .and_then(|entry| entry.last_agent_session_id.clone())
    }

    /// The id `session/load` should ask for: whatever the host handed
    /// us on this call, else the last one we saw ourselves.
    fn resumable_agent_session_id(
        &self,
        host_session_id: &str,
        offered: Option<&str>,
    ) -> Option<String> {
        if let Some(offered) = offered {
            return Some(offered.to_string());
        }
        lock(&self.sessions)
            .get(host_session_id)
            .and_then(|entry| entry.last_agent_session_id.clone())
    }

    fn host_session_id(&self, agent_session_id: &str) -> Option<String> {
        lock(&self.agent_to_host).get(agent_session_id).cloned()
    }

    fn forget(&self, host_session_id: &str) {
        let agent_session_id = {
            let mut sessions = lock(&self.sessions);
            sessions
                .remove(host_session_id)
                .and_then(|entry| entry.agent_session_id)
        };
        if let Some(agent_session_id) = agent_session_id {
            lock(&self.agent_to_host).remove(&agent_session_id);
        }
    }

    /// Drop every agent-side id while keeping the host sessions.
    ///
    /// Called when the connection dies: the ids belonged to a process
    /// that no longer exists, so reusing one would address a session
    /// on a dead agent. The host sessions themselves are still real.
    fn invalidate_agent_sessions(&self) {
        for entry in lock(&self.sessions).values_mut() {
            entry.agent_session_id = None;
        }
        lock(&self.agent_to_host).clear();
    }

    /// Route this turn's updates and permission prompts for as long as
    /// the returned guard lives.
    fn begin_turn(
        self: &Arc<Self>,
        host_session_id: &str,
        update_publisher: Option<Arc<dyn SessionUpdatePublisher>>,
        permission_publisher: Option<ChannelPermissionRequestPublisher>,
    ) -> TurnGuard {
        {
            let mut sessions = lock(&self.sessions);
            let entry = sessions.entry(host_session_id.to_string()).or_default();
            entry.update_publisher = update_publisher;
            entry.permission_publisher = permission_publisher;
        }
        TurnGuard {
            router: self.clone(),
            host_session_id: host_session_id.to_string(),
        }
    }

    fn update_publisher(&self, host_session_id: &str) -> Option<Arc<dyn SessionUpdatePublisher>> {
        lock(&self.sessions)
            .get(host_session_id)
            .and_then(|entry| entry.update_publisher.clone())
    }

    fn permission_publisher(
        &self,
        host_session_id: &str,
    ) -> Option<ChannelPermissionRequestPublisher> {
        lock(&self.sessions)
            .get(host_session_id)
            .and_then(|entry| entry.permission_publisher.clone())
    }
}

/// Clears a turn's publishers however the turn ends.
///
/// Without this, a cancelled or failed turn would leave its publishers
/// installed and the next `session/update` from a late-finishing agent
/// would render into a UI that has moved on.
struct TurnGuard {
    router: Arc<SessionRouter>,
    host_session_id: String,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        let mut sessions = lock(&self.router.sessions);
        if let Some(entry) = sessions.get_mut(&self.host_session_id) {
            entry.update_publisher = None;
            entry.permission_publisher = None;
        }
    }
}

#[async_trait]
impl ClientDelegate for SessionRouter {
    async fn session_update(&self, mut params: SessionUpdateParams) {
        let Some(host_session_id) = self.host_session_id(&params.session_id) else {
            tracing::debug!(
                agent_session = %params.session_id,
                "acp-client: update for an unmapped session"
            );
            return;
        };
        // Recorded before it is rendered: what the UI shows is
        // transient, what the journal keeps is the session's history.
        self.journal.record_update(&host_session_id, &params.update);
        let Some(publisher) = self.update_publisher(&host_session_id) else {
            // Between turns there is nobody to render into. Dropping
            // is correct — the alternative is buffering updates that
            // will be replayed out of context.
            tracing::debug!(
                host_session = %host_session_id,
                "acp-client: update outside a turn"
            );
            return;
        };
        // Speak the host's id, not the agent's.
        params.session_id = host_session_id;
        publisher.publish_owned(params).await;
    }

    async fn request_permission(
        &self,
        mut params: RequestPermissionParams,
    ) -> anyhow::Result<RequestPermissionResult> {
        let host_session_id = self
            .host_session_id(&params.session_id)
            .ok_or_else(|| anyhow::anyhow!("permission request for an unmapped session"))?;
        let publisher = self
            .permission_publisher(&host_session_id)
            .ok_or_else(|| anyhow::anyhow!("permission request outside a turn"))?;
        params.session_id = host_session_id;
        publisher.request_permission(params).await
    }

    async fn read_text_file(&self, params: ReadTextFileParams) -> anyhow::Result<String> {
        let host_session_id = self
            .host_session_id(&params.session_id)
            .unwrap_or_else(|| params.session_id.clone());
        self.host_fs
            .read_text_file(
                &host_session_id,
                &PathBuf::from(&params.path),
                params.line,
                params.limit,
            )
            .await
    }

    async fn write_text_file(&self, params: WriteTextFileParams) -> anyhow::Result<()> {
        let host_session_id = self
            .host_session_id(&params.session_id)
            .unwrap_or_else(|| params.session_id.clone());
        self.host_fs
            .write_text_file(
                &host_session_id,
                &PathBuf::from(&params.path),
                &params.content,
            )
            .await
    }
}

/// Closes a journal turn however the turn ends.
///
/// A turn that panicked, errored, or was cancelled is exactly the turn
/// whose partial work the user most needs to see afterwards — and the
/// one whose file-history arming must be released, or the next turn
/// would snapshot into a boundary that has already passed. So the flush
/// lives in `Drop`, and the stop reason is whatever the turn managed to
/// report before getting there.
struct JournalTurn<'a> {
    journal: &'a Arc<dyn TurnJournal>,
    host_session_id: String,
    turn_id: Option<String>,
    stop_reason: Option<rebon_types::StopReason>,
}

impl<'a> JournalTurn<'a> {
    fn open(
        journal: &'a Arc<dyn TurnJournal>,
        host_session_id: &str,
        turn_id: Option<String>,
    ) -> Self {
        Self {
            journal,
            host_session_id: host_session_id.to_string(),
            turn_id,
            stop_reason: None,
        }
    }

    fn finish(&mut self, stop_reason: rebon_types::StopReason) {
        self.stop_reason = Some(stop_reason);
    }
}

impl Drop for JournalTurn<'_> {
    fn drop(&mut self) {
        let Some(turn_id) = self.turn_id.take() else {
            return;
        };
        self.journal
            .end_turn(&self.host_session_id, &turn_id, self.stop_reason);
    }
}

/// How this backend's sessions started, by [`SessionResumeMode`].
///
/// Session reuse rate is this leg's efficiency signal; these counters
/// are what make that measurable instead of aspirational. Read them
/// through [`AcpAgentBackend::session_start_counts`].
#[derive(Debug, Default)]
struct SessionStartCounters {
    created: std::sync::atomic::AtomicU64,
    loaded: std::sync::atomic::AtomicU64,
    reused: std::sync::atomic::AtomicU64,
}

impl SessionStartCounters {
    fn record(&self, mode: SessionResumeMode) {
        let counter = match mode {
            SessionResumeMode::Created => &self.created,
            SessionResumeMode::Loaded => &self.loaded,
            SessionResumeMode::Reused => &self.reused,
        };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A third-party agent CLI, driven over ACP.
pub struct AcpAgentBackend {
    name: String,
    connector: Arc<dyn AgentConnector>,
    mcp_servers: Vec<McpServerConfig>,
    injected_mcp_servers: Vec<McpServerConfig>,
    session_meta: Option<std::collections::HashMap<String, serde_json::Value>>,
    handoff_provider: Option<Arc<dyn HandoffProvider>>,
    router: Arc<SessionRouter>,
    journal: Arc<dyn TurnJournal>,
    /// The live agent, if one is connected. Connecting is lazy: an
    /// agent nobody has prompted should not be running.
    client: tokio::sync::Mutex<Option<Arc<AcpClient>>>,
    snapshots_writes: bool,
    session_starts: SessionStartCounters,
    /// See [`AcpBackendConfig::workspace_cwd`].
    workspace_cwd: Option<String>,
}

impl AcpAgentBackend {
    pub fn new(config: AcpBackendConfig) -> Self {
        let connector = Arc::new(ProcessConnector::new(
            config.command,
            default_client_capabilities(),
        ));
        let injected = config.injected_mcp_servers;
        let session_meta = config.session_meta;
        let handoff_provider = config.handoff_provider;
        let workspace_cwd = config.workspace_cwd;
        let mut backend = Self::with_connector(
            config.name,
            connector,
            config.host_fs,
            config.journal,
            config.mcp_servers,
        )
        .with_injected_mcp_servers(injected)
        .with_session_meta(session_meta);
        backend.handoff_provider = handoff_provider;
        backend.workspace_cwd = workspace_cwd;
        backend
    }

    /// Build a backend over some other way of reaching an agent.
    pub fn with_connector(
        name: impl Into<String>,
        connector: Arc<dyn AgentConnector>,
        host_fs: Arc<dyn HostFs>,
        journal: Arc<dyn TurnJournal>,
        mcp_servers: Vec<McpServerConfig>,
    ) -> Self {
        let snapshots_writes = host_fs.snapshots_writes();
        Self {
            name: name.into(),
            connector,
            mcp_servers,
            injected_mcp_servers: Vec::new(),
            session_meta: None,
            handoff_provider: None,
            router: Arc::new(SessionRouter::new(host_fs, journal.clone())),
            journal,
            client: tokio::sync::Mutex::new(None),
            snapshots_writes,
            session_starts: SessionStartCounters::default(),
            workspace_cwd: None,
        }
    }

    /// See [`AcpBackendConfig::with_workspace_cwd`].
    pub fn with_workspace_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.workspace_cwd = Some(cwd.into());
        self
    }

    /// The conversation digest to prepend to this turn, if any.
    ///
    /// Only for a session the agent created fresh: a reused or loaded
    /// session already has the history on its side. Recorded against
    /// the agent session id so it happens once per cold start, not
    /// once per turn.
    fn handoff_for(&self, host_session_id: &str, start: &AgentSessionStart) -> Option<String> {
        if start.mode != SessionResumeMode::Created {
            return None;
        }
        let provider = self.handoff_provider.as_ref()?;
        {
            let sessions = lock(&self.router.sessions);
            if sessions
                .get(host_session_id)
                .and_then(|entry| entry.handoff_done_for.as_deref())
                == Some(start.session_id.as_str())
            {
                return None;
            }
        }
        let text = provider.handoff_text(host_session_id)?;
        let mut sessions = lock(&self.router.sessions);
        let entry = sessions.entry(host_session_id.to_string()).or_default();
        entry.handoff_done_for = Some(start.session_id.clone());
        Some(text)
    }

    /// See [`AcpBackendConfig::with_injected_mcp_servers`].
    pub fn with_injected_mcp_servers(mut self, servers: Vec<McpServerConfig>) -> Self {
        self.injected_mcp_servers = servers;
        self
    }

    /// See [`AcpBackendConfig::with_session_meta`].
    pub fn with_session_meta(
        mut self,
        meta: Option<std::collections::HashMap<String, serde_json::Value>>,
    ) -> Self {
        self.session_meta = meta;
        self
    }

    /// See [`AcpBackendConfig::with_handoff_provider`].
    pub fn with_handoff_provider(mut self, provider: Arc<dyn HandoffProvider>) -> Self {
        self.handoff_provider = Some(provider);
        self
    }

    /// The live client, connecting or reconnecting as needed.
    async fn client(&self) -> Result<Arc<AcpClient>, AgentBackendError> {
        let mut slot = self.client.lock().await;
        if let Some(existing) = slot.as_ref() {
            if existing.is_connected() {
                return Ok(existing.clone());
            }
            // The agent died. Every id it handed out died with it —
            // but `last_agent_session_id` survives, so the reconnect
            // below can try `session/load` instead of starting cold.
            tracing::warn!(agent = %self.name, "acp-client: agent connection lost, restarting");
            self.router.invalidate_agent_sessions();
            *slot = None;
        }

        let delegate: Arc<dyn ClientDelegate> = self.router.clone();
        let client = self
            .connector
            .connect(delegate)
            .await
            .map_err(backend_error)?;
        let client = Arc::new(client);
        *slot = Some(client.clone());
        Ok(client)
    }

    /// The agent-side session id for a host session, if any.
    ///
    /// Persist this alongside the Rebon session and hand it back via
    /// [`AgentSessionSpec::resume_session_id`] to resume across a
    /// restart.
    pub fn agent_session_id(&self, host_session_id: &str) -> Option<String> {
        self.router.agent_session_id(host_session_id)
    }

    /// The id the next resume attempt would ask for, live or stored.
    pub fn remembered_agent_session_id(&self, host_session_id: &str) -> Option<String> {
        self.router.remembered_agent_session_id(host_session_id)
    }

    /// Whether writes the agent *does* route through the host get
    /// snapshotted.
    ///
    /// Deliberately not the same question as
    /// [`AgentCapabilities::writes_through_host_fs`]. That one asks
    /// "can Rebon promise `/rewind` restores this session's code",
    /// which for an ACP agent is always no — the agent chooses whether
    /// to ask us to write. This one asks "when it does ask, is the
    /// pre-image kept", which is worth knowing: it is the difference
    /// between partial rewind coverage and none at all.
    pub fn snapshots_routed_writes(&self) -> bool {
        self.snapshots_writes
    }

    /// The session mapping and reverse-request handler.
    pub fn router(&self) -> &Arc<SessionRouter> {
        &self.router
    }

    /// How many sessions this backend has started, as
    /// `(created, loaded, reused)`. The reuse share of this triple is
    /// this leg's session-reuse-rate signal.
    pub fn session_start_counts(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.session_starts.created.load(Relaxed),
            self.session_starts.loaded.load(Relaxed),
            self.session_starts.reused.load(Relaxed),
        )
    }

    /// Synchronously drop the host↔agent id mapping for one session —
    /// the Drop-guard variant of [`AgentBackend::close_session`], for
    /// holders that must clean up on non-async exit paths. Wire-silent
    /// like its async twin.
    pub fn forget_session(&self, host_session_id: &str) {
        self.router.forget(host_session_id);
    }

    /// [`AgentBackend::start_session`] with a per-session `_meta`
    /// overlay merged over the configured `sessionMeta` (overlay leaves
    /// win). The overlay is only ever sent on `session/new` and
    /// `session/load` — a `Reused` start sends nothing on the wire, so
    /// callers that need the overlay to definitely apply must arrange a
    /// fresh session (the sub-agent pool does exactly that).
    pub async fn start_session_with_meta(
        &self,
        spec: AgentSessionSpec,
        meta_overlay: Option<std::collections::HashMap<String, serde_json::Value>>,
    ) -> Result<AgentSessionStart, AgentBackendError> {
        self.start_session_inner(spec, meta_overlay).await
    }
}

/// Recursively merge `overlay` into `base`; overlay leaves win, objects
/// merge key-by-key. `None` overlay returns `base` unchanged, so the
/// trait-path `start_session` is byte-identical to before the overlay
/// existed.
fn merge_session_meta(
    base: Option<std::collections::HashMap<String, serde_json::Value>>,
    overlay: Option<std::collections::HashMap<String, serde_json::Value>>,
) -> Option<std::collections::HashMap<String, serde_json::Value>> {
    fn merge_value(base: &mut serde_json::Value, overlay: serde_json::Value) {
        match (base, overlay) {
            (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
                for (key, value) in overlay_map {
                    match base_map.get_mut(&key) {
                        Some(existing) => merge_value(existing, value),
                        None => {
                            base_map.insert(key, value);
                        }
                    }
                }
            }
            (base_slot, overlay_value) => *base_slot = overlay_value,
        }
    }

    match (base, overlay) {
        (base, None) => base,
        (None, Some(overlay)) => Some(overlay),
        (Some(mut base), Some(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_value(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
            Some(base)
        }
    }
}

impl AcpAgentBackend {
    /// The three-tier session start (`Reused`/`Loaded`/`Created`),
    /// shared by the trait path (no overlay) and
    /// [`Self::start_session_with_meta`].
    async fn start_session_inner(
        &self,
        spec: AgentSessionSpec,
        meta_overlay: Option<std::collections::HashMap<String, serde_json::Value>>,
    ) -> Result<AgentSessionStart, AgentBackendError> {
        let client = self.client().await?;

        // The directory the *agent* opens in. For an agent sharing
        // this machine that is the host session's cwd; for one reached
        // over ssh the host's cwd names a path on the wrong machine,
        // so the backend's own workspace wins. Resolved once here so
        // `session/load` and `session/new` below cannot disagree — a
        // load in one directory and a create in another would put the
        // same host session in two places on the agent's side.
        let session_cwd = self
            .workspace_cwd
            .clone()
            .unwrap_or_else(|| spec.cwd.clone());

        // 1. Reuse: the connection is live and this session already
        //    has an agent-side id.
        if let Some(agent_session_id) = self.router.agent_session_id(&spec.session_id) {
            // Recorded here too, not only on create/load. Switching a
            // session away from this agent and back clears the stored
            // id (it belonged to whichever agent was leaving), and the
            // live session would otherwise never write it back — the
            // conversation would look continuous until the next
            // restart, when it silently started cold. Idempotent.
            self.journal
                .record_agent_session(&spec.session_id, &agent_session_id);
            self.session_starts.record(SessionResumeMode::Reused);
            tracing::info!(
                agent = %self.name,
                host_session = %spec.session_id,
                agent_session = %agent_session_id,
                mode = SessionResumeMode::Reused.as_str(),
                "acp-client: session started"
            );
            return Ok(AgentSessionStart {
                session_id: agent_session_id,
                mode: SessionResumeMode::Reused,
            });
        }

        // A `Reused` start sends nothing, so an overlay can only ever
        // apply to the load/create paths below.
        let session_meta = merge_session_meta(self.session_meta.clone(), meta_overlay);

        // The spec's list replaces the config's wholesale — but the
        // injected servers ride along whichever way that goes, deduped
        // by name with the injection winning. They carry the tools
        // that keep `/rewind` meaningful; no caller gets to shadow
        // them by accident.
        let mut mcp_servers = if spec.mcp_servers.is_empty() {
            self.mcp_servers.clone()
        } else {
            spec.mcp_servers.clone()
        };
        if !self.injected_mcp_servers.is_empty() {
            mcp_servers.retain(|server| {
                !self
                    .injected_mcp_servers
                    .iter()
                    .any(|injected| injected.name() == server.name())
            });
            mcp_servers.extend(self.injected_mcp_servers.iter().cloned());
        }

        // 2. Load: we know an id from before — either the host kept
        //    one across a restart, or we saw one before the agent
        //    process died — and the agent can restore it.
        let resumable = self
            .router
            .resumable_agent_session_id(&spec.session_id, spec.resume_session_id.as_deref());
        if let Some(agent_session_id) = resumable {
            if client.supports_load_session() {
                match client
                    .session_load(
                        agent_session_id.clone(),
                        session_cwd.clone(),
                        mcp_servers.clone(),
                        session_meta.clone(),
                    )
                    .await
                {
                    Ok(result) => {
                        self.router.bind(&spec.session_id, &result.session_id);
                        self.journal
                            .record_agent_session(&spec.session_id, &result.session_id);
                        self.session_starts.record(SessionResumeMode::Loaded);
                        tracing::info!(
                            agent = %self.name,
                            host_session = %spec.session_id,
                            agent_session = %result.session_id,
                            mode = SessionResumeMode::Loaded.as_str(),
                            "acp-client: session started"
                        );
                        return Ok(AgentSessionStart {
                            session_id: result.session_id,
                            mode: SessionResumeMode::Loaded,
                        });
                    }
                    Err(err) => {
                        // A session the agent no longer has is not a
                        // failure — it is a cold start. Say so and
                        // fall through rather than stranding the user
                        // with an error they cannot act on.
                        tracing::warn!(
                            agent = %self.name,
                            agent_session = %agent_session_id,
                            error = %err,
                            "acp-client: could not resume, starting a new session"
                        );
                    }
                }
            } else {
                tracing::debug!(
                    agent = %self.name,
                    "acp-client: agent has no loadSession capability, starting fresh"
                );
            }
        }

        // 3. Create.
        let result = client
            .session_new(session_cwd, mcp_servers, session_meta)
            .await
            .map_err(backend_error)?;
        self.router.bind(&spec.session_id, &result.session_id);
        // Written down now rather than at the end of the turn: a
        // crash mid-turn should still leave a session the next run can
        // offer back to `session/load`.
        self.journal
            .record_agent_session(&spec.session_id, &result.session_id);
        self.session_starts.record(SessionResumeMode::Created);
        tracing::info!(
            agent = %self.name,
            host_session = %spec.session_id,
            agent_session = %result.session_id,
            mode = SessionResumeMode::Created.as_str(),
            "acp-client: session started"
        );
        Ok(AgentSessionStart {
            session_id: result.session_id,
            mode: SessionResumeMode::Created,
        })
    }
}

fn backend_error(err: ClientError) -> AgentBackendError {
    match err {
        ClientError::Spawn(err) => AgentBackendError::Unavailable(err.to_string()),
        ClientError::ProtocolMismatch { .. } | ClientError::LoadSessionUnsupported => {
            AgentBackendError::SessionRejected(err.to_string())
        }
        ClientError::Connection(err) => AgentBackendError::Transport(err.to_string()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[async_trait]
impl AgentBackend for AcpAgentBackend {
    /// The servers every session on this backend gets, for hosts and
    /// tests that want to see what was wired in.
    fn injected_mcp_servers(&self) -> &[McpServerConfig] {
        &self.injected_mcp_servers
    }

    /// Hand back an agent session id this host stored in a previous
    /// run, so the next [`AgentBackend::start_session`] tries
    /// `session/load` instead of starting cold.
    ///
    /// This is the other half of persisting the id: the journal writes
    /// it down, and a later run offers it back here. Without the offer,
    /// a restart can only ever mint a fresh session, and the user's
    /// conversation looks continuous while the agent has forgotten all
    /// of it.
    fn remember_agent_session(&self, host_session_id: &str, agent_session_id: &str) {
        self.router.remember(host_session_id, agent_session_id);
    }

    /// Stop the agent process deliberately.
    ///
    /// Long-lived holders (the sub-agent pool, an app-level registry)
    /// must call this at their shutdown point instead of relying on
    /// `Drop` — a backend that outlives its Rebon session has nobody
    /// dropping it at the right time. Idempotent; a later call that
    /// needs the agent again reconnects lazily.
    async fn shutdown(&self) {
        let client = self.client.lock().await.take();
        if let Some(client) = client {
            self.router.invalidate_agent_sessions();
            client.shutdown().await;
        }
    }

    fn kind(&self) -> AgentBackendKind {
        AgentBackendKind::Acp
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            // Always false for this leg, even with a snapshotting
            // `HostFs` wired in. Rebon advertises the filesystem
            // capability, but nothing *forces* an agent to use it, and
            // a real one does not: `codex-acp` writes to disk itself,
            // leaving `writes: 0` in the file-history store while the
            // file on disk has changed. This field is a promise the
            // rewind UI repeats to the user, and "some writes may be
            // recoverable" is not a promise — see
            // [`Self::snapshots_routed_writes`] for what is actually
            // true here.
            writes_through_host_fs: false,
            // We serve `session/request_permission`, so the agent's
            // tool prompts land in Rebon's permission UI.
            host_permission_prompts: true,
            // Reuse always works within a connection; surviving a
            // restart needs the agent's `loadSession`, which is not
            // known until the handshake. Reported optimistically here
            // and enforced honestly in `start_session`, which falls
            // back to `Created` rather than failing.
            resumable_sessions: true,
            // ACP's prompt result carries a stop reason and nothing
            // else. The agent's token usage is between it and its
            // provider.
            reports_usage: false,
        }
    }

    async fn start_session(
        &self,
        spec: AgentSessionSpec,
    ) -> Result<AgentSessionStart, AgentBackendError> {
        self.start_session_inner(spec, None).await
    }

    async fn prompt(&self, request: PromptRequest) -> Result<PromptOutcome, PromptExecutorError> {
        let host_session_id = request.session_id.clone();
        let spec = AgentSessionSpec {
            session_id: host_session_id.clone(),
            cwd: request.cwd.clone(),
            mcp_servers: request.mcp_servers.clone(),
            additional_working_directories: request.additional_working_directories.clone(),
            // Mid-turn there is nothing new to offer: whatever the
            // host knew was already given to `start_session` when the
            // session opened, and the router remembers the rest.
            resume_session_id: None,
        };
        let start = self
            .start_session(spec)
            .await
            .map_err(|err| PromptExecutorError::Execution(err.to_string()))?;

        // Route this turn's updates and permission prompts. Dropped on
        // every exit path, including the cancel below.
        let _turn = self.router.begin_turn(
            &host_session_id,
            request.update_publisher.clone(),
            request.permission_publisher.clone(),
        );

        // Open the turn in the journal *after* the session exists, so a
        // session that could not be started leaves no orphan prompt in
        // the transcript, and *before* the agent is prompted, because
        // the user row is what file-history snapshots hang off.
        let mut journal_turn = JournalTurn::open(
            &self.journal,
            &host_session_id,
            self.journal.begin_turn(&host_session_id, &request.prompt),
        );

        let client = self
            .client()
            .await
            .map_err(|err| PromptExecutorError::Execution(err.to_string()))?;

        let agent_session_id = start.session_id.clone();
        // A session the agent just minted knows nothing about the
        // conversation on screen. Catch it up once, on the wire only:
        // the journal already recorded the user's actual prompt, and
        // writing the handoff there too would make the next handoff
        // quote the previous one.
        let wire_prompt = match self.handoff_for(&host_session_id, &start) {
            Some(handoff) => {
                let mut blocks = Vec::with_capacity(request.prompt.len() + 1);
                blocks.push(rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: handoff,
                    annotations: None,
                }));
                blocks.extend(request.prompt.iter().cloned());
                blocks
            }
            None => request.prompt.clone(),
        };
        let prompt = client.prompt(agent_session_id.clone(), wire_prompt);
        tokio::pin!(prompt);

        // The turn ends when the agent says so — but a local cancel has
        // to reach the agent, and then we keep waiting, because only
        // the agent can report the stop reason for work it is doing.
        let result = tokio::select! {
            result = &mut prompt => result,
            () = request.cancel.notified() => {
                tracing::info!(
                    agent = %self.name,
                    agent_session = %agent_session_id,
                    "acp-client: forwarding cancel"
                );
                if let Err(err) = client.cancel(agent_session_id.clone()).await {
                    tracing::warn!(error = %err, "acp-client: cancel could not be delivered");
                    journal_turn.finish(rebon_types::StopReason::Cancelled);
                    return Err(PromptExecutorError::Cancelled);
                }
                (&mut prompt).await
            }
        };

        match result {
            Ok(result) => {
                journal_turn.finish(result.stop_reason);
                Ok(PromptOutcome {
                    stop_reason: result.stop_reason,
                    // ACP reports no usage; zeros here mean "not
                    // reported", which is why `reports_usage` is false.
                    usage: rebon_types::Usage::default(),
                })
            }
            Err(err) if request.cancel.is_cancelled() => {
                tracing::debug!(error = %err, "acp-client: turn ended after cancel");
                journal_turn.finish(rebon_types::StopReason::Cancelled);
                Err(PromptExecutorError::Cancelled)
            }
            Err(err) => Err(PromptExecutorError::Execution(err.to_string())),
        }
    }

    async fn cancel(&self, session_id: &str) -> Result<(), AgentBackendError> {
        let Some(agent_session_id) = self.router.agent_session_id(session_id) else {
            // Nothing was ever started on the agent, so there is
            // nothing there to stop.
            return Ok(());
        };
        let slot = self.client.lock().await;
        let Some(client) = slot.as_ref() else {
            return Ok(());
        };
        client.cancel(agent_session_id).await.map_err(backend_error)
    }

    async fn steer(
        &self,
        session_id: &str,
        blocks: Vec<rebon_types::ContentBlock>,
        user_message_uuid: &str,
    ) -> Result<rebon_agent_core::SteerOutcome, AgentBackendError> {
        // Deliberately NOT `self.client()`: that path reconnects a
        // dead agent and invalidates its sessions, which is the right
        // move before a fresh turn and a pointless one mid-steer — a
        // turn on a dead connection cannot be steered anyway.
        let client = {
            let slot = self.client.lock().await;
            match slot.as_ref() {
                Some(client) if client.is_connected() => client.clone(),
                _ => {
                    return Err(AgentBackendError::Unavailable(
                        "the agent connection is not live".to_string(),
                    ))
                }
            }
        };
        if !client.supports_steering() {
            return Err(AgentBackendError::Unsupported(format!(
                "agent `{}` did not advertise steering",
                self.name
            )));
        }
        let Some(agent_session_id) = self.router.agent_session_id(session_id) else {
            return Err(AgentBackendError::UnknownSession(session_id.to_string()));
        };

        let result = client
            .steer(agent_session_id.clone(), blocks.clone())
            .await
            .map_err(backend_error)?;
        match result.outcome {
            rebon_proto::types::SteeringOutcome::Injected => {
                // Recorded after the agent confirmed delivery, so a
                // failed send never leaves a ghost row. The remaining
                // race — the turn ending between confirmation and this
                // line — drops the row with a warning, which the turn
                // outcome makes survivable: the caller re-sends.
                self.journal
                    .record_steered_prompt(session_id, user_message_uuid, &blocks);
                Ok(rebon_agent_core::SteerOutcome::Injected)
            }
            rebon_proto::types::SteeringOutcome::StartedNewTurn => {
                // The turn raced ahead and finished; the agent started
                // a turn of its own with the message. Nobody on our
                // side is awaiting that turn — its output would stream
                // into a session whose publisher is already gone — so
                // cancel it and let the caller re-send the message as
                // an ordinary prompt through the normal lifecycle.
                if let Err(err) = client.cancel(agent_session_id).await {
                    tracing::warn!(
                        error = %err,
                        agent = %self.name,
                        "acp-client: could not cancel the agent's self-started turn"
                    );
                }
                Ok(rebon_agent_core::SteerOutcome::TurnAlreadyOver)
            }
        }
    }

    async fn close_session(&self, session_id: &str) -> Result<(), AgentBackendError> {
        self.router.forget(session_id);
        Ok(())
    }
}

impl std::fmt::Debug for AcpAgentBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpAgentBackend")
            .field("name", &self.name)
            .field("agent", &self.connector.label())
            .field("snapshots_writes", &self.snapshots_writes)
            .finish()
    }
}
