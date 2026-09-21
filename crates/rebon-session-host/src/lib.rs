//! The session host: what owns a running session, and what talks to one.
//!
//! The crate grew out of data types for background jobs into the shared half of
//! a session host: the persistent job state, the store transactions over it,
//! the localhost IPC protocol ([`BackgroundIpcEnvelope`] /
//! [`BackgroundIpcRequest`]), the owner/lease state machine, and the one client
//! every endpoint uses to reach a host ([`SessionHostClient`]). Nothing about
//! the wire or the disk changed with that growth, and the `Background*` type
//! names stay: they are compatibility names for a format other builds read, and
//! renaming them is a wire-compat decision, not a refactor.
//!
//! What is here:
//!
//! - **state** — the on-disk JSON under `~/.rebon/jobs/<id>/` (state + events)
//!   and `~/.rebon/daemon/roster.json`, pinned byte-for-byte by
//!   `wire_contract_tests`.
//! - **store** — [`BackgroundStore`], the transactions that keep the job record
//!   consistent across processes.
//! - **protocol** — the request/response/event wire, one internal control plane
//!   and no second one.
//! - **client** — [`SessionHostClient`] and [`SessionHostConnection`], the owner
//!   four states, the lease, the subscription and the prompt ladder.
//! - **legacy foreground** — the file mailbox a local terminal host is still
//!   reached through. Kept indefinitely: it is the only way
//!   to command a `rebon` that holds the lock without publishing an endpoint.
//! - **supervisor** — spawn admission, election, reaping and binary
//!   replacement.
//! - **session hosting and attach** — giving a session a worker
//!   ([`start_hosted_session`], [`host_existing_session`]), finding the job a
//!   session lives in ([`home_job_for_session`]), and attaching to it
//!   ([`attach_background_job_in_store`]): what every endpoint that hosts
//!   sessions headlessly calls, so there is one implementation of it.
//!
//! What is *not* here: the worker loop, the IPC server and the permission
//! forwarder live in `rebon-session-runtime`, which assembles a session and so
//! sits above this crate. This crate is the shared half, not the whole host.

use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};

mod attach_job;
pub mod client;
mod day;
mod dispatch_prompt;
mod executable_quiesce;
mod existing_session;
mod global_stats;
/// The file mailbox a local terminal host is still commanded through. Kept
/// indefinitely, and named for what it is: it predates the
/// endpoint and is not wired to worker IPC.
mod legacy_foreground;
mod process_liveness;
pub mod protocol;
mod respawned_job;
/// The `_session/*` ACP extension: the wire types the internal control plane
/// speaks once it is JSON-RPC. Namespaced rather than re-exported flat,
/// because its names (`method`, `NoParams`, `StatusResult`) only mean anything
/// next to the method they belong to.
pub mod session_ext;
mod session_hosting;
pub mod state;
mod stop_job;
pub mod store;
mod subagent_frontmatter;
mod summary;
/// The supervisor loop: spawn admission, election, process reaping and binary
/// replacement. It has no dependency on the
/// endpoint at all.
pub mod supervisor;
mod supervisor_clients;
mod transcript_preview;
mod warm_peek;
/// The compatibility goldens that must not break. Tests only.
#[cfg(test)]
mod wire_contract_tests;
mod worktree_path;

pub use attach_job::{
    attach_background_job_in_store, mirrorable_background_job_in_store, AttachedJob,
    BackgroundAttachMode,
};
pub use day::{
    civil_from_days, current_local_day_number, date_from_day_number, day_number_from_date,
    day_number_from_ms, local_day_number, local_utc_offset_seconds,
};
pub use dispatch_prompt::resolve_background_dispatch_prompt_with_skills;
pub use executable_quiesce::{
    pids_running_executable, quiesce_roster_processes_running_executable,
    terminate_processes_running_executable, ExecutableQuiesceReport,
};
pub use existing_session::mark_existing_background_session_idle;
pub use global_stats::{
    collect_global_stats, load_cached_default_global_stats, refresh_default_global_stats,
    refresh_default_global_stats_if_stale, DailyTokenActivity, GlobalStats, ModelUsageStats,
};
pub use legacy_foreground::{
    ask_user_questions_from_permission, build_ask_user_question_updated_input,
    clear_foreground_status, complete_foreground_command, drain_foreground_commands,
    foreground_command_response_path, foreground_inbox_dir, foreground_status_path,
    read_foreground_command_response, read_foreground_status, run_foreground_command,
    send_foreground_command, write_foreground_command_response, write_foreground_status,
    ForegroundCommand, ForegroundCommandClaim, ForegroundCommandEnvelope, ForegroundCommandOutcome,
    ForegroundCommandResponse, ForegroundCommandResult, ForegroundQuestion,
    ForegroundQuestionAnswer, ForegroundQuestionOption, ForegroundSessionStatus,
    FOREGROUND_PROTOCOL_VERSION, FOREGROUND_STATUS_SUFFIX,
};
// `client::owner` and `client::session_host_client` keep their old crate-root
// paths, so grouping them under `client/` moved no caller.
pub use client::{
    acp_link, acp_subscription, owner, protocol_probe, session_host_client, stream_watermark,
    wire_errors,
};
pub use owner::{
    foreground_status_from_owner, resolve_owner, AnswerOutcome, LeaseGuard, OwnerCache,
    OwnerHandle, OwnerState, SessionEventStream, SessionOptionAppliesFrom, SessionStreamCloser,
};
pub use stream_watermark::{drain_updates, event_log_line_stamp, DrainOutcome, StreamWatermark};
// The three modules, re-exported flat so that every path outside this crate is
// the one it always was. Nothing moved on the wire or on disk; only which file
// an item is declared in changed.
pub use protocol::*;
pub use state::*;
pub use store::*;

// Items that were private to the old single file and are still crate-internal,
// re-exported here so that `use super::…` from the leaf modules resolves at the
// crate root exactly as it did before the cut. Widening these to `pub(crate)`
// is the visibility the split costs; nothing here is public API.
pub(crate) use state::{session_update_event_data, validate_permission_target};
pub(crate) use store::{
    ensure_background_job_has_no_unfinished_follow_up, ensure_background_job_ownership_reusable,
    read_tail_lines_bounded,
};

pub use process_liveness::{
    process_executable_path, process_identity, process_is_running, process_owns_detached_group,
    recorded_process_is_running, recorded_process_tree_is_running, terminate_process,
    terminate_process_best_effort, terminate_recorded_process, terminate_recorded_process_tree,
    wait_for_pid_exit, wait_for_recorded_process_exit,
};
pub(crate) use respawned_job::{
    create_respawned_job_with_id, validate_respawned_job_materialization,
};
pub use session_host_client::{
    CallIds, HostCallError, HostReply, PermissionAnswer, PermissionOutcome, PromptDelivery,
    SessionHostClient, SessionHostConnection,
};
pub use session_hosting::{
    home_job_for_session, host_existing_session, queue_session_for_worker, session_job_name,
    spawn_queued_worker_now, start_hosted_session, HostedStartup,
};
pub use stop_job::{
    detach_background_job_from_parent, release_background_job_for_local_takeover,
    stop_background_job_in_store, stop_background_job_tree_in_store, StoppedJobTree,
};
// The supervisor's surface, flat at the crate root like everything else here.
// `supervisor::*` itself stays public for the pieces only its own tests reach.
use subagent_frontmatter::apply_subagent_frontmatter_to_runtime;
pub use summary::{
    background_job_failure_summary, background_job_success_summary,
    background_job_success_summary_from_store, summarize_session_update,
};
pub use supervisor::binary::{executable_modified_ms, supervisor_binary_changed};
pub use supervisor::process::spawn_worker_process;
pub use supervisor::reaper::{BackgroundWorkerReaperHandle, BackgroundWorkerReaperStartError};
pub use supervisor::tick::{
    run_background_supervisor_with_store, supervisor_tick, SupervisorHooks,
};
pub use supervisor_clients::active_supervisor_clients;
use transcript_preview::read_transcript_preview;
pub use transcript_preview::{
    background_job_transcript_cwd, background_job_transcript_path_in,
    preserved_background_worktree_path, shorten_excerpt, shorten_labelled_excerpt,
};
pub use warm_peek::warm_background_job_for_peek_in_store;
pub use worktree_path::{
    background_worktree_slug, is_background_worktree_dir_name, should_skip_background_worktree,
};

const SUPERVISOR_CLIENT_TTL_MS: u64 = 15_000;
const SUPERVISOR_CLIENT_HEARTBEAT_INTERVAL_MS: u64 = 2_000;
pub const BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS: u64 = 15_000;
pub const MAX_PENDING_PROMPTS: usize = 32;
/// Upper bound on remembered coordinator report grants. One entry per
/// worker report path; a long coordinator run stays bounded and drops
/// its oldest grants first.
pub const MAX_COORDINATOR_REPORT_GRANTS: usize = 512;
/// Error recorded when a live-looking job is reconciled because its worker
/// process turned out to be gone. Consumers (e.g. desktop notifications) use
/// this sentinel to tell a discovered stale crash apart from a completion that
/// genuinely happened while they were watching.
pub const STALE_PID_EXIT_ERROR: &str = "process exited without updating job state";
/// How long a worker whose job has gone terminal is kept alive for reuse.
///
/// Host policy, not the terminal's: the supervisor is what reaps on it. The CLI
/// still reads it, because the
/// task bridge bounds its post-parent drain by the same hour — a stuck
/// sub-agent must not pin the bridge for longer than the worker process is
/// retained for.
pub const TERMINAL_WORKER_IDLE_TTL_MS: u64 = 60 * 60 * 1000;

static AGENT_VIEW_HEARTBEAT_LAST_MS: AtomicU64 = AtomicU64::new(0);

fn is_false(value: &bool) -> bool {
    !*value
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn generate_job_id() -> String {
    let mut buf = [0u8; 4];
    let _ = getrandom::getrandom(&mut buf);
    let random = u32::from_le_bytes(buf);
    format!("bg-{:x}{:08x}", now_ms(), random)
}

/// A fresh id for one command sent to a session owner.
///
/// The owner remembers the ids it has answered, so a client that lost its
/// connection mid-command retries with the same one and gets the first answer
/// back rather than performing the command twice.
pub fn generate_command_id() -> String {
    let mut buf = [0u8; 8];
    let _ = getrandom::getrandom(&mut buf);
    let random = u64::from_le_bytes(buf);
    format!("cmd-{:x}{:016x}", now_ms(), random)
}

pub fn generate_pending_prompt_id() -> String {
    let mut buf = [0u8; 8];
    let _ = getrandom::getrandom(&mut buf);
    let random = u64::from_le_bytes(buf);
    format!("pp-{:x}{:016x}", now_ms(), random)
}

/// Trim + drop-if-empty (job name / summary helper).
pub fn non_empty_trimmed(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Stable first sentence identifying the persistent supervisor of an Agent Queue.
/// The app emits it in the charter; authorization comes from `queue_session`
/// launch metadata, never from this user-visible text.
pub const AGENT_QUEUE_SUPERVISOR_PROMPT_PREFIX: &str =
    "You are the persistent supervising reviewer for this Agent Queue.";

/// Derive a default job name from the first line of the prompt.
pub fn job_name_from_prompt(prompt: &str) -> String {
    let trimmed = prompt.trim();
    let mut out: String = trimmed.chars().take(80).collect();
    if out.is_empty() {
        out = "background job".to_string();
    }
    out
}

#[cfg(test)]
mod layering {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// The repo root, from this crate's manifest directory.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crates/rebon-session-host sits two levels below the repo root")
            .to_path_buf()
    }

    /// Whether a manifest declares a dependency on `name`. Matches the
    /// dependency key at the start of a line, so a longer name that begins
    /// with a shorter one does not answer for it.
    fn declares(text: &str, name: &str) -> bool {
        text.lines().any(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                return false;
            }
            trimmed
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with(['.', ' ', '=']))
        })
    }

    /// `cli -> rebon-session-runtime -> rebon-session-host`, lower half:
    /// nothing above this crate may be named by it.
    ///
    /// This is the layer a session owner and its clients both speak through,
    /// which is only true while it does not reach back up for anything. The
    /// five names below are the layers above it: the terminal binary, the
    /// runtime that assembles a session, the terminal crate, the harness that
    /// builds an engine, and the engine.
    ///
    /// `rebon-core` is the sharpest case: `store.rs` once named
    /// `rebon_core::permission::OutboundPermissionQuery` for one projection,
    /// and that projection was lifted into `rebon-session-runtime`, which
    /// depends on both halves anyway. The query carries a `oneshot::Sender`, so
    /// it is a broker handle inside one process and could not come down here
    /// instead.
    ///
    /// `rebon-proto` is **not** forbidden and must not be added to the
    /// forbidden list: the wire types belong below this crate, and the internal
    /// control plane moves onto them.
    #[test]
    fn the_session_host_names_nothing_above_it() {
        let manifest = repo_root()
            .join("crates")
            .join("rebon-session-host")
            .join("Cargo.toml");
        let text = fs::read_to_string(&manifest).expect("read this crate's manifest");
        let forbidden: Vec<&str> = [
            "rebon-session-runtime",
            "rebon-cli",
            "rebon-tui",
            "rebon-harness",
            "rebon-core",
        ]
        .into_iter()
        .filter(|name| declares(&text, name))
        .collect();
        assert!(
            forbidden.is_empty(),
            "rebon-session-host is the bottom of the session layering and must not \
             depend on what sits above it; it declares {forbidden:?}"
        );
    }
}
