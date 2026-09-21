//! Typed MCP runtime and turn-contribution seats.
//! The engine consumes contracts here; connection and tool construction belong
//! to the feature plugin. An absent seat means MCP is unavailable, not a
//! request to construct a fallback implementation in the engine.

use crate::tool_seat::SeatToolProvider;
use futures_util::future::BoxFuture;
use rebon_agent_core::PromptExecutorError;
use rebon_kernel::Service;
use rebon_proto::McpServerConfig;
use rebon_tool::{McpClient, McpToolDefinition};
use std::sync::Arc;

pub const MCP_RUNTIME_SERVICE: &str = "mcp/runtime";
pub const MCP_TURN_TOOLS_SERVICE: &str = "mcp/turn-tools";

/// Owned input so connection work never borrows an executor or turn scope.
pub struct McpSessionRequest {
    pub session_id: String,
    /// Empty removes the session-specific configuration and its cached client.
    pub servers: Vec<McpServerConfig>,
    /// None means no host/global client was supplied.
    pub global: Option<Arc<dyn McpClient>>,
}

pub type McpSessionFactory = dyn Fn(
        McpSessionRequest,
    ) -> BoxFuture<'static, Result<Option<Arc<dyn McpClient>>, PromptExecutorError>>
    + Send
    + Sync;

pub struct McpRuntimeService;
impl Service for McpRuntimeService {
    type Interface = McpSessionFactory;
    const NAME: &'static str = MCP_RUNTIME_SERVICE;
}

/// The plugin supplies a live provider, not a copy of executable tools owned
/// by the engine. None means this plugin generation has already unloaded.
pub type McpTurnContribution =
    dyn Fn(Vec<(String, McpToolDefinition)>) -> Option<Arc<dyn SeatToolProvider>> + Send + Sync;

pub struct McpTurnToolsService;
impl Service for McpTurnToolsService {
    type Interface = McpTurnContribution;
    const NAME: &'static str = MCP_TURN_TOOLS_SERVICE;
}
