use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Context;

use super::*;
use crate::*;

/// Hands the worker the environment the job was created with.
///
/// Both entries exist for the same reason: a session runs in its own process,
/// and the process that created the job may be a desktop app whose environment
/// the worker does not otherwise share. `PATH` is what tools are found on; the
/// Node runtime is what the plugin plane runs on, and letting the worker
/// re-resolve it could land on a different runtime than the one the app vetted.
pub fn apply_worker_process_environment(command: &mut Command, state: &BackgroundJobState) {
    if let Some(path) = state.process.process_path.as_deref() {
        command.env("PATH", path);
    }
    if let Some(node) = state.process.node_runtime_path.as_deref() {
        command.env(rebon_node_runtime::NODE_EXECUTABLE_ENV, node);
    }
}

#[cfg(unix)]
fn configure_worker_process_group(command: &mut Command) -> bool {
    crate::detach_background_command(command);
    true
}

#[cfg(not(unix))]
fn configure_worker_process_group(_command: &mut Command) -> bool {
    false
}

pub fn complete_early_worker_claim(
    current: &mut BackgroundJobState,
    child_pid: u32,
    child_identity: &Option<String>,
    expected_turn_generation: u64,
    detached_group: bool,
    completed_at: u64,
) -> bool {
    if !state_was_claimed_by_spawned_worker(
        current,
        child_pid,
        child_identity,
        expected_turn_generation,
    ) {
        return false;
    }
    let mut claimed_owner = current.recorded_owner();
    claimed_owner.owner_detached_group = detached_group;
    current.set_recorded_owner(claimed_owner);
    current.process.spawn_admitted = false;
    current.process.updated_at_ms = completed_at;
    true
}

pub fn spawn_worker_process(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
) -> anyhow::Result<()> {
    let expected_owner = state.recorded_owner();
    let expected_updated_at_ms = state.process.updated_at_ms;
    let log = store.open_log_for_append(&state.identity.job_id)?;
    let stderr = log.try_clone()?;
    // A bare `rebon` would resolve against PATH and could start a different
    // build than the supervisor; failing the spawn is the safer answer.
    let exe = std::env::current_exe()
        .context("failed to locate the rebon executable to spawn the worker")?;
    let mut command = Command::new(exe);
    command
        .arg("__background-worker")
        .arg("--job-id")
        .arg(&state.identity.job_id)
        .current_dir(&state.identity.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    apply_worker_process_environment(&mut command, state);
    hide_background_command_window(&mut command);
    // Workers must outlive the terminal and lead a dedicated group containing
    // only their descendants. `setsid` provides both properties; the returned
    // provenance bit is persisted with the exact owner snapshot.
    let detached_group = configure_worker_process_group(&mut command);

    // Admission is durable and non-spawnable. Stop waits for this bit to clear,
    // so a job can never become Stopped/PID-less immediately before its parent
    // creates a worker and falls back to best-effort post-spawn cleanup.
    let job_id = state.identity.job_id.clone();
    let Some(admitted) = store.update_state(&job_id, |current| {
        if current.process.status != BackgroundJobStatus::Queued
            || current.recorded_owner() != expected_owner
            || current.process.spawn_admitted
            || current.process.updated_at_ms != expected_updated_at_ms
        {
            return Ok(None);
        }
        current.process.spawn_admitted = true;
        current.process.updated_at_ms = now_ms();
        Ok(Some(current.clone()))
    })?
    else {
        *state = store.read_state(&job_id)?;
        return Ok(());
    };
    *state = admitted.clone();
    match command.spawn() {
        Ok(mut child) => {
            let child_pid = child.id();
            let child_identity = crate::process_identity(child_pid);
            let now = now_ms();
            let admitted_owner = admitted.recorded_owner();
            // A store error here must not drop a freshly spawned child that
            // nobody else can identify. Preserve the baseline cleanup path
            // while using Task #19's atomic owner claim.
            let updated = match store.update_state(&job_id, |current| {
                if complete_early_worker_claim(
                    current,
                    child_pid,
                    &child_identity,
                    admitted_owner.turn_generation,
                    detached_group,
                    now,
                ) {
                    return Ok(Some(current.clone()));
                }
                if current.process.status != BackgroundJobStatus::Queued
                    || current.recorded_owner() != admitted_owner
                    || !current.process.spawn_admitted
                {
                    return Ok(None);
                }
                current.set_recorded_owner(RecordedOwnerSnapshot::owned(
                    child_pid,
                    child_identity.clone(),
                    detached_group,
                    false,
                    None,
                    None,
                    admitted_owner.turn_generation,
                ));
                current.process.spawn_admitted = false;
                current.outcome.pending_permission = None;
                // The child owns the Queued -> Running claim; preempting it here can strand the worker.
                current.process.status = BackgroundJobStatus::Queued;
                current.process.updated_at_ms = now;
                current.outcome.error = None;
                Ok(Some(current.clone()))
            }) {
                Ok(updated) => updated,
                Err(err) => {
                    // The worker was never recorded, so nothing will ever
                    // stop it but this.
                    let cleanup = terminate_child_after_reaper_start_failure(child);
                    if let Err(cleanup_err) = cleanup {
                        tracing::error!(
                            pid = child_pid,
                            %cleanup_err,
                            "spawned worker could not be recorded and could not be stopped"
                        );
                    }
                    return Err(err).with_context(|| {
                        format!("failed to record the spawned worker for background job {job_id}")
                    });
                }
            };
            let Some(updated) = updated else {
                // The job moved on while this worker was starting, so it is
                // nobody's. Terminate the verified dedicated tree, then reap
                // the exact child handle before returning.
                let cleanup_error = match child_identity.as_deref() {
                    Some(identity) => terminate_recorded_process_tree(
                        child_pid,
                        Some(identity),
                        detached_group,
                        Duration::from_secs(5),
                    )
                    .err(),
                    None => child.kill().err().map(|error| {
                        anyhow::anyhow!("failed to terminate discarded child: {error}")
                    }),
                };
                if cleanup_error.is_some() {
                    // The retained Child handle still identifies the exact spawned
                    // leader even if group verification failed.
                    let _ = child.kill();
                }
                let wait_error = child.wait().err();
                let _ = store.update_state(&job_id, |current| {
                    if current.recorded_owner() == admitted_owner && current.process.spawn_admitted
                    {
                        current.process.spawn_admitted = false;
                        current.process.updated_at_ms = now_ms();
                    }
                    Ok(())
                });
                *state = store.read_state(&job_id)?;
                store.append_event(
                    &job_id,
                    "spawn_discarded_stale",
                    serde_json::json!({
                        "pid": child_pid,
                        "cleanupError": cleanup_error.as_ref().map(ToString::to_string),
                        "waitError": wait_error.as_ref().map(ToString::to_string),
                    }),
                )?;
                if let Some(error) = cleanup_error {
                    return Err(error).context("failed to terminate stale background worker tree");
                }
                if let Some(error) = wait_error {
                    return Err(error).context("failed to reap stale background worker");
                }
                return Ok(());
            };
            *state = updated;
            let spawned_owner = state.recorded_owner();
            if let Err(err) = spawn_background_worker_reaper(child) {
                let failed_at = now_ms();
                let exit_verified = err.exit_verified;
                let reaper_error = format!("failed to start background worker reaper: {err}");
                *state = store.update_state(&job_id, |current| {
                    apply_background_worker_reaper_start_failure(
                        current,
                        &spawned_owner,
                        exit_verified,
                        failed_at,
                        &reaper_error,
                    );
                    Ok(current.clone())
                })?;
                store.append_event(
                    &job_id,
                    "spawn_reaper_failed",
                    serde_json::json!({
                        "pid": child_pid,
                        "error": err.to_string(),
                        "exitVerified": exit_verified,
                    }),
                )?;
                return Err(err).context("failed to start background worker reaper");
            }
            store.append_event(
                &state.identity.job_id,
                "spawned",
                serde_json::json!({
                    "pid": state.process.pid,
                    "detachedGroup": state.process.owner_detached_group,
                }),
            )?;
            Ok(())
        }
        Err(err) => {
            let now = now_ms();
            let admitted_owner = admitted.recorded_owner();
            let updated = store.update_state(&job_id, |current| {
                if current.process.status != BackgroundJobStatus::Queued
                    || current.recorded_owner() != admitted_owner
                    || !current.process.spawn_admitted
                {
                    return Ok(None);
                }
                current.process.status = BackgroundJobStatus::Failed;
                current.clear_recorded_owner();
                current.process.spawn_admitted = false;
                current.outcome.pending_permission = None;
                current.outcome.error = Some(err.to_string());
                current.process.completed_at_ms = Some(now);
                current.process.updated_at_ms = now;
                Ok(Some(current.clone()))
            })?;
            let Some(updated) = updated else {
                *state = store.read_state(&job_id)?;
                store.append_event(
                    &job_id,
                    "spawn_failure_discarded_stale",
                    serde_json::json!({ "error": err.to_string() }),
                )?;
                return Ok(());
            };
            *state = updated;
            store.append_event(
                &state.identity.job_id,
                "spawn_failed",
                serde_json::json!({ "error": err.to_string() }),
            )?;
            Err(err).context("failed to spawn background worker")
        }
    }
}
