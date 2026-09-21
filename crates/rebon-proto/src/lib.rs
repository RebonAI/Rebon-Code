//! Direction-agnostic Agent Client Protocol wire layer.
//!
//! ACP speaks JSON-RPC 2.0 over stdio with two possible framings
//! (Content-Length or NDJSON), auto-detected from the first bytes of the
//! connection. Nothing here knows which end of the connection it is on: the
//! same crate serves an agent being driven and a client driving a third-party
//! agent CLI.
//!
//! - [`framing`] — a pure, synchronous frame decoder that auto-detects
//!   Content-Length vs NDJSON framing and yields raw message bodies.
//! - [`transport`] — async stdio reader / writer built on top of the decoder,
//!   parameterized over any `AsyncRead`/`AsyncWrite`.
//! - [`types`] — the JSON-RPC 2.0 + ACP data types: initialize, session setup,
//!   prompt turn, content blocks, permission requests, session updates.
//! - [`web_api`] — not ACP: the shapes the local server returns from its
//!   `/api/*` reads. They are here because the browser client reads both, and
//!   its TypeScript is generated from this crate.
//! - [`mcp_channel`] — not ACP either: the MCP channel push
//!   (`notifications/claude/channel`), which Rebon both hosts and serves.

pub mod framing;
pub mod mcp_channel;
pub mod transport;
pub mod types;
pub mod web_api;

pub use framing::{FrameDecodeStep, FrameDecoder, FramingMode};
pub use transport::{StdioReader, StdioWriter};
pub use types::{
    error_code, AgentCapabilities, AudioContent, AuthMethod, ClientCapabilities, ConfigOption,
    ConfigOptionType, ConfigOptionValue, ContentBlock, DiffContent, FsCapabilities, ImageContent,
    ImplementationInfo, InitializeParams, InitializeResult, JsonRpcError, JsonRpcMessage,
    JsonRpcNotification, JsonRpcParseError, JsonRpcRequest, JsonRpcResponse, JsonRpcVersion,
    McpCapabilities, McpServerConfig, PermissionOption, PermissionOptionKind, PermissionOutcome,
    PlanEntry, PlanEntryPriority, PlanEntryStatus, PromptCapabilities, ProtocolVersion,
    ReadTextFileParams, ReadTextFileResult, RegularContent, RequestId, RequestPermissionParams,
    RequestPermissionResult, ResourceBody, ResourceContent, ResourceLinkContent,
    SessionCancelParams, SessionCapabilities, SessionId, SessionInfo, SessionListCapability,
    SessionListParams, SessionListResult, SessionLoadParams, SessionLoadResult, SessionNewParams,
    SessionNewResult, SessionPromptParams, SessionPromptResult, SessionSetConfigOptionParams,
    SessionSetConfigOptionResult, SessionUpdate, SessionUpdateParams, SlashCommand,
    SlashCommandCategory, SlashCommandInput, StopReason, TerminalContent, TextContent,
    ToolCallContent, ToolCallLocation, ToolCallReference, ToolCallStatus, ToolKind,
    WriteTextFileParams, WriteTextFileResult,
};
pub use web_api::{
    ActionResult, AgentEntry, AgentOption, AgentsResponse, CommandsResponse, FilesResponse,
    HistorySnapshot, ModelOption, ModelsResponse, RewindCheckpoint, RewindResponse, ServerInfo,
    SkillEntry, SkillsResponse, TaskEntry, TasksResponse, UsageResponse, WebUiSource,
};
