//! Agent groups (RFC-0009, `docs/rfc-agent-groups` in the desktop repo).
//!
//! A group is a handful of agents working in one project — Rebon sessions,
//! Claude Code conversations, other CLIs — that talk through a log Rebon
//! keeps for them instead of through each other's context:
//!
//! ```text
//! <config home>/groups/<group id>/
//!     group.json     who is in it, under which alias, and how they are reached
//!     log.jsonl      notes, requests, replies, memory — one entry per line, by seq
//!     cursors.json   per member: the last entry delivered, the last one read
//!     MEMORY.md      the group's memory as a document, rebuilt from the log
//!     lock           held while any of the above changes
//! ```
//!
//! Everything a member does is an append: a note, a request, the reply to it
//! and a remembered fact differ only in their kind. Nothing is ever pushed
//! into an agent's system prompt or rewritten in its history; reading the
//! log is the agent's own tool call.
//!
//! - [`store`] — the files and the lock.
//! - [`identity`] — which session a server belongs to, from the environment
//!   the agent started it with.
//! - [`tools`] — the tool schemas and what each call does, as JSON in and
//!   JSON out, for whichever host offers them.
//!
//! Host-neutral on purpose: this crate knows nothing of MCP or of the
//! engine. `rebon mcp serve` wraps [`tools`] for any MCP client; a Rebon
//! session is meant to get the same tools from a feature plugin.

pub mod identity;
pub mod model;
pub mod store;
pub mod tools;

pub use identity::{AgentKind, Caller};
pub use model::{Delivery, Entry, EntryKind, Group, Member, Warmth};
pub use store::GroupStore;

/// Where groups live under a config home.
pub fn default_root(config_home: &std::path::Path) -> std::path::PathBuf {
    config_home.join("groups")
}
