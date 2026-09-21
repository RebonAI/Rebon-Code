use std::fs;
use std::fs::OpenOptions;
use std::time::Duration;

use anyhow::Context;
use fs2::FileExt;

use super::*;
use crate::*;

/// What the embedding binary lends the supervisor loop.
///
/// Injected rather than called directly (reach for an outside capability
/// through a callback). Both of these are the CLI's, not the
/// host's: one runs a plugin, the other shells out to `gh`. A host crate that
/// named either would be reaching across the layer it sits above, and a
/// supervisor that could not run without them would be claiming they are part
/// of supervising, which they are not — every hook is optional and the loop is
/// complete without any of them.
#[derive(Default)]
pub struct SupervisorHooks<'a> {
    /// A one-time migration run before the first tick. Today: the Windows
    /// scheduled-task rewrite, which lives in the updater plugin. `Ok(true)`
    /// when something was migrated.
    pub migration: Option<&'a dyn Fn() -> anyhow::Result<bool>>,
    /// Refresh whatever pull-request status the jobs carry, on a tick where a
    /// client is watching. `Ok(true)` when a job record changed, so the loop
    /// re-reads them.
    pub refresh_pull_requests:
        Option<&'a dyn Fn(&BackgroundStore, &[BackgroundJobState], u64) -> anyhow::Result<bool>>,
}

/// Run the supervisor loop against `store`.
///
/// The whole loop; the binary's hidden `background-supervisor` subcommand is a
/// thin entry that resolves the store and the executable and calls this.
pub fn run_background_supervisor_with_store(
    store: &BackgroundStore,
    hooks: &SupervisorHooks<'_>,
) -> anyhow::Result<()> {
    fs::create_dir_all(store.daemon_dir())?;
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(store.supervisor_lock_path())?;
    if FileExt::try_lock_exclusive(&lock_file).is_err() {
        return Ok(());
    }
    if let Some(migration) = hooks.migration {
        match migration() {
            Ok(true) => {
                store
                    .append_log_line("supervisor", "migrated legacy Windows scheduler launcher")
                    .ok();
            }
            Ok(false) => {}
            Err(err) => {
                store
                    .append_log_line(
                        "supervisor",
                        &format!("failed to migrate legacy Windows scheduler launcher: {err}"),
                    )
                    .ok();
            }
        }
    }
    store.append_log_line("supervisor", "started").ok();
    let _ = store.append_event(
        "supervisor",
        "started",
        serde_json::json!({ "pid": std::process::id() }),
    );
    let supervisor_exe =
        std::env::current_exe().context("failed to locate the running supervisor executable")?;
    let supervisor_exe_modified_at = executable_modified_ms(&supervisor_exe);
    let mut restart_for_updated_binary = false;
    loop {
        if supervisor_binary_changed(&supervisor_exe, supervisor_exe_modified_at) {
            restart_for_updated_binary = true;
            let _ = store.append_event(
                "supervisor",
                "binary_changed_restarting",
                serde_json::json!({ "path": supervisor_exe }),
            );
            break;
        }
        let has_active_jobs = supervisor_tick(store, hooks)?;
        if !has_active_jobs {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    FileExt::unlock(&lock_file).ok();
    if restart_for_updated_binary {
        spawn_supervisor_process(store, &supervisor_exe)?;
    }
    Ok(())
}

pub fn supervisor_tick(
    store: &BackgroundStore,
    hooks: &SupervisorHooks<'_>,
) -> anyhow::Result<bool> {
    let retained_worker_activity_before_spawns = poll_retained_background_worker_children();
    let now = now_ms();
    let active_clients = active_supervisor_clients(store, now)?;
    let mut jobs = store.list_jobs()?;
    recover_orphaned_spawn_admissions(store, &mut jobs)?;
    supervise_terminal_worker_processes(store, &mut jobs, now)?;
    for state in &mut jobs {
        if state.process.status != BackgroundJobStatus::Queued {
            continue;
        }
        // A fenced Queued owner may be a foreground LocalTakeover journal
        // holder, not a stuck background worker. Never route it through the
        // grace-period replacement path (which would terminate the foreground
        // process). Reconcile a dead owner using the durable pid identity; a
        // live owner retains exclusive consumption of pending_prompts.
        if state.process.process_owner_fenced {
            store.reconcile_stale_pid(state)?;
            if state.process.process_owner_fenced {
                continue;
            }
        }
        match queued_spawn_decision(state, now, std::process::id(), process_is_running) {
            QueuedSpawnDecision::DeferToLiveWorker => {}
            QueuedSpawnDecision::ReplaceStuckWorker(pid) => {
                let observed = state.clone();
                if !fence_stuck_queued_worker_for_replacement(store, state)? {
                    continue;
                }
                match terminate_recorded_process_tree(
                    pid,
                    observed.process.pid_identity.as_deref(),
                    observed.process.owner_detached_group,
                    Duration::from_secs(5),
                ) {
                    Ok(()) => {
                        if !clear_fenced_stuck_worker_after_exit(store, state)? {
                            continue;
                        }
                        let _ = store.append_event(
                            &state.identity.job_id,
                            "stuck_worker_replaced",
                            serde_json::json!({ "pid": pid }),
                        );
                        spawn_worker_process(store, state)?;
                    }
                    Err(err) => {
                        let _ = store.append_event(
                            &state.identity.job_id,
                            "stuck_worker_replacement_blocked",
                            serde_json::json!({ "pid": pid, "error": err.to_string() }),
                        );
                    }
                }
            }
            QueuedSpawnDecision::Spawn => {
                if state.process.process_owner_fenced {
                    match fenced_stuck_worker_has_exited(state) {
                        Ok(true) => {
                            if !clear_fenced_stuck_worker_after_exit(store, state)? {
                                continue;
                            }
                        }
                        Ok(false) => continue,
                        Err(err) => {
                            let _ = store.append_event(
                                &state.identity.job_id,
                                "stuck_worker_exit_unverified",
                                serde_json::json!({
                                    "pid": state.process.pid,
                                    "error": err.to_string(),
                                }),
                            );
                            continue;
                        }
                    }
                }
                let _ = spawn_worker_process(store, state);
            }
        }
    }
    if !active_clients.is_empty() {
        match hooks
            .refresh_pull_requests
            .map_or(Ok(false), |refresh| refresh(store, &jobs, now))
        {
            Ok(true) => jobs = store.list_jobs()?,
            Ok(false) => {}
            Err(err) => {
                store
                    .append_log_line("supervisor", &format!("pr status refresh failed: {err}"))
                    .ok();
            }
        }
    }
    let roster = BackgroundRoster {
        supervisor_pid: std::process::id(),
        // Recorded so a reused pid cannot pass for this supervisor once it
        // is gone — the liveness gate would then never start a replacement,
        // and every queued job would wait on a supervisor that does not
        // exist.
        supervisor_pid_identity: crate::process_identity(std::process::id()),
        updated_at_ms: now_ms(),
        jobs: jobs
            .into_iter()
            .map(|job| BackgroundRosterJob {
                job_id: job.identity.job_id,
                session_id: job.identity.session_id,
                cwd: job.identity.cwd,
                status: job.process.status,
                pid: job.process.pid,
                pid_identity: job.process.pid_identity,
                owner_detached_group: job.process.owner_detached_group,
                updated_at_ms: job.process.updated_at_ms,
            })
            .collect(),
    };
    let has_active_jobs = roster.jobs.iter().any(|job| {
        matches!(
            job.status,
            BackgroundJobStatus::Queued
                | BackgroundJobStatus::Running
                | BackgroundJobStatus::NeedsInput
        )
    });
    store.write_roster(&roster)?;
    let retained_worker_activity_after_spawns = poll_retained_background_worker_children();
    Ok(has_active_jobs
        || !active_clients.is_empty()
        || retained_worker_activity_before_spawns
        || retained_worker_activity_after_spawns)
}

pub fn recover_orphaned_spawn_admissions(
    store: &BackgroundStore,
    jobs: &mut [BackgroundJobState],
) -> anyhow::Result<()> {
    for state in jobs.iter_mut().filter(|state| state.process.spawn_admitted) {
        let observed = state.clone();
        let observed_owner = observed.recorded_owner();
        let recovered_at = now_ms();
        let Some(updated) = store.update_state(&observed.identity.job_id, |current| {
            if current.process.status != observed.process.status
                || current.recorded_owner() != observed_owner
                || !current.process.spawn_admitted
                || current.process.updated_at_ms != observed.process.updated_at_ms
            {
                return Ok(None);
            }
            let mut owner = observed_owner.clone();
            if !owner.owner_detached_group {
                if let (Some(pid), Some(identity)) = (owner.pid, owner.pid_identity.as_deref()) {
                    owner.owner_detached_group =
                        matches!(recorded_process_is_running(pid, Some(identity)), Ok(true))
                            && process_owns_detached_group(pid);
                }
            }
            current.set_recorded_owner(owner);
            current.process.spawn_admitted = false;
            current.process.updated_at_ms = recovered_at;
            Ok(Some(current.clone()))
        })?
        else {
            *state = store.read_state(&observed.identity.job_id)?;
            continue;
        };
        *state = updated;
        store.append_event(
            &observed.identity.job_id,
            "orphaned_spawn_admission_recovered",
            serde_json::json!({ "pid": state.process.pid }),
        )?;
        *state = store.read_state(&observed.identity.job_id)?;
    }
    Ok(())
}

/// How long the supervisor lets a live worker sit on a Queued job
/// before treating it as stuck and spawning a replacement. The worker's
/// reuse loop polls every second, so a healthy one reacts well within
/// this window.
pub const QUEUED_LIVE_WORKER_GRACE_MS: u64 = 30_000;

#[derive(Debug, PartialEq, Eq)]
pub enum QueuedSpawnDecision {
    /// No live worker owns the job — spawn one.
    Spawn,
    /// A live worker owns the job; its reuse loop will pick the queued
    /// prompt up. Spawning a second worker would race it for the
    /// session (duplicate MCP stacks, session-file lock contention).
    DeferToLiveWorker,
    /// A live worker owns the job but has ignored it past the grace
    /// window; kill it and spawn a replacement.
    ReplaceStuckWorker(u32),
}

pub fn queued_spawn_decision(
    state: &BackgroundJobState,
    now: u64,
    self_pid: u32,
    is_running: impl Fn(u32) -> Option<bool>,
) -> QueuedSpawnDecision {
    if state.process.spawn_admitted {
        return QueuedSpawnDecision::DeferToLiveWorker;
    }
    let Some(pid) = state.process.pid else {
        return QueuedSpawnDecision::Spawn;
    };
    // Legacy in-process detached sessions record the host process's own
    // pid without a worker existing — never treat that as a worker.
    if pid == self_pid {
        return QueuedSpawnDecision::Spawn;
    }
    if is_running(pid) != Some(true) {
        return if state.process.owner_detached_group {
            QueuedSpawnDecision::ReplaceStuckWorker(pid)
        } else {
            QueuedSpawnDecision::Spawn
        };
    }
    if now.saturating_sub(state.process.updated_at_ms) >= QUEUED_LIVE_WORKER_GRACE_MS {
        return QueuedSpawnDecision::ReplaceStuckWorker(pid);
    }
    QueuedSpawnDecision::DeferToLiveWorker
}

pub fn fenced_stuck_worker_has_exited(state: &BackgroundJobState) -> anyhow::Result<bool> {
    let Some(pid) = state.process.pid else {
        return Ok(true);
    };
    if pid == std::process::id() {
        return Ok(false);
    }
    Ok(!recorded_process_tree_is_running(
        pid,
        state.process.pid_identity.as_deref(),
        state.process.owner_detached_group,
    )?)
}

pub fn clear_fenced_stuck_worker_after_exit(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
) -> anyhow::Result<bool> {
    let observed = state.clone();
    let observed_owner = observed.recorded_owner();
    let cleared_at = now_ms();
    let (cleared, updated) = store.update_state(&observed.identity.job_id, |current| {
        if !observed.process.process_owner_fenced
            || current.recorded_owner() != observed_owner
            || current.process.spawn_admitted
            || current.process.status != BackgroundJobStatus::Queued
            || current.process.turn_generation != observed.process.turn_generation
            || current.identity.session_id != observed.identity.session_id
            || current.process.updated_at_ms != observed.process.updated_at_ms
            || current.identity.pending_prompts != observed.identity.pending_prompts
            || current.outcome.pending_permission != observed.outcome.pending_permission
        {
            return Ok((false, current.clone()));
        }
        current.clear_recorded_owner();
        current.process.spawn_admitted = false;
        current.process.updated_at_ms = cleared_at;
        Ok((true, current.clone()))
    })?;
    *state = updated;
    Ok(cleared)
}

pub fn fence_stuck_queued_worker_for_replacement(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
) -> anyhow::Result<bool> {
    let observed = state.clone();
    let observed_owner = observed.recorded_owner();
    let (fenced, updated) = store.update_state(&observed.identity.job_id, |current| {
        if current.recorded_owner() != observed_owner
            || current.process.spawn_admitted
            || current.process.status != BackgroundJobStatus::Queued
            || current.process.turn_generation != observed.process.turn_generation
            || current.identity.session_id != observed.identity.session_id
            || current.process.updated_at_ms != observed.process.updated_at_ms
            || current.identity.pending_prompts != observed.identity.pending_prompts
            || current.outcome.pending_permission != observed.outcome.pending_permission
        {
            return Ok((false, current.clone()));
        }
        current.set_recorded_owner(observed_owner.clone().fenced(true));
        current.outcome.pending_permission = None;
        Ok((true, current.clone()))
    })?;
    *state = updated;
    Ok(fenced)
}

pub fn supervise_terminal_worker_processes(
    store: &BackgroundStore,
    jobs: &mut [BackgroundJobState],
    now: u64,
) -> anyhow::Result<()> {
    for state in jobs {
        let Some(pid) = state.process.pid else {
            continue;
        };
        if !matches!(
            state.process.status,
            BackgroundJobStatus::Idle
                | BackgroundJobStatus::Succeeded
                | BackgroundJobStatus::Failed
                | BackgroundJobStatus::Stopped
        ) {
            continue;
        }
        let owner_running = if pid == std::process::id() {
            true
        } else {
            match recorded_process_tree_is_running(
                pid,
                state.process.pid_identity.as_deref(),
                state.process.owner_detached_group,
            ) {
                Ok(running) => running,
                Err(err) => {
                    let _ = store.append_event(
                        &state.identity.job_id,
                        "terminal_process_identity_unverified",
                        serde_json::json!({ "pid": pid, "error": err.to_string() }),
                    );
                    continue;
                }
            }
        };
        if !owner_running {
            if !state.process.owner_detached_group {
                clear_terminal_worker_process_refs(store, state, "terminal_process_exited", now)?;
                continue;
            }
            let observed = state.clone();
            if !fence_terminal_worker_for_cleanup(store, state)? {
                continue;
            }
            if let Err(err) = terminate_recorded_process_tree(
                pid,
                observed.process.pid_identity.as_deref(),
                true,
                Duration::from_secs(5),
            ) {
                let _ = store.append_event(
                    &state.identity.job_id,
                    "terminal_process_group_stop_blocked",
                    serde_json::json!({ "pid": pid, "error": err.to_string() }),
                );
                continue;
            }
            clear_terminal_worker_process_refs(store, state, "terminal_process_group_exited", now)?;
            continue;
        }
        let idle_since = state
            .process
            .completed_at_ms
            .unwrap_or(state.process.updated_at_ms);
        if now.saturating_sub(idle_since) < TERMINAL_WORKER_IDLE_TTL_MS {
            continue;
        }
        // Idle to the record, but not to the terminal sitting on it: a
        // client renewing a lease is what "somebody is watching" looks like
        // from here, and the worker's own linger already defers to it. The
        // reaper is the backstop for a worker that forgot to leave, not for
        // one that was asked to stay.
        if state.has_live_client_lease(now) {
            continue;
        }
        let observed = state.clone();
        if !fence_terminal_worker_for_cleanup(store, state)? {
            continue;
        }
        if let (Some(port), Some(token)) = (
            observed.process.ipc_port,
            observed.process.ipc_token.clone(),
        ) {
            let _ = send_background_ipc_request(
                &observed,
                port,
                token,
                BackgroundIpcRequest::cancel_for(&observed),
            );
        }
        if pid != std::process::id() {
            if let Err(err) = terminate_recorded_process_tree(
                pid,
                observed.process.pid_identity.as_deref(),
                observed.process.owner_detached_group,
                Duration::from_secs(5),
            ) {
                let _ = store.append_event(
                    &state.identity.job_id,
                    "idle_process_stop_blocked",
                    serde_json::json!({ "pid": pid, "error": err.to_string() }),
                );
                continue;
            }
        }
        clear_terminal_worker_process_refs(store, state, "idle_process_stopped", now)?;
    }
    Ok(())
}

pub fn fence_terminal_worker_for_cleanup(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
) -> anyhow::Result<bool> {
    let observed = state.clone();
    let observed_owner = observed.recorded_owner();
    let job_id = state.identity.job_id.clone();
    let (fenced, updated) = store.update_state(&job_id, |current| {
        if current.recorded_owner() != observed_owner
            || current.process.spawn_admitted
            || current.process.status != observed.process.status
            || current.identity.session_id != observed.identity.session_id
            || current.process.updated_at_ms != observed.process.updated_at_ms
            || current.identity.pending_prompts != observed.identity.pending_prompts
            || current.outcome.pending_permission != observed.outcome.pending_permission
            || !matches!(
                current.process.status,
                BackgroundJobStatus::Idle
                    | BackgroundJobStatus::Succeeded
                    | BackgroundJobStatus::Failed
                    | BackgroundJobStatus::Stopped
            )
        {
            return Ok((false, current.clone()));
        }
        current.set_recorded_owner(observed_owner.clone().fenced(true));
        current.outcome.pending_permission = None;
        Ok((true, current.clone()))
    })?;
    *state = updated;
    Ok(fenced)
}

pub fn clear_terminal_worker_process_refs(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    event_kind: &str,
    now: u64,
) -> anyhow::Result<()> {
    let observed = state.clone();
    let observed_owner = observed.recorded_owner();
    let job_id = state.identity.job_id.clone();
    let (cleared, updated) = store.update_state(&job_id, |current| {
        if current.recorded_owner() != observed_owner
            || current.process.spawn_admitted
            || current.process.status != observed.process.status
            || current.identity.session_id != observed.identity.session_id
            || current.process.updated_at_ms != observed.process.updated_at_ms
        {
            return Ok((false, current.clone()));
        }
        current.clear_recorded_owner();
        current.process.spawn_admitted = false;
        current.outcome.pending_permission = None;
        current.process.updated_at_ms = now;
        Ok((true, current.clone()))
    })?;
    *state = updated;
    if !cleared {
        return Ok(());
    }
    store.append_event(
        &job_id,
        event_kind,
        serde_json::json!({
            "pid": observed_owner.pid,
        }),
    )?;
    Ok(())
}
