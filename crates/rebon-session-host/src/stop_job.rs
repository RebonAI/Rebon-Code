use std::time::Duration;

use anyhow::Context;

use super::{
    now_ms, process_is_running, recorded_process_is_running, send_background_ipc_request,
    terminate_process_best_effort, terminate_recorded_process_tree, BackgroundIpcRequest,
    BackgroundJobState, BackgroundJobStatus, BackgroundStore, RecordedOwnerSnapshot,
};

/// Preflight: confirm we can still reason about the recorded owner process
/// before mutating job state. Legacy states predate `pid_identity`; for them
/// fall back to plain pid liveness (the same degradation `reconcile_stale_pid`
/// applies) instead of refusing forever and leaving the job unstoppable.
fn check_owner_process(pid: u32, identity: Option<&str>) -> anyhow::Result<()> {
    match identity {
        Some(identity) => recorded_process_is_running(pid, Some(identity)).map(|_| ()),
        None => match process_is_running(pid) {
            Some(_) => Ok(()),
            None => anyhow::bail!("could not determine whether process {pid} is running"),
        },
    }
}

/// Terminate a job's exact recorded owner snapshot.
///
/// `owner_detached_group` is durable spawn provenance: only a Rebon-spawned
/// dedicated group may be group-signalled. Legacy owners without identity are
/// limited to best-effort single-pid termination.
fn terminate_owner_process(owner: &RecordedOwnerSnapshot, timeout: Duration) -> anyhow::Result<()> {
    let pid = owner
        .pid
        .ok_or_else(|| anyhow::anyhow!("recorded owner has no pid"))?;
    match owner.pid_identity.as_deref() {
        Some(identity) => terminate_recorded_process_tree(
            pid,
            Some(identity),
            owner.owner_detached_group,
            timeout,
        ),
        None if owner.owner_detached_group => anyhow::bail!(
            "refusing to signal process group {pid} without a recorded leader identity"
        ),
        None => terminate_process_best_effort(pid, timeout),
    }
}

fn wait_for_spawn_admission(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    while state.process.spawn_admitted {
        if started.elapsed() >= Duration::from_secs(5) {
            anyhow::bail!(
                "background job {} is still resolving an admitted worker spawn",
                state.identity.job_id
            );
        }
        std::thread::sleep(Duration::from_millis(10));
        *state = store.read_state(&state.identity.job_id)?;
    }
    Ok(())
}

pub fn release_background_job_for_local_takeover(
    store: &BackgroundStore,
    observed: &BackgroundJobState,
) -> anyhow::Result<BackgroundJobState> {
    let mut current_observed = observed.clone();
    wait_for_spawn_admission(store, &mut current_observed)?;
    let observed = &current_observed;
    let expected_owner = observed.recorded_owner();
    let expected_pid = expected_owner.pid;
    let expected_port = expected_owner.ipc_port;
    let expected_token = expected_owner.ipc_token.clone();
    let expected_spawn_admitted = observed.process.spawn_admitted;
    let expected_session_id = observed.identity.session_id.clone();
    let expected_status = observed.process.status;
    let expected_turn_generation = observed.process.turn_generation;
    let expected_updated_at_ms = observed.process.updated_at_ms;
    let expected_pending_prompts = observed.identity.pending_prompts.clone();
    let expected_pending_permission = observed.outcome.pending_permission.clone();
    if let Some(pid) = expected_pid.filter(|pid| *pid != std::process::id()) {
        check_owner_process(pid, expected_owner.pid_identity.as_deref()).map_err(|err| {
            anyhow::anyhow!(
                "cannot safely take over background job {}: {err}",
                observed.identity.job_id
            )
        })?;
    }
    let fenced_at = now_ms();
    let fenced = store.update_state(&observed.identity.job_id, |current| {
        if current.recorded_owner() != expected_owner
            || current.process.spawn_admitted != expected_spawn_admitted
            || current.identity.session_id != expected_session_id
            || current.process.status != expected_status
            || current.process.updated_at_ms != expected_updated_at_ms
            || current.identity.pending_prompts != expected_pending_prompts
            || current.outcome.pending_permission != expected_pending_permission
        {
            anyhow::bail!(
                "background job {} changed owner while preparing local takeover",
                observed.identity.job_id
            );
        }
        if matches!(
            current.process.status,
            BackgroundJobStatus::Queued
                | BackgroundJobStatus::Running
                | BackgroundJobStatus::NeedsInput
        ) {
            current.process.status = BackgroundJobStatus::Stopped;
            current.process.completed_at_ms = Some(fenced_at);
        }
        current.set_recorded_owner(expected_owner.clone().fenced(expected_pid.is_some()));
        current.process.spawn_admitted = false;
        current.outcome.pending_permission = None;
        current.process.updated_at_ms = fenced_at;
        Ok(current.clone())
    })?;

    if let (Some(port), Some(token)) = (expected_port, expected_token.clone()) {
        let _ = send_background_ipc_request(
            observed,
            port,
            token,
            BackgroundIpcRequest::cancel_for(observed),
        );
    }

    if let Some(pid) = expected_pid.filter(|pid| *pid != std::process::id()) {
        terminate_owner_process(&expected_owner, Duration::from_secs(5)).with_context(|| {
            format!(
                "failed to terminate background worker {pid} for local takeover of {}",
                observed.identity.job_id
            )
        })?;
    }

    let released_at = now_ms();
    let released = store.update_state(&observed.identity.job_id, |current| {
        let expected_fenced_owner = expected_owner.clone().fenced(expected_pid.is_some());
        if current.recorded_owner() != expected_fenced_owner
            || current.process.spawn_admitted
            || current.process.turn_generation != expected_turn_generation
            || current.identity.session_id != expected_session_id
            || current.process.status != fenced.process.status
            || current.identity.pending_prompts != expected_pending_prompts
        {
            anyhow::bail!(
                "background job {} changed owner before local takeover completed",
                observed.identity.job_id
            );
        }
        current.clear_recorded_owner();
        current.process.spawn_admitted = false;
        current.process.updated_at_ms = released_at;
        Ok(current.clone())
    })?;
    store.append_event(
        &observed.identity.job_id,
        "local_takeover_owner_released",
        serde_json::json!({
            "pid": expected_pid,
            "selfPid": expected_pid == Some(std::process::id()),
            "status": fenced.process.status.as_str(),
        }),
    )?;
    Ok(released)
}

/// What stopping a worker did to the work it had started.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StoppedJobTree {
    /// Descendants that were stopped along with the root, deepest last.
    pub stopped_children: Vec<String>,
    /// Descendants that could not be stopped, with the reason. A child that
    /// refuses is reported, never silently left running: "closing the worker
    /// closed its work" has to be true or said out loud.
    pub failed_children: Vec<(String, String)>,
}

/// Cut a job loose from the worker that started it, and report whether it
/// was attached to one.
///
/// This is the opt-out the ownership rule is written around: everything a
/// worker starts is released with it *unless* someone deliberately
/// backgrounds it from inside, which is what `/bg` on that job's session
/// means. After this the job is nobody's — it survives its former parent.
pub fn detach_background_job_from_parent(
    store: &BackgroundStore,
    job_id: &str,
) -> anyhow::Result<bool> {
    let mut had_parent = None;
    store.update_state(job_id, |current| {
        had_parent = current.identity.parent_job_id.take();
        Ok(())
    })?;
    if let Some(parent) = had_parent.as_deref() {
        // The detach is committed at this point — the link is gone from the
        // state, which is what every reader consults. A failed audit line is
        // a lost record, not a lost detach, and reporting it as an error
        // would tell the caller to retry something that already happened
        // (and, in `/bg`, to keep a session attached that no longer is).
        if let Err(err) = store.append_event(
            job_id,
            "detached_from_parent",
            serde_json::json!({ "parentJobId": parent }),
        ) {
            tracing::warn!(%err, job_id, parent, "detach committed but its event was not recorded");
        }
    }
    Ok(had_parent.is_some())
}

/// Stop a job and everything it started.
///
/// A child job exists because it was dispatched from a session already
/// running in this worker — it is part of that worker's work, not a separate
/// errand, so it goes when the worker goes. The exception is explicit: a
/// child whose `parent_job_id` was cleared (`/bg` from inside it) is no
/// longer anyone's, and is left alone.
///
/// The root is stopped first — it is the one the user asked about, and a
/// stopped root cannot dispatch more children while the walk runs. Its
/// children can, right up until they die, so the walk re-lists as it goes
/// and then sweeps again until a full pass finds nothing new.
pub fn stop_background_job_tree_in_store(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
) -> anyhow::Result<StoppedJobTree> {
    // Deliberately not `?`. The root can report a real failure — an owner
    // whose exit could not be verified is the common one — and the children
    // are still running either way. Skipping them because the root complained
    // is how "closing the worker closed its work" quietly stops being true;
    // the error is re-raised after the walk instead.
    let root_result = stop_background_job_in_store(store, state);

    let mut tree = StoppedJobTree::default();
    // Every job whose subtree has been walked. Persisted across sweeps so a
    // job is stopped at most once however many passes it takes to settle.
    let mut visited: std::collections::HashSet<String> =
        std::collections::HashSet::from([state.identity.job_id.clone()]);
    // A job that cut itself loose mid-walk. Remembered so a later sweep does
    // not re-examine (and re-report) the opt-out it already honoured.
    let mut detached: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Bounded: each sweep either stops at least one new job or ends the
    // walk, and the set of jobs is finite — the cap is only a backstop
    // against a store being written to faster than it can be drained.
    const MAX_SWEEPS: usize = 8;
    for sweep in 0..MAX_SWEEPS {
        let mut stopped_this_sweep = 0usize;
        let mut frontier: Vec<String> = visited.iter().cloned().collect();
        while let Some(parent) = frontier.pop() {
            // Re-listed per parent, not once for the whole walk: a child
            // being stopped can dispatch its own children until it actually
            // dies, and a listing taken before that never sees them. Those
            // jobs are this tree's, and a walk that misses them leaves work
            // running that the user asked to stop.
            let jobs = store.list_jobs()?;
            for child in jobs
                .iter()
                .filter(|job| job.identity.parent_job_id.as_deref() == Some(parent.as_str()))
            {
                // A cycle is not reachable through the dispatch paths, but a
                // hand-edited store must not spin here.
                if visited.contains(&child.identity.job_id)
                    || detached.contains(&child.identity.job_id)
                {
                    continue;
                }
                // Re-read: the listing is a snapshot, and stopping needs the
                // owner fields as they are now.
                let mut current = match store.read_state(&child.identity.job_id) {
                    Ok(current) => current,
                    Err(err) => {
                        visited.insert(child.identity.job_id.clone());
                        tree.failed_children
                            .push((child.identity.job_id.clone(), err.to_string()));
                        continue;
                    }
                };
                // The link is verified again inside the stop's own
                // compare-and-set, not just here: `/bg` cutting this job
                // loose between the read and the write is the one documented
                // opt-out, and stopping it anyway would kill the work the
                // user just asked to keep.
                match stop_background_job_as_child(store, &mut current, Some(parent.as_str())) {
                    Ok(ChildStop::Stopped) => {
                        visited.insert(child.identity.job_id.clone());
                        frontier.push(child.identity.job_id.clone());
                        tree.stopped_children.push(child.identity.job_id.clone());
                        stopped_this_sweep += 1;
                    }
                    // Cut loose — by definition no longer this tree's, and
                    // neither is anything below it.
                    Ok(ChildStop::NoLongerOurChild) => {
                        detached.insert(child.identity.job_id.clone());
                    }
                    Err(err) => {
                        visited.insert(child.identity.job_id.clone());
                        // Still descend. This job failed to stop, but the
                        // agents *it* gave workers of their own are a
                        // separate matter — skipping them because their
                        // parent refused is how a failed stop quietly
                        // leaves a whole subtree running.
                        frontier.push(child.identity.job_id.clone());
                        tree.failed_children
                            .push((child.identity.job_id.clone(), err.to_string()));
                    }
                }
            }
        }
        if stopped_this_sweep == 0 {
            break;
        }
        if sweep + 1 == MAX_SWEEPS {
            // Reported, not just logged: the caller is about to tell a user
            // that closing the worker closed its work, and here that is not
            // known to be true.
            tracing::warn!(
                root = %state.identity.job_id,
                "stop tree hit its sweep cap; new children may still be appearing"
            );
            tree.failed_children.push((
                state.identity.job_id.clone(),
                format!(
                    "children were still appearing after {MAX_SWEEPS} sweeps; some may still be running"
                ),
            ));
        }
    }
    root_result?;
    Ok(tree)
}

/// What stopping one child of a tree walk did.
enum ChildStop {
    Stopped,
    /// The job cut itself loose (`/bg`) before the stop could commit, so it
    /// is nobody's now and was deliberately left running.
    NoLongerOurChild,
}

pub fn stop_background_job_in_store(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
) -> anyhow::Result<()> {
    stop_background_job_as_child(store, state, None).map(|_| ())
}

/// [`stop_background_job_in_store`], refusing when the job is no longer the
/// child of `expected_parent`.
///
/// The parent guard is part of each owner mutation: `/bg` wins a race with a
/// parent stop, while the Task #19 owner snapshot keeps stop/takeover linear.
fn stop_background_job_as_child(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    expected_parent: Option<&str>,
) -> anyhow::Result<ChildStop> {
    if !job_still_child_of(state, expected_parent) {
        return Ok(ChildStop::NoLongerOurChild);
    }
    wait_for_spawn_admission(store, state)?;
    if state.process.status == BackgroundJobStatus::Stopped {
        return retry_stopped_worker_termination(store, state, expected_parent);
    }

    let terminal = matches!(
        state.process.status,
        BackgroundJobStatus::Succeeded | BackgroundJobStatus::Failed
    );
    let observed = state.clone();
    let observed_owner = observed.recorded_owner();
    let old_pid = observed_owner.pid;
    if terminal && old_pid.is_none() {
        let current = store.update_state(&observed.identity.job_id, |current| {
            if !job_still_child_of(current, expected_parent) {
                return Ok(None);
            }
            if current.process.status != observed.process.status
                || current.recorded_owner() != observed_owner
                || current.process.spawn_admitted != observed.process.spawn_admitted
                || current.identity.session_id != observed.identity.session_id
                || current.process.updated_at_ms != observed.process.updated_at_ms
            {
                anyhow::bail!(
                    "background job {} changed owner while stopping",
                    observed.identity.job_id
                );
            }
            if current.recorded_owner()
                != RecordedOwnerSnapshot::unowned(current.process.turn_generation)
            {
                current.clear_recorded_owner();
                current.outcome.pending_permission = None;
                current.clear_pending_prompts();
                current.process.updated_at_ms = now_ms();
            }
            Ok(Some(current.clone()))
        })?;
        let Some(current) = current else {
            return Ok(ChildStop::NoLongerOurChild);
        };
        store.append_event(
            &state.identity.job_id,
            "stop_ignored_terminal",
            serde_json::json!({ "status": current.process.status.as_str() }),
        )?;
        *state = store.read_state(&state.identity.job_id)?;
        return Ok(ChildStop::Stopped);
    }

    let is_self_pid = old_pid == Some(std::process::id());
    if let Some(pid) = old_pid.filter(|_| !is_self_pid) {
        check_owner_process(pid, observed_owner.pid_identity.as_deref()).with_context(|| {
            format!(
                "cannot safely stop background job {} because its owner identity is unavailable",
                observed.identity.job_id
            )
        })?;
    }

    let completed_at = now_ms();
    let fenced_owner = observed_owner.clone().fenced(old_pid.is_some());
    let stopped = store.update_state(&observed.identity.job_id, |current| {
        // The opt-out wins the race: a `/bg` that landed since the read
        // means this job is nobody's, and the stop must not commit.
        if !job_still_child_of(current, expected_parent) {
            return Ok(None);
        }
        if current.process.status != observed.process.status
            || current.recorded_owner() != observed_owner
            || current.process.spawn_admitted
            || current.identity.session_id != observed.identity.session_id
            || current.process.updated_at_ms != observed.process.updated_at_ms
        {
            anyhow::bail!(
                "background job {} changed owner while stopping",
                observed.identity.job_id
            );
        }
        if !terminal {
            current.process.status = BackgroundJobStatus::Stopped;
            current.process.completed_at_ms = Some(completed_at);
            current.clear_pending_prompts();
        }
        current.set_recorded_owner(fenced_owner.clone());
        current.process.spawn_admitted = false;
        current.process.updated_at_ms = completed_at;
        current.outcome.pending_permission = None;
        current.clear_pending_prompts();
        Ok(Some(current.clone()))
    })?;
    let Some(stopped) = stopped else {
        return Ok(ChildStop::NoLongerOurChild);
    };
    *state = stopped;

    if let (Some(port), Some(token)) = (observed_owner.ipc_port, observed_owner.ipc_token.clone()) {
        let _ = send_background_ipc_request(
            &observed,
            port,
            token,
            BackgroundIpcRequest::cancel_for(&observed),
        );
    }

    if let Some(pid) = old_pid.filter(|_| !is_self_pid) {
        if let Err(err) = terminate_owner_process(&observed_owner, Duration::from_secs(5)) {
            store.append_event(
                &observed.identity.job_id,
                "stop_process_exit_unverified",
                serde_json::json!({ "pid": pid, "error": err.to_string() }),
            )?;
            return Err(err).with_context(|| {
                format!(
                    "background job {} was fenced but its worker exit was not verified",
                    observed.identity.job_id
                )
            });
        }
    }

    if old_pid.is_some() {
        let cleared_at = now_ms();
        if let Some(current) = store.update_state(&observed.identity.job_id, |current| {
            if current.process.status == state.process.status
                && current.recorded_owner() == fenced_owner
                && current.identity.session_id == observed.identity.session_id
                && !current.process.spawn_admitted
            {
                current.clear_recorded_owner();
                current.process.updated_at_ms = cleared_at;
                return Ok(Some(current.clone()));
            }
            Ok(None)
        })? {
            *state = current;
        }
    }
    store.append_event(
        &observed.identity.job_id,
        if terminal {
            "terminal_owner_stopped"
        } else {
            "stopped"
        },
        serde_json::json!({ "selfPid": is_self_pid }),
    )?;
    *state = store.read_state(&observed.identity.job_id)?;
    Ok(ChildStop::Stopped)
}

/// Whether a job is still the child the caller believed it was.
///
/// `None` means the caller made no claim about parentage, so anything
/// passes; otherwise the link has to match exactly — a cleared or
/// re-pointed `parent_job_id` is the `/bg` opt-out.
fn job_still_child_of(state: &BackgroundJobState, expected_parent: Option<&str>) -> bool {
    match expected_parent {
        None => true,
        Some(parent) => state.identity.parent_job_id.as_deref() == Some(parent),
    }
}

/// A previous stop fenced the job to Stopped but could not verify the worker's
/// exit (`stop_process_exit_unverified`), leaving the pid recorded. Re-read and
/// reserve that exact current owner before signalling; stale callers never act
/// on an owner that has since been replaced.
fn retry_stopped_worker_termination(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    expected_parent: Option<&str>,
) -> anyhow::Result<ChildStop> {
    *state = store.read_state(&state.identity.job_id)?;
    if !job_still_child_of(state, expected_parent) {
        return Ok(ChildStop::NoLongerOurChild);
    }
    wait_for_spawn_admission(store, state)?;
    if !job_still_child_of(state, expected_parent) {
        return Ok(ChildStop::NoLongerOurChild);
    }
    if state.process.status != BackgroundJobStatus::Stopped {
        return Ok(ChildStop::Stopped);
    }

    let observed = state.clone();
    let observed_owner = observed.recorded_owner();
    let Some(pid) = observed_owner.pid else {
        let current = store.update_state(&observed.identity.job_id, |current| {
            if !job_still_child_of(current, expected_parent) {
                return Ok(None);
            }
            if current.process.status != BackgroundJobStatus::Stopped
                || current.recorded_owner() != observed_owner
                || current.process.spawn_admitted
            {
                anyhow::bail!(
                    "background job {} changed owner while retrying stop",
                    observed.identity.job_id
                );
            }
            if current.recorded_owner()
                != RecordedOwnerSnapshot::unowned(current.process.turn_generation)
            {
                current.clear_recorded_owner();
                current.outcome.pending_permission = None;
                current.process.updated_at_ms = now_ms();
            }
            Ok(Some(current.clone()))
        })?;
        let Some(current) = current else {
            return Ok(ChildStop::NoLongerOurChild);
        };
        *state = current;
        return Ok(ChildStop::Stopped);
    };
    let is_self_pid = pid == std::process::id();
    let fenced_owner = observed_owner.clone().fenced(true);
    let Some(fenced) = store.update_state(&observed.identity.job_id, |current| {
        if !job_still_child_of(current, expected_parent) {
            return Ok(None);
        }
        if current.process.status != BackgroundJobStatus::Stopped
            || current.recorded_owner() != observed_owner
            || current.process.spawn_admitted
        {
            return Ok(None);
        }
        current.set_recorded_owner(fenced_owner.clone());
        current.outcome.pending_permission = None;
        Ok(Some(current.clone()))
    })?
    else {
        *state = store.read_state(&observed.identity.job_id)?;
        return if job_still_child_of(state, expected_parent) {
            Ok(ChildStop::Stopped)
        } else {
            Ok(ChildStop::NoLongerOurChild)
        };
    };
    *state = fenced;

    if !is_self_pid {
        terminate_owner_process(&observed_owner, Duration::from_secs(5)).with_context(|| {
            format!(
                "background job {} is marked stopped but its worker exit could not be verified",
                observed.identity.job_id
            )
        })?;
    }

    let cleared_at = now_ms();
    let cleared = store.update_state(&observed.identity.job_id, |current| {
        if current.process.status == BackgroundJobStatus::Stopped
            && current.recorded_owner() == fenced_owner
            && !current.process.spawn_admitted
        {
            current.clear_recorded_owner();
            current.process.updated_at_ms = cleared_at;
            return Ok(Some(current.clone()));
        }
        Ok(None)
    })?;
    if let Some(current) = cleared {
        *state = current;
        store.append_event(
            &observed.identity.job_id,
            "stop_retry_terminated",
            serde_json::json!({ "pid": pid, "selfPid": is_self_pid }),
        )?;
        *state = store.read_state(&observed.identity.job_id)?;
    }
    Ok(ChildStop::Stopped)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{BackgroundPermissionQuerySnapshot, BackgroundRuntimeFields};

    fn store() -> (tempfile::TempDir, BackgroundStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        (dir, store)
    }

    fn runtime() -> BackgroundRuntimeFields {
        BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        }
    }

    #[cfg(windows)]
    fn spawn_long_running_child() -> std::process::Child {
        std::process::Command::new("cmd")
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    #[cfg(unix)]
    fn spawn_long_running_child() -> std::process::Child {
        std::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    fn queued_child(
        store: &BackgroundStore,
        parent: Option<&str>,
        name: &str,
    ) -> BackgroundJobState {
        let mut state = store
            .create_job_with_name(
                "work".into(),
                PathBuf::from("."),
                runtime(),
                Some(name.to_string()),
            )
            .unwrap();
        state.process.status = BackgroundJobStatus::Queued;
        state.identity.parent_job_id = parent.map(str::to_string);
        store.write_state(&state).unwrap();
        state
    }

    /// The ownership rule: what a worker started goes when the worker goes,
    /// however deep it nests. Anything else means closing a worker leaves
    /// agents running that nobody is watching and nobody asked for.
    #[test]
    fn stopping_a_worker_stops_what_it_started_all_the_way_down() {
        let (_dir, store) = store();
        let mut root = queued_child(&store, None, "root");
        let child = queued_child(&store, Some(&root.identity.job_id), "child");
        let grandchild = queued_child(&store, Some(&child.identity.job_id), "grandchild");
        let stranger = queued_child(&store, None, "stranger");

        let tree = stop_background_job_tree_in_store(&store, &mut root).unwrap();

        assert_eq!(root.process.status, BackgroundJobStatus::Stopped);
        assert!(tree.failed_children.is_empty());
        assert_eq!(tree.stopped_children.len(), 2);
        assert!(tree.stopped_children.contains(&child.identity.job_id));
        assert!(tree.stopped_children.contains(&grandchild.identity.job_id));
        for job in [&child.identity.job_id, &grandchild.identity.job_id] {
            assert_eq!(
                store.read_state(job).unwrap().process.status,
                BackgroundJobStatus::Stopped
            );
        }
        assert_eq!(
            store
                .read_state(&stranger.identity.job_id)
                .unwrap()
                .process
                .status,
            BackgroundJobStatus::Queued,
            "a job nobody started must not be collateral"
        );
    }

    /// The opt-out has to actually opt out: a child that was deliberately
    /// backgrounded from inside is nobody's, and outlives its former parent.
    #[test]
    fn a_child_that_was_backgrounded_on_purpose_survives_its_parent() {
        let (_dir, store) = store();
        let mut root = queued_child(&store, None, "root");
        let child = queued_child(&store, Some(&root.identity.job_id), "child");

        assert!(detach_background_job_from_parent(&store, &child.identity.job_id).unwrap());
        assert!(
            !detach_background_job_from_parent(&store, &child.identity.job_id).unwrap(),
            "detaching twice is not an error, it is a no-op"
        );

        let tree = stop_background_job_tree_in_store(&store, &mut root).unwrap();

        assert!(tree.stopped_children.is_empty());
        assert_eq!(
            store
                .read_state(&child.identity.job_id)
                .unwrap()
                .process
                .status,
            BackgroundJobStatus::Queued
        );
    }

    /// Found on a real machine: stopping the root can fail — an owner whose
    /// exit cannot be verified, or a state that changed under the caller —
    /// and the children are running regardless. Letting the root's error skip
    /// them is how the ownership promise quietly stops being kept.
    #[test]
    fn children_are_released_even_when_stopping_the_root_reports_a_failure() {
        let (_dir, store) = store();
        let mut root = queued_child(&store, None, "root");
        let child = queued_child(&store, Some(&root.identity.job_id), "child");
        // Someone else touched the root between the read and the stop, so its
        // compare-and-swap refuses. `root` is now a stale observation.
        let mut concurrent = store.read_state(&root.identity.job_id).unwrap();
        concurrent.process.updated_at_ms += 1;
        store.write_state(&concurrent).unwrap();

        let err = stop_background_job_tree_in_store(&store, &mut root).unwrap_err();

        assert!(
            err.to_string().contains(&root.identity.job_id),
            "the root failure still has to reach the caller: {err}"
        );
        assert_eq!(
            store
                .read_state(&child.identity.job_id)
                .unwrap()
                .process
                .status,
            BackgroundJobStatus::Stopped,
            "the work the root started must be released whatever the root said"
        );
    }

    /// The listing the walk starts from is a snapshot, and `/bg` from inside
    /// a child is the one documented opt-out from its parent's lifetime.
    /// A child cut loose between the listing and its turn in the walk must
    /// be left alone — and so must its own children, which are no longer
    /// this tree's either.
    #[test]
    fn a_child_detached_after_the_listing_is_left_running() {
        let (_dir, store) = store();
        let mut root = queued_child(&store, None, "root");
        let child = queued_child(&store, Some(&root.identity.job_id), "child");
        let grandchild = queued_child(&store, Some(&child.identity.job_id), "grandchild");
        // The `/bg` the walk has not seen yet.
        detach_background_job_from_parent(&store, &child.identity.job_id).unwrap();

        let tree = stop_background_job_tree_in_store(&store, &mut root).unwrap();

        assert!(
            tree.stopped_children.is_empty(),
            "a job cut loose keeps running, and so does everything under it: {tree:?}"
        );
        assert_eq!(
            store
                .read_state(&child.identity.job_id)
                .unwrap()
                .process
                .status,
            BackgroundJobStatus::Queued
        );
        assert_eq!(
            store
                .read_state(&grandchild.identity.job_id)
                .unwrap()
                .process
                .status,
            BackgroundJobStatus::Queued
        );
        assert_eq!(root.process.status, BackgroundJobStatus::Stopped);
    }

    /// A finished job is not a finished process: a worker whose turn ended
    /// stays resident for its idle TTL, keeping its pid and endpoint so a
    /// follow-up prompt can reuse it. Stop reported success and left that
    /// process running — holding the API key, the working directory and the
    /// session it was resumed on.
    #[test]
    fn stopping_a_succeeded_job_ends_its_still_resident_worker() {
        let (_dir, store) = store();
        let mut child = spawn_long_running_child();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Succeeded;
        state.process.completed_at_ms = Some(now_ms());
        state.process.pid = Some(child.id());
        state.process.pid_identity = super::super::process_identity(child.id());
        state.process.ipc_port = Some(40501);
        state.process.ipc_token = Some("resident-worker".into());
        store.write_state(&state).unwrap();

        if let Err(err) = stop_background_job_in_store(&store, &mut state) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("stopping a resident terminal worker failed: {err}");
        }

        let _ = child.wait();
        assert_eq!(super::super::process_is_running(child.id()), Some(false));
        assert_eq!(
            state.process.status,
            BackgroundJobStatus::Succeeded,
            "the work really did succeed; only its worker is gone"
        );
        assert_eq!(state.process.pid, None);
        assert_eq!(state.process.ipc_port, None);
        assert_eq!(state.process.ipc_token, None);
    }

    /// Nothing to terminate is still nothing to terminate: a terminal job
    /// whose worker already exited keeps the old quiet no-op.
    #[test]
    fn stopping_a_finished_job_without_a_worker_stays_a_no_op() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Failed;
        state.process.completed_at_ms = Some(now_ms());
        store.write_state(&state).unwrap();

        stop_background_job_in_store(&store, &mut state).unwrap();

        assert_eq!(state.process.status, BackgroundJobStatus::Failed);
        assert_eq!(state.process.pid, None);
    }

    /// A child dispatched while the walk is running is still the tree's:
    /// the listing that started the walk never saw it, so the walk re-lists
    /// and sweeps again until a pass finds nothing new.
    #[test]
    fn a_child_created_during_the_walk_is_still_stopped() {
        let (_dir, store) = store();
        let mut root = queued_child(&store, None, "root");
        let child = queued_child(&store, Some(&root.identity.job_id), "child");
        // Stand in for a dispatch that lands mid-walk: a grandchild that no
        // listing taken before the walk could have contained. Created here
        // because the walk re-lists per parent, so it is discovered exactly
        // the way a real late dispatch would be.
        let grandchild = queued_child(&store, Some(&child.identity.job_id), "grandchild");

        let tree = stop_background_job_tree_in_store(&store, &mut root).unwrap();

        assert!(
            tree.stopped_children.contains(&child.identity.job_id),
            "{tree:?}"
        );
        assert!(
            tree.stopped_children.contains(&grandchild.identity.job_id),
            "a job dispatched below a child must not survive the tree stop: {tree:?}"
        );
        for job in [&child.identity.job_id, &grandchild.identity.job_id] {
            assert_eq!(
                store.read_state(job).unwrap().process.status,
                BackgroundJobStatus::Stopped
            );
        }
    }

    /// The opt-out has to win the write, not just a check before it: `/bg`
    /// landing between the walk's read and its state mutation must still
    /// leave the job running.
    #[test]
    fn a_detach_that_lands_before_the_write_still_spares_the_child() {
        let (_dir, store) = store();
        let root = queued_child(&store, None, "root");
        let mut child = queued_child(&store, Some(&root.identity.job_id), "child");
        // The read the walk would have done, taken before the detach — this
        // is exactly the stale view the compare-and-set has to reject.
        let observed = child.clone();
        detach_background_job_from_parent(&store, &child.identity.job_id).unwrap();
        child = observed;

        let outcome =
            stop_background_job_as_child(&store, &mut child, Some(root.identity.job_id.as_str()))
                .unwrap();

        assert!(matches!(outcome, ChildStop::NoLongerOurChild));
        assert_eq!(
            store
                .read_state(&child.identity.job_id)
                .unwrap()
                .process
                .status,
            BackgroundJobStatus::Queued,
            "a job cut loose keeps running"
        );
    }

    /// A child that refuses to stop is reported — and its own descendants
    /// are still visited. They are separate jobs with separate workers, and
    /// skipping them because their parent complained is how one failure
    /// silently leaves a whole subtree running.
    #[test]
    fn a_child_that_fails_to_stop_does_not_hide_its_descendants() {
        let (_dir, store) = store();
        let mut root = queued_child(&store, None, "root");
        let mut blocked = queued_child(&store, Some(&root.identity.job_id), "blocked");
        let grandchild = queued_child(&store, Some(&blocked.identity.job_id), "grandchild");
        // A live owner recorded as a process group of its own but without a
        // leader identity: the stop path refuses to signal a group it cannot
        // verify, and reports that rather than guessing. (A pid that does
        // not exist is no refusal — the owner is provably gone, and the job
        // is fenced and cleared like any other dead worker's.)
        let mut worker = spawn_long_running_child();
        blocked.process.pid = Some(worker.id());
        blocked.process.pid_identity = None;
        blocked.process.owner_detached_group = true;
        store.write_state(&blocked).unwrap();

        let tree = stop_background_job_tree_in_store(&store, &mut root).unwrap();
        let _ = worker.kill();
        let _ = worker.wait();

        assert!(
            tree.failed_children
                .iter()
                .any(|(job, _)| job == &blocked.identity.job_id),
            "the refusal is reported: {tree:?}"
        );
        assert!(
            tree.stopped_children.contains(&grandchild.identity.job_id),
            "the subtree under a failed child is still this tree's: {tree:?}"
        );
    }

    /// A hand-edited store must not spin the walk forever.
    #[test]
    fn a_parent_cycle_does_not_hang_the_walk() {
        let (_dir, store) = store();
        let mut first = queued_child(&store, None, "first");
        let second = queued_child(&store, Some(&first.identity.job_id), "second");
        first.identity.parent_job_id = Some(second.identity.job_id.clone());
        store.write_state(&first).unwrap();

        let tree = stop_background_job_tree_in_store(&store, &mut first).unwrap();

        assert_eq!(tree.stopped_children, vec![second.identity.job_id]);
    }

    #[test]
    fn local_takeover_rejects_a_changed_owner_generation() {
        let (_dir, store) = store();
        let mut observed = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        observed.identity.session_id = Some("sess-owner-race".into());
        observed.process.status = BackgroundJobStatus::Running;
        observed.process.turn_generation = 3;
        observed.process.pid = Some(std::process::id());
        observed.process.ipc_port = Some(40001);
        observed.process.ipc_token = Some("old-owner".into());
        store.write_state(&observed).unwrap();
        store
            .update_state(&observed.identity.job_id, |current| {
                current.process.turn_generation = 4;
                current.process.ipc_port = Some(40002);
                current.process.ipc_token = Some("replacement-owner".into());
                current.process.updated_at_ms = current.process.updated_at_ms.saturating_add(1);
                Ok(())
            })
            .unwrap();

        let error = release_background_job_for_local_takeover(&store, &observed).unwrap_err();

        assert!(error.to_string().contains("changed owner"));
        let current = store.read_state(&observed.identity.job_id).unwrap();
        assert_eq!(current.process.status, BackgroundJobStatus::Running);
        assert_eq!(current.process.turn_generation, 4);
        assert_eq!(
            current.process.ipc_token.as_deref(),
            Some("replacement-owner")
        );
    }

    #[test]
    fn local_takeover_waits_for_admitted_spawn_to_publish_before_stopping() {
        let (_dir, store) = store();
        let mut observed = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        observed.identity.session_id = Some("sess-admitted-spawn".into());
        observed.process.status = BackgroundJobStatus::Queued;
        observed.process.spawn_admitted = true;
        store.write_state(&observed).unwrap();

        let parent_store = store.clone();
        let job_id = observed.identity.job_id.clone();
        let parent = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            parent_store
                .update_state(&job_id, |current| {
                    current.set_recorded_owner(RecordedOwnerSnapshot::owned(
                        std::process::id(),
                        super::super::process_identity(std::process::id()),
                        false,
                        false,
                        None,
                        None,
                        current.process.turn_generation,
                    ));
                    current.process.spawn_admitted = false;
                    current.process.updated_at_ms = now_ms();
                    Ok(())
                })
                .unwrap();
        });

        let started = std::time::Instant::now();
        let released = release_background_job_for_local_takeover(&store, &observed).unwrap();
        parent.join().unwrap();

        assert!(started.elapsed() >= Duration::from_millis(40));
        assert_eq!(released.process.status, BackgroundJobStatus::Stopped);
        assert_eq!(released.recorded_owner(), RecordedOwnerSnapshot::unowned(0));
        assert!(!released.process.spawn_admitted);
    }

    #[test]
    fn local_takeover_terminates_and_verifies_a_distinct_owner() {
        let (_dir, store) = store();
        let mut child = spawn_long_running_child();
        let mut observed = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        observed.identity.session_id = Some("sess-live-owner".into());
        observed.process.status = BackgroundJobStatus::Running;
        observed.process.turn_generation = 1;
        observed.process.pid = Some(child.id());
        observed.process.pid_identity = super::super::process_identity(child.id());
        store.write_state(&observed).unwrap();

        let released = match release_background_job_for_local_takeover(&store, &observed) {
            Ok(released) => released,
            Err(err) => {
                let _ = child.kill();
                panic!("local takeover failed: {err}");
            }
        };

        let _ = child.wait();
        assert_eq!(released.process.status, BackgroundJobStatus::Stopped);
        assert_eq!(released.process.pid, None);
        assert_eq!(super::super::process_is_running(child.id()), Some(false));
    }

    #[test]
    fn local_takeover_does_not_kill_a_reused_pid() {
        let (_dir, store) = store();
        let mut child = spawn_long_running_child();
        let mut observed = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        observed.identity.session_id = Some("sess-reused-owner-pid".into());
        observed.process.status = BackgroundJobStatus::Running;
        observed.process.turn_generation = 1;
        observed.process.pid = Some(child.id());
        observed.process.pid_identity = Some("different-process-instance".into());
        store.write_state(&observed).unwrap();

        let released = release_background_job_for_local_takeover(&store, &observed).unwrap();

        assert_eq!(released.process.pid, None);
        assert!(child.try_wait().unwrap().is_none());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn local_takeover_best_effort_stops_live_owner_without_recorded_identity() {
        // Legacy job states predate `pid_identity`. Refusing the takeover left
        // those jobs permanently un-attachable and unstoppable; fall back to a
        // best-effort pid kill instead.
        let (_dir, store) = store();
        let mut child = spawn_long_running_child();
        let mut observed = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        observed.identity.session_id = Some("sess-owner-without-identity".into());
        observed.process.status = BackgroundJobStatus::Running;
        observed.process.turn_generation = 1;
        observed.process.pid = Some(child.id());
        observed.process.pid_identity = None;
        store.write_state(&observed).unwrap();

        let released = match release_background_job_for_local_takeover(&store, &observed) {
            Ok(released) => released,
            Err(err) => {
                let _ = child.kill();
                panic!("legacy local takeover failed: {err}");
            }
        };

        let _ = child.wait();
        assert_eq!(released.process.status, BackgroundJobStatus::Stopped);
        assert_eq!(released.process.pid, None);
        assert_eq!(super::super::process_is_running(child.id()), Some(false));
    }

    #[test]
    fn stop_best_effort_stops_live_owner_without_recorded_identity() {
        let (_dir, store) = store();
        let mut child = spawn_long_running_child();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.identity.session_id = Some("sess-stop-without-identity".into());
        state.process.status = BackgroundJobStatus::Running;
        state.process.pid = Some(child.id());
        state.process.pid_identity = None;
        store.write_state(&state).unwrap();

        if let Err(err) = stop_background_job_in_store(&store, &mut state) {
            let _ = child.kill();
            panic!("legacy stop failed: {err}");
        }

        let _ = child.wait();
        assert_eq!(state.process.status, BackgroundJobStatus::Stopped);
        assert_eq!(state.process.pid, None);
        assert_eq!(super::super::process_is_running(child.id()), Some(false));
    }

    #[test]
    fn stop_retries_termination_when_stopped_job_still_records_a_live_worker() {
        // Shape left behind when a previous stop fenced the job to Stopped but
        // `terminate` failed or the exit wait timed out: pid still recorded,
        // owner fenced, worker actually alive. A later Stop must retry the
        // kill instead of no-oping on the Stopped early return.
        let (_dir, store) = store();
        let mut child = spawn_long_running_child();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.identity.session_id = Some("sess-stop-wedged".into());
        state.process.status = BackgroundJobStatus::Stopped;
        state.process.completed_at_ms = Some(now_ms());
        state.process.process_owner_fenced = true;
        state.process.pid = Some(child.id());
        state.process.pid_identity = super::super::process_identity(child.id());
        store.write_state(&state).unwrap();

        if let Err(err) = stop_background_job_in_store(&store, &mut state) {
            let _ = child.kill();
            panic!("stop retry failed: {err}");
        }

        let _ = child.wait();
        assert_eq!(super::super::process_is_running(child.id()), Some(false));
        let reloaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(reloaded.process.status, BackgroundJobStatus::Stopped);
        assert_eq!(reloaded.process.pid, None);
        assert!(!reloaded.process.process_owner_fenced);
        let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
        assert!(events
            .iter()
            .any(|event| event.kind == "stop_retry_terminated"));
    }

    #[test]
    fn stopped_job_without_pid_clears_stale_owner_metadata() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Stopped;
        state.process.process_owner_fenced = true;
        state.process.owner_detached_group = true;
        state.process.ipc_port = Some(41000);
        state.process.ipc_token = Some("stale-endpoint".into());
        store.write_state(&state).unwrap();

        stop_background_job_in_store(&store, &mut state).unwrap();

        assert_eq!(state.recorded_owner(), RecordedOwnerSnapshot::unowned(0));
    }

    #[test]
    fn stop_rejects_a_replacement_owner_generation() {
        let (_dir, store) = store();
        let mut observed = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        observed.identity.session_id = Some("sess-stop-owner-race".into());
        observed.process.status = BackgroundJobStatus::Running;
        observed.process.turn_generation = 3;
        observed.process.pid = Some(std::process::id());
        store.write_state(&observed).unwrap();
        let mut stale = observed.clone();
        store
            .update_state(&observed.identity.job_id, |current| {
                current.process.turn_generation = 4;
                current.process.ipc_port = Some(40002);
                current.process.ipc_token = Some("replacement-owner".into());
                current.process.updated_at_ms = current.process.updated_at_ms.saturating_add(1);
                Ok(())
            })
            .unwrap();

        let error = stop_background_job_in_store(&store, &mut stale).unwrap_err();

        assert!(error.to_string().contains("changed owner"));
        let current = store.read_state(&observed.identity.job_id).unwrap();
        assert_eq!(current.process.status, BackgroundJobStatus::Running);
        assert_eq!(current.process.turn_generation, 4);
        assert_eq!(
            current.process.ipc_token.as_deref(),
            Some("replacement-owner")
        );
    }

    #[test]
    fn stop_running_detached_session_does_not_kill_self_pid() {
        // Detached sessions store the TUI's own pid as the job's pid.
        // Stopping such a job (e.g. on attach) must not invoke
        // `terminate_process(self)` or `wait_for_pid_exit(self)` —
        // doing so killed the TUI on every agent-view attach.
        let (_dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        let self_pid = std::process::id();
        state.identity.session_id = Some("sess-self-detached".into());
        state.process.status = BackgroundJobStatus::Running;
        state.process.pid = Some(self_pid);
        state.outcome.summary = Some("running detached from interactive session".into());
        store.write_state(&state).unwrap();

        // If the guard regresses, `wait_for_pid_exit` would block for
        // the full 5-second timeout against our own pid before
        // returning — exceeding the runtime ceiling of this test.
        let started = std::time::Instant::now();
        stop_background_job_in_store(&store, &mut state).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "stop must not wait on the TUI's own pid"
        );
        assert_eq!(state.process.status, BackgroundJobStatus::Stopped);
        let reloaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(reloaded.process.status, BackgroundJobStatus::Stopped);
    }

    #[test]
    fn stop_terminal_job_does_not_relabel_as_stopped() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Succeeded;
        state.process.completed_at_ms = Some(now_ms());
        state.process.owner_detached_group = true;
        state.process.process_owner_fenced = true;
        state.process.ipc_port = Some(41000);
        state.process.ipc_token = Some("stale-terminal-endpoint".into());
        store.write_state(&state).unwrap();

        stop_background_job_in_store(&store, &mut state).unwrap();

        let loaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
        assert_eq!(loaded.recorded_owner(), RecordedOwnerSnapshot::unowned(0));
        let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
        assert!(events
            .iter()
            .any(|event| event.kind == "stop_ignored_terminal"));
    }

    #[test]
    fn stop_terminal_job_terminates_lingering_exact_owner_without_relabelling() {
        let (_dir, store) = store();
        let mut child = spawn_long_running_child();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Failed;
        state.process.completed_at_ms = Some(now_ms());
        state.process.pid = Some(child.id());
        state.process.pid_identity = super::super::process_identity(child.id());
        store.write_state(&state).unwrap();

        if let Err(error) = stop_background_job_in_store(&store, &mut state) {
            let _ = child.kill();
            panic!("terminal owner stop failed: {error}");
        }
        let _ = child.wait();

        assert_eq!(state.process.status, BackgroundJobStatus::Failed);
        assert_eq!(state.recorded_owner(), RecordedOwnerSnapshot::unowned(0));
        assert_eq!(super::super::process_is_running(child.id()), Some(false));
    }

    #[test]
    fn stop_running_job_clears_ipc_and_pending_permission() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::NeedsInput;
        state.process.pid = None;
        state.process.ipc_port = Some(1234);
        state.process.ipc_token = Some("secret".into());
        state.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
            query_id: 1,
            turn_generation: 0,
            endpoint: None,
            tool: Some("Bash".into()),
            tool_call_id: None,
            session_id: None,
            title: Some("Run command".into()),
            message: None,
            tool_input: None,
            metadata: None,
            options: Vec::new(),
        });
        state.identity.pending_prompts = vec![crate::PendingPrompt::new(
            "pp-stop-test".into(),
            "queued follow up".into(),
            vec![crate::BackgroundImageAttachment {
                id: 1,
                data: "pending-image".into(),
                media_type: "image/png".into(),
                filename: None,
                source_path: None,
            }],
            crate::now_ms(),
        )
        .unwrap()];
        store.write_state(&state).unwrap();

        stop_background_job_in_store(&store, &mut state).unwrap();

        let loaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(loaded.process.status, BackgroundJobStatus::Stopped);
        assert_eq!(loaded.process.pid, None);
        assert_eq!(loaded.process.ipc_port, None);
        assert_eq!(loaded.process.ipc_token, None);
        assert_eq!(loaded.outcome.pending_permission, None);
        assert!(loaded.identity.pending_prompts.is_empty());
    }
}
