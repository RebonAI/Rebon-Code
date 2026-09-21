//! `rebon mcp serve`: Rebon's background jobs as an MCP server, with the
//! outcome pushed back over the MCP channel extension.
//!
//! ```text
//! MCP client ──spawns──▶ rebon mcp serve        (stdio, this crate)
//!                            │  launch / reply / answer / stop: the host's
//!                            │  own transactions; reads state.json
//!                            ▼
//!                  background supervisor / worker ──▶ ~/.rebon/jobs/<id>/
//!                            ▲                       state.json   (authority)
//!                            │                       mcp-channel.json (ledger)
//!                            └── rebon --bg, the TUI, the app      result.md (projection)
//! ```
//!
//! **Rebon holds the job; MCP only carries the news.** The client never owns a
//! process, so a job is not bound by any client timeout and survives the
//! client exiting or crashing (G1, G3). `exec_start` returns a job id at once;
//! when the job finishes or parks on a question, the watcher pushes a
//! `notifications/claude/channel` message carrying status and a result-file
//! path — never instructions, because the host marks pushes untrusted.
//!
//! **The tool surface stands on its own.** A push is an optimisation: every
//! fact it carries is also answered by `job_status` / `job_result`, and a
//! client that never receives one (channels gated off, a restarted server
//! between two ticks) loses latency, not information (G5, §7).
//!
//! Where things are decided:
//!
//! - `jobs` — the operations, and the rules around them: which jobs are
//!   visible, where a job may run, which permission answers may be given.
//! - `ledger` — which jobs this surface started, who pushes for them, and
//!   the at-most-once record of what was pushed.
//! - `push` — what a push says (fixed templates), and the throttle.
//! - `watch` — when to look.
//! - `result` — the result file.
//! - `server` / `tools` — the JSON-RPC connection and the tool schemas.
//!
//! An endpoint, like the terminal and the desktop app: it depends on
//! `rebon-session-host` and nothing above it, and nothing depends on it but
//! the binary.

pub mod cli;
mod jobs;
mod ledger;
mod push;
mod result;
mod server;
mod tools;
mod watch;

pub use jobs::LaunchGate;
pub use ledger::LedgerOwner;
pub use server::{serve, ServeConfig, SERVER_NAME};
pub use watch::Cadence;
