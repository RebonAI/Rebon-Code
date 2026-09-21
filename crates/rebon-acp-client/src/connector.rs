//! How a backend gets a connected agent.
//!
//! In production this is "spawn the CLI and shake hands", which is why
//! [`ProcessConnector`] is the only implementation that ships. It is a
//! trait because the alternative — [`crate::AcpAgentBackend`] calling
//! `Command::spawn` directly — makes the session logic above it
//! (three-tier resume, id mapping, reconnect-after-death) reachable
//! only by launching real binaries, which is not a thing a test should
//! have to do.

use std::sync::Arc;

use rebon_proto::types::ClientCapabilities;

use crate::client::{AcpClient, ClientError};
use crate::connection::ClientDelegate;
use crate::process::AgentCommand;

/// Produces connected, initialized agents on demand.
#[async_trait::async_trait]
pub trait AgentConnector: Send + Sync {
    /// Connect and complete the handshake.
    ///
    /// Called once per connection, including reconnects after the
    /// agent dies — so implementations must be able to run more than
    /// once.
    async fn connect(&self, delegate: Arc<dyn ClientDelegate>) -> Result<AcpClient, ClientError>;

    /// How the agent is identified in logs and errors.
    fn label(&self) -> String;
}

/// Starts the agent as a child process.
pub struct ProcessConnector {
    command: AgentCommand,
    client_capabilities: ClientCapabilities,
}

impl ProcessConnector {
    pub fn new(command: AgentCommand, client_capabilities: ClientCapabilities) -> Self {
        Self {
            command,
            client_capabilities,
        }
    }

    pub fn command(&self) -> &AgentCommand {
        &self.command
    }
}

#[async_trait::async_trait]
impl AgentConnector for ProcessConnector {
    async fn connect(&self, delegate: Arc<dyn ClientDelegate>) -> Result<AcpClient, ClientError> {
        AcpClient::connect(&self.command, self.client_capabilities.clone(), delegate).await
    }

    fn label(&self) -> String {
        self.command.display()
    }
}
