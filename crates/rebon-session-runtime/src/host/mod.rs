//! The worker host and its IPC server: what actually runs a session that has
//! no screen.
//!
//! `rebon-cli` held these until they landed here. They came out because a worker is
//! not a terminal concern — the binary's job is to parse `__background-worker`
//! and call in — and because the layering asks for exactly one implementation of each,
//! reachable without depending on the binary.
//!
//! The module is a facade as well as a parent: the files under it that need
//! these names open with `use super::super::*`, which is how they reached the
//! binary's `background` module before. The re-exports below are that surface, now sourced from
//! `rebon-session-host` (state, store, protocol, owner) and from this crate
//! (the session itself). Nothing here is re-declared; if a name is missing the
//! compiler says so rather than a second definition appearing.

pub mod agent_view_summary;
pub mod ipc;
pub mod permission_snapshot;
pub mod runtime_fields;
pub mod runtime_permissions;
pub mod task_bridge;
pub mod worker;

#[cfg(test)]
mod tests;

// What the binary's `background` module put in scope for these files. The
// files that need it open with `use super::super::*`, so this is their prelude;
// keeping it here rather than editing their imports is what made the move
// mechanical.
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

use crate::commands::effort::provider_kind_from_format;
use rebon_core::permission::{OutboundPermissionQuery, PermissionAnswer};
use rebon_core::query::AttachmentPoller;

use anyhow::Context;
use rebon_plugin_tasks::runtime::{
    stop_task, TaskData, TaskEventCursor, TaskId, TaskLiveEvent, TaskLiveEventKind,
    TaskNotification, TaskRegistry, TaskSnapshot, TaskStatus, TaskTerminalStream,
};
#[cfg(test)]
use rebon_session_host::hide_background_command_window;
#[cfg(test)]
use rebon_session_host::send_background_ipc_request;
use rebon_session_host::shorten_excerpt;
use rebon_tool::TeamManager;
use rebon_types::constant_time_eq;
// The supervisor's process half, which the worker uses to claim and to reap.
// It moved to `rebon-session-host`; this is the same glob the binary's
// facade kept for exactly this reason.

// The job record, its store, and the wire the server speaks — by name, not by
// glob. A glob would bring in `rebon_session_host::SessionEventStream`, the
// *client's* stream, and shadow the server-side publisher of the same name in
// `ipc::events`; the binary's facade imported these one at a time for the same
// reason.
// The projection from a live permission query to the snapshot the wire
// carries. It used to live in `rebon-session-host`, which had to depend on
// `rebon-core` for the query type; this crate depends on both halves
// already, so it sits here and that edge is gone.
pub use permission_snapshot::background_permission_snapshot;

pub use rebon_session_host::BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS;
use rebon_session_host::TERMINAL_WORKER_IDLE_TTL_MS;
use rebon_session_host::{
    background_job_failure_summary, background_job_success_summary,
    background_job_success_summary_from_store, background_job_transcript_cwd,
    background_worktree_slug, build_ask_user_question_updated_input, generate_ipc_token,
    generate_pending_prompt_id, now_ms, preserved_background_worktree_path, process_is_running,
    process_owns_detached_group, should_skip_background_worktree, validate_job_id,
};
pub use rebon_session_host::{
    detach_background_job_from_parent, mark_existing_background_session_idle,
    stop_background_job_tree_in_store, BackgroundDispatchPrompt, BackgroundImageAttachment,
    BackgroundIpcEndpoint, BackgroundIpcEnvelope, BackgroundIpcRequest, BackgroundIpcResponse,
    BackgroundJobState, BackgroundJobStatus, BackgroundLaunchOptions,
    BackgroundPermissionQuerySnapshot, BackgroundPullRequestDotStatus, BackgroundPullRequestStatus,
    BackgroundRuntimeFields, BackgroundStore, BackgroundTaskDescriptor, BackgroundTaskEvent,
    BackgroundTaskEventBatch, BackgroundTaskEventKind, PendingPrompt, RecordedOwnerSnapshot,
    StoppedJobTree,
};

// The host's own surface, which its files reach through the same glob. The
// shape is the binary facade's, kept so the moved files' `use super::super::*`
// resolves to the same set of names it always did.
pub use ipc::acp_permission::{permission_option_kind, permission_params};
pub use ipc::commands::{permission_retry_blocked_by_running_turn, WorkerCommandInputs};
pub use ipc::server::{start_background_ipc_server, BackgroundIpcServer};
pub use worker::execution::run_background_worker;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use worker::pending_prompt::pending_prompt_transcript_state_in_messages;
pub(crate) use worker::pending_prompt::PendingPromptTranscriptState;
// The reaper's claim predicate and the supervisor's process half, which the
// worker uses to claim and the worker's tests assert on.
pub(crate) use ipc::*;
#[cfg(test)]
pub(crate) use rebon_session_host::supervisor::process::*;
#[cfg(test)]
pub(crate) use rebon_session_host::supervisor::reaper::*;
#[cfg(test)]
pub(crate) use rebon_session_host::supervisor::tick::*;
#[cfg(test)]
pub(crate) use worker::execution::*;
pub(crate) use worker::*;

// The two wire<->type conversions the host does on the way in and out.
pub(crate) use agent_view_summary::{
    background_job_agent_view_summary_input_from_store, should_refresh_agent_view_model_summary,
};
pub use runtime_fields::*;
pub use runtime_permissions::*;
pub(crate) use task_bridge::*;

/// Whether a session started for this agent type should be kept out of the
/// chat lists a person browses.
///
/// Followed the worker here: the worker is what stamps the agent type on the
/// job, and the binary reads the answer through its `session` alias like every
/// other host fact. A pure predicate over one string — no state, no I/O.
pub fn should_hide_agent_session_from_chats(agent_type: Option<&str>) -> bool {
    agent_type.is_some_and(|kind| kind.eq_ignore_ascii_case("verification"))
}

/// How long the worker waits for an Agent View summary before giving up.
const BACKGROUND_AGENT_VIEW_SUMMARY_TIMEOUT_MS: u64 = 8_000;

/// How often a waiting worker looks for steering input.
const BACKGROUND_STEER_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// After the parent prompt finishes, keep draining task live events until
/// every registered task is terminal, or this bound elapses. Matches the
/// terminal-worker idle TTL so a stuck LocalAgent cannot pin the bridge
/// longer than the worker process itself is retained for reuse.
const TASK_BRIDGE_POST_PARENT_MAX_MS: u64 = TERMINAL_WORKER_IDLE_TTL_MS;

/// How often that bridge polls while it is winding down.
const TASK_BRIDGE_POLL_INTERVAL_MS: u64 = 100;

/// The store a worker opens when it is told only which job to run.
///
/// The binary resolves its own config home the same way; this is not a second
/// answer, it is the same one asked from the side that now owns the worker.
fn cli_default_store() -> BackgroundStore {
    BackgroundStore::new(crate::rebon_config::config_home_dir())
}

/// The executable a worker re-spawns itself or a sibling from.
///
/// The same `current_exe()` answer the binary gives; it lives here because the
/// worker is what needs it now. The bare-name fallback is never spawned —
/// `resolve_supervisor_exe` refuses a name with no path shape — so an
/// unreadable executable path surfaces as that refusal instead.
fn rebon_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rebon"))
}
