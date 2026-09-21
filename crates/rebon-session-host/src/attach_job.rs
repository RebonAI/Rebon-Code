//! Attaching to the job a session lives in: mirror its worker when one is up
//! and answering, give it one when none is.
//!
//! Every endpoint that hosts sessions headlessly (`rebon serve`, the terminal,
//! the Remote Control runner) asks
//! the one implementation. What an endpoint does with the answer — the
//! terminal turns it into runtime overrides for its mirror — stays with the
//! endpoint.

use std::path::Path;

use super::{
    ensure_supervisor_running, now_ms, process_is_running, recorded_process_is_running,
    send_background_ipc_request, warm_background_job_for_peek_in_store, BackgroundIpcRequest,
    BackgroundJobState, BackgroundJobStatus, BackgroundStore,
};

/// How a client ends up attached to a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundAttachMode {
    /// A worker is up and answering; mirror it.
    RemoteProxy,
    /// The job had no live worker, so one was queued. Mirror it once it
    /// publishes an endpoint — a session never comes back to this process.
    WorkerStarting,
}

/// The job an attach found or brought back, as recorded when it returned.
///
/// For [`BackgroundAttachMode::RemoteProxy`] the record carries the live
/// worker's pid, port and token; for `WorkerStarting` it is the record after
/// the worker was queued.
#[derive(Debug, Clone)]
pub struct AttachedJob {
    pub mode: BackgroundAttachMode,
    pub state: BackgroundJobState,
}

/// Is this job a live worker a client could mirror right now — without
/// changing anything if the answer is no?
///
/// [`attach_background_job_in_store`] cannot answer this question: a job
/// whose worker is not up gets one queued, and a `Queued` job that the
/// supervisor has not picked up yet is exactly the state a handover sits in
/// for the first second of its life. Polling with the attach call would
/// keep re-queuing it. So this reads, pings, and returns `None` for every
/// "not yet" — never a write. `Some` is the job as read, with its worker's
/// endpoint recorded.
pub fn mirrorable_background_job_in_store(
    store: &BackgroundStore,
    job_id: &str,
) -> anyhow::Result<Option<BackgroundJobState>> {
    let state = store.read_state(job_id)?;
    if state.identity.session_id.is_none() {
        return Ok(None);
    }
    if background_owner_is_running(&state)? != Some(true) {
        return Ok(None);
    }
    if !endpoint_accepts_ping(&state) {
        return Ok(None);
    }
    Ok(Some(state))
}

/// Find, or bring back, the worker a client attaches to.
///
/// A live worker is mirrored. A job without one gets a worker queued and the
/// caller waits for it (`WorkerStarting`): a session never returns to the
/// attaching process. Whatever the job was doing when its worker went —
/// parked, mid-turn with a prompt still claimed, stopped by the user — the
/// new worker resumes the same session and carries on from the record.
///
/// `start_supervisor` is what actually brings a queued worker up; tests pass
/// `false` and read the job record instead. `rebon_exe_path` is the
/// already-resolved executable the supervisor is started from; it is never
/// looked up on `PATH`.
pub fn attach_background_job_in_store(
    store: &BackgroundStore,
    job_id: &str,
    start_supervisor: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<AttachedJob> {
    let mut state = store.read_state(job_id)?;
    store.reconcile_stale_pid(&mut state)?;
    if state.identity.session_id.is_none() {
        anyhow::bail!("background job {job_id} has not started a session yet");
    };

    let owner_is_running = background_owner_is_running(&state)?;
    if owner_is_running == Some(true) && endpoint_accepts_ping(&state) {
        return Ok(AttachedJob {
            mode: BackgroundAttachMode::RemoteProxy,
            state,
        });
    }
    if owner_is_running == Some(true) {
        anyhow::bail!(
            "background job {job_id} is still owned by live process {}, but its IPC endpoint is unavailable; not starting another worker beside it",
            state.process.pid.expect("live owner requires a pid")
        );
    }

    revive_background_job(store, &state, start_supervisor, rebon_exe_path)?;
    let state = store.read_state(job_id)?;
    Ok(AttachedJob {
        mode: BackgroundAttachMode::WorkerStarting,
        state,
    })
}

/// Queue a worker for a job that has none.
fn revive_background_job(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    start_supervisor: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<()> {
    let job_id = state.identity.job_id.as_str();
    match state.process.status {
        // Already waiting for the supervisor; only make sure there is one.
        BackgroundJobStatus::Queued => {}
        // The owner died mid-turn — reconciliation just cleared it. Queued
        // again, the next worker resumes the session and takes up the prompt
        // it was on; with nothing claimed it parks, like a handover.
        BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput => {
            let has_prompts = state.has_pending_prompts();
            store.update_state(job_id, |current| {
                if current.process.process_owner_fenced {
                    anyhow::bail!(
                        "background job {job_id} ownership is fenced until its recorded process exit is verified"
                    );
                }
                if current.process.removal_reserved {
                    anyhow::bail!("background job {job_id} is reserved for removal");
                }
                current.process.status = BackgroundJobStatus::Queued;
                current.identity.resume_only = !has_prompts;
                current.outcome.pending_permission = None;
                current.process.updated_at_ms = now_ms();
                Ok(())
            })?;
        }
        BackgroundJobStatus::Idle
        | BackgroundJobStatus::Succeeded
        | BackgroundJobStatus::Failed
        | BackgroundJobStatus::Stopped => {
            if state.has_pending_prompts() {
                // An accepted follow-up the old worker never ran: queue the
                // job for real, so the new worker runs it rather than parks.
                store.update_state(job_id, |current| {
                    if current.process.process_owner_fenced {
                        anyhow::bail!(
                            "background job {job_id} ownership is fenced until its recorded process exit is verified"
                        );
                    }
                    if current.process.removal_reserved {
                        anyhow::bail!("background job {job_id} is reserved for removal");
                    }
                    current.process.status = BackgroundJobStatus::Queued;
                    current.identity.resume_only = false;
                    current.outcome.pending_permission = None;
                    current.process.completed_at_ms = None;
                    current.outcome.exit_code = None;
                    current.outcome.error = None;
                    current.process.updated_at_ms = now_ms();
                    Ok(())
                })?;
            } else if !warm_background_job_for_peek_in_store(store, job_id, false, rebon_exe_path)?
            {
                anyhow::bail!(
                    "background job {job_id} could not be given a worker (status {})",
                    state.process.status.as_str()
                );
            }
        }
    }
    if start_supervisor {
        ensure_supervisor_running(store, rebon_exe_path)?;
    }
    store.append_event(job_id, "worker_revived_for_attach", serde_json::json!({}))?;
    Ok(())
}

fn background_owner_is_running(state: &BackgroundJobState) -> anyhow::Result<Option<bool>> {
    let Some(pid) = state.process.pid else {
        return Ok(None);
    };
    match state.process.pid_identity.as_deref() {
        Some(identity) => recorded_process_is_running(pid, Some(identity))
            .map(Some)
            .map_err(|err| {
                anyhow::anyhow!(
                    "cannot safely determine whether background job {} owner process {pid} is still running: {err}",
                    state.identity.job_id
                )
            }),
        None => process_is_running(pid).map(Some).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot safely determine whether background job {} owner process {pid} is still running",
                state.identity.job_id
            )
        }),
    }
}

fn endpoint_accepts_ping(state: &BackgroundJobState) -> bool {
    let (Some(port), Some(token)) = (state.process.ipc_port, state.process.ipc_token.clone())
    else {
        return false;
    };
    send_background_ipc_request(state, port, token, BackgroundIpcRequest::Ping).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BackgroundImageAttachment, BackgroundIpcEnvelope, BackgroundIpcResponse,
        BackgroundRuntimeFields, PendingPrompt,
    };
    use std::path::PathBuf;

    fn store() -> (tempfile::TempDir, BackgroundStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        (dir, store)
    }

    /// An absolute path nothing lives at. The attaches below that pass it
    /// with `start_supervisor = true` return before a supervisor would be
    /// started; one that did not would fail on it rather than find another
    /// build on `PATH`.
    fn unspawnable_exe(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("no-such-rebon-build").join("rebon.exe")
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

    fn exited_process_id() -> u32 {
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .unwrap();
        #[cfg(unix)]
        let mut child = std::process::Command::new("sh")
            .args(["-c", "true"])
            .spawn()
            .unwrap();
        #[cfg(not(any(windows, unix)))]
        compile_error!("attach ownership tests require process liveness support");

        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn pending_prompt(
        id: &str,
        text: &str,
        images: Vec<BackgroundImageAttachment>,
    ) -> PendingPrompt {
        PendingPrompt::new(id.into(), text.into(), images, now_ms()).unwrap()
    }

    /// The two entry points differ exactly where a `/hosted` handover lives:
    /// a `Queued` job with a session and no owner yet — warmed, waiting for
    /// the supervisor to spawn its worker. Asking whether it is *mirrorable*
    /// is a question and leaves it alone; asking to *attach* leaves it
    /// queued too, and says a worker is on its way. A UI that polls must
    /// use the question.
    #[test]
    fn probing_a_queued_job_leaves_it_alone_where_attaching_releases_it() {
        let (dir, store) = store();
        let mut state = store
            .create_job("handover".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Queued;
        state.identity.session_id = Some("sess-awaiting-supervisor".into());
        state.identity.resume_only = true;
        store.write_state(&state).unwrap();

        assert!(
            mirrorable_background_job_in_store(&store, &state.identity.job_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .read_state(&state.identity.job_id)
                .unwrap()
                .process
                .status,
            BackgroundJobStatus::Queued,
            "probing must not take the job away from the supervisor"
        );

        let attached = attach_background_job_in_store(
            &store,
            &state.identity.job_id,
            false,
            &unspawnable_exe(&dir),
        )
        .unwrap();

        assert_eq!(attached.mode, BackgroundAttachMode::WorkerStarting);
        assert_eq!(
            store
                .read_state(&state.identity.job_id)
                .unwrap()
                .process
                .status,
            BackgroundJobStatus::Queued,
            "attaching never takes a queued job away from the supervisor"
        );
    }

    #[test]
    fn attach_to_live_running_and_idle_jobs_preserves_worker_and_uses_remote_proxy() {
        for (index, status) in [BackgroundJobStatus::Running, BackgroundJobStatus::Idle]
            .into_iter()
            .enumerate()
        {
            let (dir, store) = store();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let envelope: BackgroundIpcEnvelope = serde_json::from_reader(&mut stream).unwrap();
                assert_eq!(envelope.request, BackgroundIpcRequest::Ping);
                serde_json::to_writer(&mut stream, &BackgroundIpcResponse::ok()).unwrap();
            });
            let mut state = store
                .create_job("prompt".into(), PathBuf::from("."), runtime())
                .unwrap();
            state.process.status = status;
            state.identity.session_id = Some(format!("sess-live-{index}"));
            state.process.pid = Some(std::process::id());
            state.process.ipc_port = Some(port);
            state.process.ipc_token = Some("secret".into());
            store.write_state(&state).unwrap();

            let attached = attach_background_job_in_store(
                &store,
                &state.identity.job_id,
                true,
                &unspawnable_exe(&dir),
            )
            .unwrap();

            server.join().unwrap();
            assert_eq!(attached.mode, BackgroundAttachMode::RemoteProxy);
            assert_eq!(attached.state.process.pid, Some(std::process::id()));
            assert_eq!(attached.state.process.ipc_port, Some(port));
            let loaded = store.read_state(&state.identity.job_id).unwrap();
            assert_eq!(loaded.process.status, status);
            assert_eq!(loaded.process.pid, Some(std::process::id()));
            assert_eq!(loaded.process.ipc_port, Some(port));
        }
    }

    /// A job without a worker gets one queued, whatever it was doing when
    /// the last one went: a claimed prompt stays claimed for the next worker
    /// to run, an idle session is warmed so the worker parks on it. Nothing
    /// is released, and nothing is taken over.
    #[test]
    fn attach_with_no_owner_queues_a_worker() {
        for (index, status) in [
            BackgroundJobStatus::Queued,
            BackgroundJobStatus::Running,
            BackgroundJobStatus::NeedsInput,
            BackgroundJobStatus::Idle,
        ]
        .into_iter()
        .enumerate()
        {
            let (dir, store) = store();
            let mut state = store
                .create_job("prompt".into(), PathBuf::from("."), runtime())
                .unwrap();
            state.process.status = status;
            if matches!(
                status,
                BackgroundJobStatus::Queued | BackgroundJobStatus::Running
            ) {
                state.identity.pending_prompts = vec![pending_prompt(
                    &format!("pp-missing-{index}"),
                    "accepted follow-up",
                    Vec::new(),
                )];
            }
            state.identity.session_id = Some(format!("sess-missing-{index}"));
            state.process.pid = None;
            state.process.ipc_port = None;
            state.process.ipc_token = None;
            store.write_state(&state).unwrap();

            let attached = attach_background_job_in_store(
                &store,
                &state.identity.job_id,
                false,
                &unspawnable_exe(&dir),
            )
            .unwrap();

            assert_eq!(attached.mode, BackgroundAttachMode::WorkerStarting);
            let loaded = store.read_state(&state.identity.job_id).unwrap();
            assert_eq!(loaded.process.pid, None);
            assert_eq!(loaded.process.ipc_port, None);
            assert_eq!(loaded.process.ipc_token, None);
            assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
            if matches!(
                status,
                BackgroundJobStatus::Queued | BackgroundJobStatus::Running
            ) {
                assert_eq!(
                    loaded.pending_prompt().map(|prompt| prompt.text.as_str()),
                    Some("accepted follow-up")
                );
                assert!(
                    !loaded.identity.resume_only,
                    "a claimed prompt is run, not parked on"
                );
            } else {
                assert!(loaded.identity.pending_prompts.is_empty());
                assert!(
                    loaded.identity.resume_only,
                    "nothing to run: the worker parks on the session"
                );
            }
            assert!(store
                .read_events_tail(&state.identity.job_id, 10)
                .unwrap()
                .iter()
                .any(|event| event.kind == "worker_revived_for_attach"));
        }
    }

    #[test]
    fn attach_with_live_owner_and_missing_endpoint_refuses_takeover() {
        let (dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Running;
        state.identity.session_id = Some("sess-live-missing-endpoint".into());
        state.process.pid = Some(std::process::id());
        state.process.ipc_port = Some(49152);
        state.process.ipc_token = None;
        store.write_state(&state).unwrap();

        let error = match attach_background_job_in_store(
            &store,
            &state.identity.job_id,
            true,
            &unspawnable_exe(&dir),
        ) {
            Ok(_) => panic!("a live owner must not get a second worker beside it"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("not starting another worker"));
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(loaded.process.status, BackgroundJobStatus::Running);
        assert_eq!(loaded.process.pid, Some(std::process::id()));
        assert_eq!(loaded.process.ipc_port, Some(49152));
    }

    #[test]
    fn attach_to_failed_claimed_turn_recovers_the_accepted_follow_up() {
        let (dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Failed;
        state.identity.session_id = Some("sess-failed-follow-up".into());
        state.identity.pending_prompts = vec![pending_prompt(
            "pp-crash-recovery",
            "accepted before worker crash",
            vec![BackgroundImageAttachment {
                id: 1,
                data: "image".into(),
                media_type: "image/png".into(),
                filename: None,
                source_path: None,
            }],
        )];
        store.write_state(&state).unwrap();

        let attached = attach_background_job_in_store(
            &store,
            &state.identity.job_id,
            false,
            &unspawnable_exe(&dir),
        )
        .unwrap();

        // The follow-up is the next worker's to run, images and all — not
        // this process's.
        assert_eq!(attached.mode, BackgroundAttachMode::WorkerStarting);
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
        assert!(!loaded.identity.resume_only);
        assert_eq!(
            loaded.pending_prompt().map(|prompt| prompt.text.as_str()),
            Some("accepted before worker crash")
        );
        assert_eq!(loaded.identity.pending_prompts[0].images.len(), 1);
    }

    #[test]
    fn attach_to_completed_job_preserves_a_lingering_live_owner() {
        let (dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Succeeded;
        state.identity.session_id = Some("sess-one".into());
        state.process.pid = Some(std::process::id());
        state.process.ipc_port = Some(49152);
        state.process.ipc_token = None;
        store.write_state(&state).unwrap();

        let error = match attach_background_job_in_store(
            &store,
            &state.identity.job_id,
            true,
            &unspawnable_exe(&dir),
        ) {
            Ok(_) => panic!("a live owner must not get a second worker beside it"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("not starting another worker"));
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(loaded.process.status, BackgroundJobStatus::Succeeded);
        assert_eq!(loaded.process.pid, Some(std::process::id()));
        assert_eq!(loaded.process.ipc_port, Some(49152));
        assert!(store
            .read_events_tail(&state.identity.job_id, 10)
            .unwrap()
            .iter()
            .all(|event| event.kind != "worker_revived_for_attach"));
    }

    #[test]
    fn attach_to_completed_job_releases_a_definitively_dead_owner() {
        let (dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Succeeded;
        state.identity.session_id = Some("sess-dead-owner".into());
        state.process.pid = Some(exited_process_id());
        state.process.pid_identity = None;
        state.process.ipc_port = Some(49152);
        state.process.ipc_token = None;
        store.write_state(&state).unwrap();

        let attached = attach_background_job_in_store(
            &store,
            &state.identity.job_id,
            false,
            &unspawnable_exe(&dir),
        )
        .unwrap();

        assert_eq!(
            attached.state.identity.session_id.as_deref(),
            Some("sess-dead-owner")
        );
        assert_eq!(attached.mode, BackgroundAttachMode::WorkerStarting);
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        // Stale-pid reconciliation frees the provably dead owner; the job is
        // then warmed so the next worker parks on its session.
        assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
        assert!(loaded.identity.resume_only);
        assert_eq!(loaded.process.pid, None);
        assert_eq!(loaded.process.ipc_port, None);
        assert_eq!(loaded.process.ipc_token, None);
        let events = store.read_events_tail(&state.identity.job_id, 10).unwrap();
        assert!(events
            .iter()
            .any(|event| event.kind == "stale_pid_reconciled"));
    }
}
