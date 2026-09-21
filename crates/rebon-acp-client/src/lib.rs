//! Agent Client Protocol **client**: Rebon driving somebody else's agent.
//!
//! Rebon also has a server end, where an editor drives Rebon over stdio.
//! Here the roles are reversed: Rebon spawns a third-party agent CLI and
//! drives *it* — same protocol, same wire layer ([`rebon_proto`]),
//! opposite ends. The two ends deliberately do not depend on each
//! other; what they share lives upstream.
//!
//! ```text
//!   rebon-proto   ← the wire (framing, transport, JSON-RPC + ACP types)
//!        ├── the server end    (an editor drives Rebon)
//!        └── rebon-acp-client  (client: Rebon drives an agent)
//! ```
//!
//! # The pieces
//!
//! - [`process`] — the child process, and the guarantee it does not
//!   outlive the session that spawned it.
//! - [`pending`] — outbound request bookkeeping, including the part
//!   that turns a dead agent into an error instead of a hang.
//! - [`connection`] — the JSON-RPC peer: our requests out, the
//!   agent's requests in.
//! - [`host_fs`] — where the agent's file reads and writes land, and
//!   whether they can be rewound.
//! - [`backend`] — all of the above as a
//!   [`rebon_agent_core::AgentBackend`], with the session-id mapping
//!   and three-tier resume.
//!
//! # What this leg cannot promise
//!
//! Two things are worth knowing before pointing a session at an
//! external agent:
//!
//! - **Rewind depends on the agent cooperating.** Rebon advertises
//!   `fs/write_text_file` so writes come back through the host and can
//!   be snapshotted, but nothing forces an agent to use it. So
//!   `writes_through_host_fs` is always `false` on this leg — /rewind
//!   cannot promise it restores anything — and the backend reports
//!   [`AcpAgentBackend::snapshots_routed_writes`] from the
//!   [`host_fs::HostFs`] the host wired in: whether the writes that do
//!   route through the host keep their pre-image.
//! - **Token usage is not visible.** ACP's prompt result carries a
//!   stop reason and nothing else; what the agent spent is between it
//!   and its provider. Session *reuse rate* — how often
//!   [`rebon_agent_core::SessionResumeMode::Reused`] comes back — is
//!   the closest thing to a cache-efficiency signal this leg has.

pub mod backend;
pub mod client;
pub mod connection;
pub mod connector;
pub mod fs_mcp;
pub mod host_fs;
pub mod journal;
pub mod pending;
pub mod process;

#[cfg(test)]
mod tests;

pub use backend::{AcpAgentBackend, AcpBackendConfig, HandoffProvider, SessionRouter};
pub use client::{
    default_client_capabilities, AcpClient, ClientError, ACP_CLIENT_PROTOCOL_VERSION,
};
pub use connection::{ClientDelegate, Connection, ConnectionError};
pub use connector::{AgentConnector, ProcessConnector};
pub use fs_mcp::{HostFsService, HostFsServiceHandle, FS_BRIDGE_SUBCOMMAND, FS_MCP_SERVER_NAME};
pub use host_fs::{DirectHostFs, HostFileHistory, HostFs, SnapshotHostFs};
pub use journal::{NoopTurnJournal, TranscriptJournal, TranscriptSink, TurnJournal};
pub use pending::{PendingError, PendingRequests};
pub use process::{AgentCommand, AgentProcess, SpawnError};
