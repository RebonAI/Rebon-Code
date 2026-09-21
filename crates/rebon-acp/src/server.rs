//! ACP JSON-RPC 2.0 server loop.
//!
//! Reads messages from a [`StdioReader`], classifies them as requests,
//! notifications, or responses, dispatches requests through a user-provided
//! [`RequestHandler`], and writes JSON-RPC responses back through a
//! [`StdioWriter`] using the framing that the reader auto-detected. The
//! server matches the wire framing of the peer (NDJSON or Content-Length).
//!
//! The layering is deliberate: a handler owns method routing, the
//! transport owns wire format, and everything else (sessions, prompt
//! turns, tool execution) lives at a higher layer.
//!
//! Concretely, this module supports:
//!
//! - `initialize` requests via [`DefaultHandler`] (duplicate initialize is
//!   rejected with `-32600 Invalid Request: "Already initialized"`)
//! - `session/new` requests, gated on a prior successful `initialize`
//!   (returns `-32600 Invalid Request: "Not initialized. Call initialize first."`
//!   when the order is wrong); the response carries a freshly-minted
//!   `sessionId`.
//! - `session/prompt` requests, gated on initialize + an existing session
//!   id; the handler records the prompt content on the session and returns
//!   `{ stopReason: "end_turn" }` synchronously. Model integration,
//!   streaming `session/update` notifications, and tool execution live
//!   at a higher layer.
//! - `session/load` requests, gated on initialize; the handler resolves
//!   `${projects_root}/${sanitize(cwd)}/${sessionId}.jsonl`, walks the
//!   transcript JSONL best-effort, and reinserts the restored session
//!   into the shared state map. Tests can override `projects_root` to
//!   a temp dir; production falls back to
//!   `${REBON_CONFIG_DIR:-$HOME/.rebon}/projects`. Metadata-line
//!   parsing and rich message reconstruction are out of scope here.
//! - `session/list` requests, gated on initialize; the handler snapshots
//!   the in-memory session map, scans
//!   `${projects_root}/${sanitize(effective_cwd)}/` for `*.jsonl`
//!   stems, inserts lightweight placeholders for any stem not already
//!   in memory, and post-filters by `params.cwd` only when the caller
//!   supplied a non-blank cwd. Pagination (`cursor`/`nextCursor`),
//!   title extraction from on-disk placeholders, worktree fallback,
//!   and sorting are all deliberately out of scope.
//! - `session/cancel` notifications are routed through
//!   [`RequestHandler::handle_notification`] and, for known sessions,
//!   recorded on the shared [`ServerState`] for higher layers to consume.
//!   Unknown sessions are silently ignored.
//! - method-not-found errors for any other request method
//! - silent ignore of unknown notifications (per JSON-RPC 2.0 spec)
//! - JSON parse errors → `-32700 Parse error` responses with `id: null`
//! - structurally invalid JSON-RPC messages → `-32600 Invalid Request`

use rebon_proto::types::ProtocolVersion;

/// Highest ACP protocol version understood by this server.
pub const ACP_PROTOCOL_VERSION: ProtocolVersion = 1;

pub use handler::{
    acp_prompt_with_ultrawork_reminder, starts_with_ultrawork_command, DefaultHandler,
    RequestHandler, SessionConfigOptions, SteeringMessage, SteeringSink,
};
pub use loop_::{serve, serve_with_publisher, serve_with_publishers};

mod commands;
mod config;
mod handler;
mod loop_;
#[cfg(test)]
mod tests;
