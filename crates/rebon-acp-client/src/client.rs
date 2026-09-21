//! Driving a third-party agent: handshake, sessions, turns.
//!
//! [`AcpClient`] owns a spawned agent and the connection to it, and
//! exposes the four things a host actually does with one: negotiate
//! capabilities, get a session, prompt it, tell it to stop.
//!
//! # What the handshake decides
//!
//! `initialize` is where both sides state what they can do, and the
//! answers change behaviour rather than just being recorded:
//!
//! - Rebon advertises `fs.readTextFile` / `fs.writeTextFile` so the
//!   agent routes file access back through us. That is not politeness
//!   — a write that comes back as `fs/write_text_file` can be
//!   snapshotted before it lands, which is the difference between
//!   `/rewind` working and lying.
//! - The agent advertises `loadSession`. Without it there is no way to
//!   resume a session after the process restarts, so
//!   [`SessionResumeMode::Loaded`] is off the table and a returning
//!   user pays for a cold session.

use std::sync::Arc;

use rebon_proto::types::{
    AgentCapabilities as ProtoAgentCapabilities, ClientCapabilities, ContentBlock, FsCapabilities,
    ImplementationInfo, InitializeParams, InitializeResult, McpServerConfig, ProtocolVersion,
    SessionCancelParams, SessionLoadParams, SessionLoadResult, SessionNewParams, SessionNewResult,
    SessionPromptParams, SessionPromptResult, SessionSteeringParams, SessionSteeringResult,
};
use rebon_proto::FramingMode;

use crate::connection::{method, ClientDelegate, Connection, ConnectionError};
use crate::process::{AgentCommand, AgentProcess, SpawnError};

/// ACP protocol version this client speaks. The server end speaks the
/// same version; they are the same protocol from opposite ends.
pub const ACP_CLIENT_PROTOCOL_VERSION: ProtocolVersion = 1;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Spawn(#[from] SpawnError),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error("agent speaks ACP v{agent}, this client speaks v{client}")]
    ProtocolMismatch {
        agent: ProtocolVersion,
        client: ProtocolVersion,
    },
    #[error("agent cannot resume sessions (no loadSession capability)")]
    LoadSessionUnsupported,
}

/// What Rebon tells the agent it can do.
///
/// Defaults to advertising filesystem access, because the whole point
/// of hosting somebody else's agent inside Rebon is that Rebon's
/// guarantees — permissions, snapshots, rewind — keep applying.
pub fn default_client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        fs: Some(FsCapabilities {
            read_text_file: Some(true),
            write_text_file: Some(true),
        }),
        terminal: Some(false),
        meta: None,
    }
}

/// A connected, initialized agent.
pub struct AcpClient {
    connection: Arc<Connection>,
    /// The child process, when the agent is one. A client speaking to
    /// an agent over some other pipe — a test harness, a socket — is
    /// still a client; it just has no process to reap.
    process: tokio::sync::Mutex<Option<AgentProcess>>,
    agent_capabilities: ProtoAgentCapabilities,
    agent_info: Option<ImplementationInfo>,
    /// Top-level `_meta` from the initialize result — extension
    /// advertisements like steering live here, not in the
    /// capabilities.
    agent_meta: Option<std::collections::HashMap<String, serde_json::Value>>,
    label: String,
}

impl AcpClient {
    /// Spawn the agent and complete the `initialize` handshake.
    pub async fn connect(
        spec: &AgentCommand,
        client_capabilities: ClientCapabilities,
        delegate: Arc<dyn ClientDelegate>,
    ) -> Result<Self, ClientError> {
        let label = spec.display();
        let (process, stdout, stdin) = AgentProcess::spawn(spec)?;
        Self::handshake(
            stdout,
            stdin,
            Some(process),
            label,
            client_capabilities,
            delegate,
        )
        .await
    }

    /// Complete the handshake over an already-open pipe.
    ///
    /// The process argument is what ties the agent's lifetime to this
    /// client; pass `None` when something else owns it.
    pub async fn connect_over_pipe<R, W>(
        reader: R,
        writer: W,
        process: Option<AgentProcess>,
        label: impl Into<String>,
        client_capabilities: ClientCapabilities,
        delegate: Arc<dyn ClientDelegate>,
    ) -> Result<Self, ClientError>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        Self::handshake(
            reader,
            writer,
            process,
            label.into(),
            client_capabilities,
            delegate,
        )
        .await
    }

    async fn handshake<R, W>(
        reader: R,
        writer: W,
        process: Option<AgentProcess>,
        label: String,
        client_capabilities: ClientCapabilities,
        delegate: Arc<dyn ClientDelegate>,
    ) -> Result<Self, ClientError>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let connection = Connection::spawn(reader, writer, FramingMode::Ndjson, delegate);

        let result: InitializeResult = connection
            .request(
                method::INITIALIZE,
                InitializeParams {
                    protocol_version: ACP_CLIENT_PROTOCOL_VERSION,
                    client_capabilities,
                    client_info: Some(ImplementationInfo {
                        name: "rebon".to_string(),
                        title: None,
                        version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    }),
                },
            )
            .await?;

        if result.protocol_version != ACP_CLIENT_PROTOCOL_VERSION {
            connection.shutdown("protocol version mismatch");
            return Err(ClientError::ProtocolMismatch {
                agent: result.protocol_version,
                client: ACP_CLIENT_PROTOCOL_VERSION,
            });
        }

        tracing::info!(
            agent = %label,
            agent_name = result.agent_info.as_ref().map(|info| info.name.as_str()).unwrap_or("?"),
            load_session = result.agent_capabilities.load_session.unwrap_or(false),
            "acp-client: agent initialized"
        );

        Ok(Self {
            connection,
            process: tokio::sync::Mutex::new(process),
            agent_capabilities: result.agent_capabilities,
            agent_info: result.agent_info,
            agent_meta: result.meta,
            label,
        })
    }

    /// The command line this agent was started with.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// What the agent said it could do during the handshake.
    pub fn agent_capabilities(&self) -> &ProtoAgentCapabilities {
        &self.agent_capabilities
    }

    /// Name and version the agent reported, if it reported any.
    pub fn agent_info(&self) -> Option<&ImplementationInfo> {
        self.agent_info.as_ref()
    }

    /// Whether `session/load` is available — i.e. whether a session
    /// can survive this process.
    pub fn supports_load_session(&self) -> bool {
        self.agent_capabilities.load_session.unwrap_or(false)
    }

    /// Whether the agent takes `_session/steering` — a message
    /// injected into the running turn instead of queued behind it.
    /// Both `claude-agent-acp` and `codex-acp` advertise this the same
    /// way; absence means "queue and wait", never an error.
    pub fn supports_steering(&self) -> bool {
        self.agent_meta
            .as_ref()
            .and_then(|meta| meta.get("steering"))
            .and_then(|steering| steering.get("supported"))
            .and_then(|supported| supported.as_bool())
            .unwrap_or(false)
    }

    /// Inject a message into the turn currently running on
    /// `agent_session_id`. The caller owns the fallback: an outcome of
    /// [`SteeringOutcome::StartedNewTurn`] means the turn had already
    /// finished and the agent opened a fresh one nobody is awaiting.
    pub async fn steer(
        &self,
        agent_session_id: impl Into<String>,
        prompt: Vec<ContentBlock>,
    ) -> Result<SessionSteeringResult, ClientError> {
        Ok(self
            .connection
            .request(
                method::SESSION_STEERING,
                SessionSteeringParams {
                    session_id: agent_session_id.into(),
                    prompt,
                },
            )
            .await?)
    }

    /// Whether the connection is still usable.
    pub fn is_connected(&self) -> bool {
        !self.connection.is_closed()
    }

    /// Mint a new agent-side session.
    pub async fn session_new(
        &self,
        cwd: impl Into<String>,
        mcp_servers: Vec<McpServerConfig>,
        meta: Option<std::collections::HashMap<String, serde_json::Value>>,
    ) -> Result<SessionNewResult, ClientError> {
        Ok(self
            .connection
            .request(
                method::SESSION_NEW,
                SessionNewParams {
                    cwd: cwd.into(),
                    mcp_servers,
                    meta,
                },
            )
            .await?)
    }

    /// Ask the agent to restore a session it stored earlier.
    ///
    /// Fails fast when the agent never advertised `loadSession`,
    /// rather than sending a request we know it will reject.
    pub async fn session_load(
        &self,
        session_id: impl Into<String>,
        cwd: impl Into<String>,
        mcp_servers: Vec<McpServerConfig>,
        meta: Option<std::collections::HashMap<String, serde_json::Value>>,
    ) -> Result<SessionLoadResult, ClientError> {
        if !self.supports_load_session() {
            return Err(ClientError::LoadSessionUnsupported);
        }
        Ok(self
            .connection
            .request(
                method::SESSION_LOAD,
                SessionLoadParams {
                    session_id: session_id.into(),
                    cwd: cwd.into(),
                    mcp_servers,
                    meta,
                },
            )
            .await?)
    }

    /// Run one turn. Resolves when the agent reports a stop reason.
    pub async fn prompt(
        &self,
        session_id: impl Into<String>,
        prompt: Vec<ContentBlock>,
    ) -> Result<SessionPromptResult, ClientError> {
        Ok(self
            .connection
            .request(
                method::SESSION_PROMPT,
                SessionPromptParams {
                    session_id: session_id.into(),
                    prompt,
                    meta: None,
                },
            )
            .await?)
    }

    /// Tell the agent to stop the current turn.
    ///
    /// A notification, not a request: the turn's own `session/prompt`
    /// response is what reports that it actually stopped.
    pub async fn cancel(&self, session_id: impl Into<String>) -> Result<(), ClientError> {
        Ok(self
            .connection
            .notify(
                method::SESSION_CANCEL,
                SessionCancelParams {
                    session_id: session_id.into(),
                },
            )
            .await?)
    }

    /// Close the connection and stop the agent.
    pub async fn shutdown(&self) {
        self.connection.shutdown("client shutting down");
        if let Some(process) = self.process.lock().await.as_mut() {
            process.shutdown().await;
        }
    }
}

impl std::fmt::Debug for AcpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpClient")
            .field("label", &self.label)
            .field("connected", &self.is_connected())
            .field("load_session", &self.supports_load_session())
            .finish()
    }
}
