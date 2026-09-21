//! The terminal's half of attaching to a job.
//!
//! Finding and reviving the worker is `rebon-session-host`'s
//! ([`rebon_session_host::attach_background_job_in_store`]); what stays here
//! is turning the job it returns into a [`BackgroundAttachTarget`] — the
//! runtime overrides the terminal's mirror is built from — and the terminal's
//! recovery decisions for a mirror whose worker went away.

use super::{
    attach_target_from_state, background_owner_is_still_recorded, rebon_exe, BackgroundAttachMode,
    BackgroundAttachTarget, BackgroundIpcEndpoint, BackgroundJobStatus, BackgroundStore,
};

/// Is this job a live worker a UI could mirror right now — without changing
/// anything if the answer is no? See
/// [`rebon_session_host::mirrorable_background_job_in_store`]; this only
/// turns the answer into an attach target.
pub(crate) fn mirrorable_background_job_in_store(
    store: &BackgroundStore,
    job_id: &str,
) -> anyhow::Result<Option<BackgroundAttachTarget>> {
    let Some(state) = rebon_session_host::mirrorable_background_job_in_store(store, job_id)? else {
        return Ok(None);
    };
    attach_target_from_state(&state, BackgroundAttachMode::RemoteProxy).map(Some)
}

/// The owner-side result of recovering a mirror whose worker disappeared.
///
/// This is deliberately specific to that recovery. It keeps the job-store
/// reads, reconciliation and revival decision together while leaving terminal
/// wording and `AppState` changes to the caller.
pub(crate) enum LostWorkerRecovery {
    /// A deliberate stop is final until the user submits another prompt.
    Stopped { status: BackgroundJobStatus },
    /// The job already has another worker; follow it without reviving.
    Follow(BackgroundIpcEndpoint),
    /// Recovery queued a replacement worker for this same job.
    Starting { job_id: String },
    /// A live owner is still recorded, so no second worker may be started.
    OwnerStillRecorded { error: String },
    /// No owner remains and a replacement could not be queued.
    Failed {
        status: BackgroundJobStatus,
        error: String,
    },
}

/// Decide and execute recovery for a lost attached worker.
///
/// The stopped check must stay before the attach/revival call: otherwise a
/// watcher silently undoes another client's `rebon stop`.
pub(crate) fn recover_lost_worker_in_store(
    store: &BackgroundStore,
    job_id: &str,
) -> LostWorkerRecovery {
    let stopped = store
        .read_state(job_id)
        .is_ok_and(|state| state.process.status == BackgroundJobStatus::Stopped);
    if stopped {
        return LostWorkerRecovery::Stopped {
            status: BackgroundJobStatus::Stopped,
        };
    }

    match attach_background_job_in_store(store, job_id) {
        Ok(target) if target.mode == BackgroundAttachMode::RemoteProxy => {
            LostWorkerRecovery::Follow(
                target
                    .remote_endpoint
                    .expect("a remote-proxy attach target always has an endpoint"),
            )
        }
        Ok(target) => LostWorkerRecovery::Starting {
            job_id: target.job_id,
        },
        Err(err) => {
            let error = err.to_string();
            if background_owner_is_still_recorded(store, job_id) {
                LostWorkerRecovery::OwnerStillRecorded { error }
            } else {
                let status = store
                    .read_state(job_id)
                    .map(|state| state.process.status)
                    .unwrap_or(BackgroundJobStatus::Failed);
                LostWorkerRecovery::Failed { status, error }
            }
        }
    }
}

/// Return a worker somebody else gave this parked job, without reviving it.
pub(crate) fn worker_given_elsewhere_in_store(
    store: &BackgroundStore,
    job_id: &str,
) -> Option<BackgroundIpcEndpoint> {
    let mut state = store.read_state(job_id).ok()?;
    store.reconcile_stale_pid(&mut state).ok()?;
    Some(BackgroundIpcEndpoint {
        pid: state.process.pid?,
        port: state.process.ipc_port?,
        token: state.process.ipc_token?,
    })
}

/// Find, or bring back, the worker a client attaches to, and make sure of a
/// supervisor to start it. See
/// [`rebon_session_host::attach_background_job_in_store`].
pub(crate) fn attach_background_job_in_store(
    store: &BackgroundStore,
    job_id: &str,
) -> anyhow::Result<BackgroundAttachTarget> {
    attach_background_job_in_store_with_supervisor(store, job_id, true)
}

/// `start_supervisor` is what actually brings a queued worker up; tests pass
/// `false` and read the job record instead.
pub(crate) fn attach_background_job_in_store_with_supervisor(
    store: &BackgroundStore,
    job_id: &str,
    start_supervisor: bool,
) -> anyhow::Result<BackgroundAttachTarget> {
    let attached = rebon_session_host::attach_background_job_in_store(
        store,
        job_id,
        start_supervisor,
        &rebon_exe(),
    )?;
    attach_target_from_state(&attached.state, attached.mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::BackgroundRuntimeFields;
    use std::path::PathBuf;

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

    /// A job whose worker is on its way is attached as the terminal needs
    /// it: resuming the job's session in the job's cwd, bound to the job,
    /// with no endpoint yet. The host decides what happened to the job; this
    /// half only shapes the answer.
    #[test]
    fn a_revived_job_becomes_a_worker_starting_target_that_resumes_its_session() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("prompt".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Idle;
        state.identity.session_id = Some("sess-parked".into());
        store.write_state(&state).unwrap();

        let target =
            attach_background_job_in_store_with_supervisor(&store, &state.identity.job_id, false)
                .unwrap();

        assert_eq!(target.mode, BackgroundAttachMode::WorkerStarting);
        assert_eq!(target.session_id, "sess-parked");
        assert_eq!(target.job_id, state.identity.job_id);
        assert_eq!(target.status, BackgroundJobStatus::Queued);
        assert!(target.remote_endpoint.is_none());
        assert_eq!(target.overrides.resume.as_deref(), Some("sess-parked"));
        assert_eq!(
            target.overrides.attached_background_job_id.as_deref(),
            Some(state.identity.job_id.as_str())
        );
        assert_eq!(target.overrides.cwd.as_deref(), Some(target.cwd.as_str()));
    }

    /// Polling a queued handover must not produce a target; the host says
    /// "not yet" and the terminal passes that on.
    #[test]
    fn a_job_without_a_live_worker_is_not_mirrorable() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("handover".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Queued;
        state.identity.session_id = Some("sess-awaiting-supervisor".into());
        store.write_state(&state).unwrap();

        assert!(
            mirrorable_background_job_in_store(&store, &state.identity.job_id)
                .unwrap()
                .is_none()
        );
    }
}
