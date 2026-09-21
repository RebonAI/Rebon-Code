use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use rebon_core::permission::PermissionAnswer;

mod attach_job;
mod client;
mod foreground_mailbox;
mod hosted_wait;
mod mirror_turn;
mod pull_requests;

/// Which kind a permission option is. One table, shared with the owner's own
/// half, so the two cannot read the same field differently.
pub(crate) use crate::session::host::permission_option_kind;
pub(crate) use attach_job::{
    attach_background_job_in_store, attach_background_job_in_store_with_supervisor,
    mirrorable_background_job_in_store, recover_lost_worker_in_store,
    worker_given_elsewhere_in_store, LostWorkerRecovery,
};
pub(crate) use foreground_mailbox::{
    command_was_already_resolved, validate_question_answer, ForegroundMailbox,
};
pub(crate) use hosted_wait::{
    probe_hosted_wait, settle_hosted_wait, HostedWaitProbe, HostedWaitReport,
};
// The cadence and the budgets are only asserted on now; the poll reads them
// on the session side.
#[cfg(test)]
pub(crate) use hosted_wait::{
    hosted_probe_interval, HOSTED_PROBE_EAGER_FOR, HOSTED_PROBE_INTERVAL,
    HOSTED_PROBE_INTERVAL_EAGER, HOSTED_STARTUP_SLOW_NOTICE, HOSTED_WAIT_GIVE_UP_AFTER,
};
pub(crate) use mirror_turn::{
    apply_owner_hello, apply_owner_status, apply_owner_turn, background_owner_is_still_recorded,
    clear_remote_turn_projection, cover_settled_turn_entries, drain_owner_words,
    last_remote_user_turn_uuid, merge_local_system_rows, persisted_remote_turn, projected_tool_ids,
    projection_covers_persisted_turn, prune_stale_coverage, read_remote_session_updates_from_store,
    remote_turn_is_absorbed, sync_turn_with_record, transcript_fingerprint,
    update_targets_settled_tool, MergeStreamingContext, OwnerStatusPresentation, OwnerTurnOutcome,
    OwnerWord, PersistedRemoteTurn,
};
use pull_requests::refresh_stale_pull_request_statuses;
// The runtime-field conversions live with the session runtime; re-exported so
// the module's own callers and tests reach them by the name they always used.
pub use crate::session::host::runtime_fields::RuntimeFieldsExt;

#[cfg(test)]
pub(crate) use crate::session::host::ipc::commands::WorkerCommandInputs;

pub(crate) use crate::session::host::ipc::commands::is_compact_command;
pub(crate) use crate::session::host::ipc::commands::permission_retry_blocked_by_running_turn;
pub(crate) use crate::session::host::worker::execution::run_background_worker;
pub(crate) use client::*;
/// The hidden `background-supervisor` subcommand.
///
/// All this does is resolve what only the binary knows — its own store, its own
/// executable — and lend the loop the two things that are the CLI's rather than
/// the host's: the updater plugin's one-time Windows scheduler migration, and
/// the `gh`-backed pull-request status refresh. The loop itself lives in
/// `rebon-session-host`, and this is the shape design §11 asks the
/// hidden subcommands to end up in.
pub(crate) fn run_background_supervisor() -> anyhow::Result<()> {
    let store = cli_default_store();
    let rebon_exe = std::env::current_exe()
        .context("failed to locate the rebon executable for the background supervisor")?;
    let migrate = || {
        rebon_plugin_updater::cli::migrate_legacy_supervisor_service(&rebon_exe, store.root())
            .map_err(anyhow::Error::new)
    };
    let refresh = |store: &BackgroundStore, jobs: &[BackgroundJobState], now: u64| {
        refresh_stale_pull_request_statuses(store, jobs, now)
    };
    rebon_session_host::run_background_supervisor_with_store(
        &store,
        &rebon_session_host::SupervisorHooks {
            migration: Some(&migrate),
            refresh_pull_requests: Some(&refresh),
        },
    )
}

pub(crate) use crate::session::host::should_hide_agent_session_from_chats;

/// A worker's endpoint, for a test that needs to stand one up.
///
/// `serve`'s translation layer is exercised against a
/// real socket, a real session lock and a real event stream rather than a
/// mock of them — the same shape the desktop app's `stub_owner` uses — and
/// that test lives in `crate::serve`, not here.
#[cfg(test)]
pub(crate) use crate::session::host::ipc::server::{
    start_background_ipc_server, BackgroundIpcServer,
};

// =====================================================================
// Facade: the framework-agnostic CLIENT + STORAGE layer now lives in the
// `rebon-session-host` library crate (so a separate GPUI app can depend on
// it). This module keeps the binary-coupled worker loop, supervisor loop,
// re-exports / thin-wraps the moved layer so every existing
// `crate::background::*` call site keeps resolving unchanged.
// =====================================================================
use rebon_session_host::{
    background_job_transcript_cwd, send_background_ipc_request, shorten_excerpt,
};
pub use rebon_session_host::{
    // client fns whose external (store/job-based) signature is unchanged:
    detach_background_job_from_parent,
    mark_existing_background_session_idle,
    stop_background_job_tree_in_store,
    BackgroundDispatchPrompt,
    BackgroundImageAttachment,
    BackgroundIpcRequest,
    BackgroundJobState,
    BackgroundJobStatus,
    BackgroundLaunchOptions,
    BackgroundPermissionQuerySnapshot,
    BackgroundPullRequestDotStatus,
    BackgroundPullRequestStatus,
    BackgroundRuntimeFields,
    BackgroundStore,
    StoppedJobTree,
};

const PULL_REQUEST_STATUS_REFRESH_INTERVAL_MS: u64 = 5 * 60 * 1000;
const PULL_REQUEST_STATUS_REFRESH_TIMEOUT_MS: u64 = 4_000;

// =====================================================================
// CLI-side context injected into the framework-agnostic client layer.
// =====================================================================

/// The rebon-cli default store, rooted at the rebon config home. The
/// library no longer has a `BackgroundStore::default()` (it cannot reach
/// `rebon_config`); rebon-cli supplies the config home here.
pub(crate) fn cli_default_store() -> BackgroundStore {
    BackgroundStore::new(crate::rebon_config::config_home_dir())
}

/// The path used to (re)spawn the background supervisor. Inside the rebon
/// binary this is the running executable; the library takes it as a param
/// so it never resolves `current_exe()` itself.
///
/// Called mid-session whenever a job is launched. The bare-name fallback is
/// not spawned: `resolve_supervisor_exe` refuses a name with no path shape,
/// so an unreadable executable path surfaces as that refusal instead.
pub(crate) fn rebon_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rebon"))
}

// =====================================================================
// Thin facade wrappers preserving the OLD external signatures, injecting
// the store / exe / permission gate the library now takes as params.
// =====================================================================

pub fn launch_background_prompt(
    store: &BackgroundStore,
    options: BackgroundLaunchOptions,
) -> anyhow::Result<BackgroundJobState> {
    rebon_session_host::launch_background_prompt(
        store,
        options,
        &rebon_exe(),
        crate::session::host::runtime_permissions::ensure_background_runtime_permission_mode_allowed,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn create_attached_background_job(
    store: &BackgroundStore,
    prompt: String,
    images: Vec<BackgroundImageAttachment>,
    cwd: PathBuf,
    runtime: BackgroundRuntimeFields,
    session_id: String,
    agent_type: Option<String>,
) -> anyhow::Result<BackgroundJobState> {
    rebon_session_host::create_attached_background_job(
        store,
        prompt,
        images,
        cwd,
        runtime,
        session_id,
        agent_type,
        crate::session::host::runtime_permissions::ensure_background_runtime_permission_mode_allowed,
    )
}

pub fn adopt_existing_background_session(
    store: &BackgroundStore,
    prompt: String,
    cwd: PathBuf,
    runtime: BackgroundRuntimeFields,
    session_id: String,
    name: Option<String>,
    start_supervisor: bool,
    placement: rebon_session_host::JobPlacement,
) -> anyhow::Result<BackgroundJobState> {
    rebon_session_host::adopt_existing_background_session(
        store,
        prompt,
        cwd,
        runtime,
        session_id,
        name,
        start_supervisor,
        placement,
        &rebon_exe(),
    )
}

pub fn queue_background_job(job_id: &str) -> anyhow::Result<()> {
    rebon_session_host::queue_background_job(&cli_default_store(), job_id, &rebon_exe())
}

pub fn reply_to_background_job(job_id: &str, message: String) -> anyhow::Result<()> {
    rebon_session_host::reply_to_background_job(&cli_default_store(), job_id, message, &rebon_exe())
}

pub fn reply_to_background_job_with_images(
    job_id: &str,
    message: String,
    images: Vec<BackgroundImageAttachment>,
) -> anyhow::Result<()> {
    rebon_session_host::reply_to_background_job_in_store_with_images(
        &cli_default_store(),
        job_id,
        message,
        images,
        true,
        true,
        &rebon_exe(),
    )
}

/// Cancel a job's in-flight turn. The implementation lives on `BackgroundStore`
/// so the desktop supervisor reaches the same one — it used to have no route to
/// turn cancellation at all and stopped the whole job instead.
pub fn cancel_background_job_turn(job_id: &str) -> anyhow::Result<bool> {
    cli_default_store().cancel_background_job_turn(job_id)
}

pub fn cancel_background_job_tasks(job_id: &str, task_ids: Vec<String>) -> anyhow::Result<bool> {
    cli_default_store().cancel_background_tasks(job_id, task_ids)
}

pub fn reply_to_background_task(
    job_id: &str,
    task_id: String,
    message: String,
) -> anyhow::Result<()> {
    cli_default_store().reply_to_background_task(job_id, task_id, message)
}

pub fn warm_background_job_for_peek(job_id: &str) -> anyhow::Result<bool> {
    rebon_session_host::warm_background_job_for_peek(&cli_default_store(), job_id, &rebon_exe())
}

pub fn respawn_background_job(job_id: &str) -> anyhow::Result<BackgroundJobState> {
    rebon_session_host::respawn_background_job(&cli_default_store(), job_id, &rebon_exe())
}

pub fn respawn_all_background_jobs() -> anyhow::Result<Vec<BackgroundJobState>> {
    rebon_session_host::respawn_all_background_jobs(&cli_default_store(), &rebon_exe())
}

pub fn keep_supervisor_alive_for_agent_view() {
    rebon_session_host::keep_supervisor_alive_for_agent_view(&cli_default_store(), &rebon_exe());
}

pub fn refresh_agent_view_supervisor_heartbeat() {
    rebon_session_host::refresh_agent_view_supervisor_heartbeat(&cli_default_store(), &rebon_exe());
}

pub fn print_background_logs(job_id: &str, max_lines: usize) -> anyhow::Result<()> {
    rebon_session_host::print_background_logs(&cli_default_store(), job_id, max_lines)
}

pub fn stop_background_job(job_id: &str) -> anyhow::Result<rebon_session_host::StoppedJobTree> {
    rebon_session_host::stop_background_job(&cli_default_store(), job_id)
}

pub fn remove_background_job(job_id: &str) -> anyhow::Result<()> {
    rebon_session_host::remove_background_job(&cli_default_store(), job_id)
}

pub fn resolve_background_dispatch_prompt_with_skills(
    raw_prompt: &str,
    cwd: &Path,
    skills: &[String],
) -> BackgroundDispatchPrompt {
    rebon_session_host::resolve_background_dispatch_prompt_with_skills(
        raw_prompt,
        cwd,
        skills,
        &crate::rebon_config::config_home_dir(),
    )
}

#[cfg(test)]
mod tests;
