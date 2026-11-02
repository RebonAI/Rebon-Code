//! Wakeups of hosted sessions whose worker is gone, delivered by the
//! background supervisor.
//!
//! A session a background job hosts mirrors its `ScheduleWakeup` slot to
//! `<job dir>/wakeup.json`, and while its worker lives the worker fires it.
//! But the supervisor reaps a worker idle for an hour, and a worker can die;
//! a wakeup living only in that worker's memory would die with it, and a
//! scheduled prompt queued later would never start a new worker. So the
//! supervisor, each tick, delivers the due wakeup of every job that has no
//! live worker: it is queued as the job's next turn, marked as scheduled,
//! and the supervisor's next tick starts a worker to run it. While any job
//! has a wakeup armed the supervisor stays up for it.

use std::path::{Path, PathBuf};

use rebon_session_host::{
    recorded_process_is_running, BackgroundJobState, BackgroundJobStatus, BackgroundStore,
};
use rebon_tool::cron::{deliver_persisted_wakeup, persisted_wakeup_armed, WAKEUP_FILE_NAME};

/// Where a job's session mirrors its wakeup.
pub(crate) fn wakeup_path(store: &BackgroundStore, job_id: &str) -> PathBuf {
    store.job_dir(job_id).join(WAKEUP_FILE_NAME)
}

/// Deliver the due wakeups of jobs with no live worker. `Ok(true)` while any
/// job still has one armed, which keeps the supervisor running for it.
pub(crate) fn deliver_hosted_wakeups(
    store: &BackgroundStore,
    jobs: &[BackgroundJobState],
    now_ms: u64,
) -> anyhow::Result<bool> {
    let mut armed = false;
    for job in jobs {
        let path = wakeup_path(store, &job.identity.job_id);
        if !path.exists() {
            continue;
        }
        if job_may_be_woken(job) && !worker_alive(job) {
            deliver_one(store, job, &path, now_ms);
        }
        armed |= job_may_be_woken(job) && persisted_wakeup_armed(&path);
    }
    Ok(armed)
}

/// A job the user stopped stays stopped: its wakeup is not delivered. One
/// that is queued or running has, or is about to have, a worker of its own.
fn job_may_be_woken(job: &BackgroundJobState) -> bool {
    matches!(
        job.process.status,
        BackgroundJobStatus::Idle | BackgroundJobStatus::Succeeded | BackgroundJobStatus::Failed
    )
}

/// A worker still runs for the job, and fires the wakeup itself. A pid that
/// cannot be checked counts as alive: two deliveries are worse than a late one.
fn worker_alive(job: &BackgroundJobState) -> bool {
    job.process.pid.is_some_and(|pid| {
        recorded_process_is_running(pid, job.process.pid_identity.as_deref()).unwrap_or(true)
    })
}

fn deliver_one(store: &BackgroundStore, job: &BackgroundJobState, path: &Path, now_ms: u64) {
    let job_id = job.identity.job_id.as_str();
    // Queued as the job's next turn first; only then is the wakeup spent.
    let queue = |wakeup: &rebon_tool::cron::SessionWakeup| {
        let id = format!("u-internal-scheduled-wakeup-{}", wakeup.id);
        let text = rebon_tool::cron::mark_scheduled_prompt(&wakeup.prompt);
        rebon_session_host::append_background_scheduled_prompt(store, job_id, id, text).map(|_| ())
    };
    if let Err(error) = deliver_persisted_wakeup(path, now_ms as i64, queue) {
        tracing::warn!(%job_id, error = %format!("{error:#}"), "rebon: could not deliver a session wakeup");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_session_host::BackgroundRuntimeFields;
    use rebon_tool::SessionCronStore;

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

    /// A job whose worker armed a wakeup and was then reaped.
    fn reaped_job_with_wakeup(
        store: &BackgroundStore,
        status: BackgroundJobStatus,
        delay_ms: i64,
    ) -> BackgroundJobState {
        let mut job = store
            .create_job("p".into(), PathBuf::from("."), runtime())
            .unwrap();
        job.process.status = status;
        job.process.pid = None;
        job.identity.session_id = Some(format!("sess-{}", job.identity.job_id));
        store.write_state(&job).unwrap();
        let worker = SessionCronStore::new();
        worker.persist_wakeup_at(wakeup_path(store, &job.identity.job_id));
        worker
            .arm_wakeup(
                "Continue the self-paced /loop: check CI",
                "CI",
                0,
                delay_ms,
                None,
            )
            .unwrap();
        job
    }

    #[test]
    fn a_reaped_jobs_due_wakeup_becomes_its_next_scheduled_turn() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let job = reaped_job_with_wakeup(&store, BackgroundJobStatus::Succeeded, 60_000);

        assert!(
            deliver_hosted_wakeups(&store, &[job.clone()], 59_000).unwrap(),
            "armed: stay up"
        );
        assert!(store
            .read_state(&job.identity.job_id)
            .unwrap()
            .identity
            .pending_prompts
            .is_empty());

        assert!(
            !deliver_hosted_wakeups(&store, &[job.clone()], 60_000).unwrap(),
            "delivered"
        );
        let loaded = store.read_state(&job.identity.job_id).unwrap();
        assert_eq!(
            loaded.process.status,
            BackgroundJobStatus::Queued,
            "the next tick starts a worker"
        );
        let [prompt] = loaded.identity.pending_prompts.as_slice() else {
            panic!("one prompt: {:?}", loaded.identity.pending_prompts);
        };
        assert!(prompt.id.starts_with("u-internal-scheduled-wakeup-"));
        assert_eq!(
            prompt.origin,
            Some(rebon_session_host::PendingPromptOrigin::Scheduled)
        );
        assert!(!prompt.typed_by_user());
        assert_eq!(
            prompt.text,
            rebon_tool::cron::mark_scheduled_prompt("Continue the self-paced /loop: check CI")
        );

        // Delivered once.
        let loaded = store.read_state(&job.identity.job_id).unwrap();
        deliver_hosted_wakeups(&store, &[loaded], 120_000).unwrap();
        let again = store.read_state(&job.identity.job_id).unwrap();
        assert_eq!(again.identity.pending_prompts.len(), 1);
    }

    #[test]
    fn a_live_worker_fires_its_own_wakeup() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = reaped_job_with_wakeup(&store, BackgroundJobStatus::Idle, 1_000);
        job.process.pid = Some(std::process::id());
        job.process.pid_identity = rebon_session_host::process_identity(std::process::id());
        assert!(deliver_hosted_wakeups(&store, &[job.clone()], 10_000).unwrap());
        assert!(store
            .read_state(&job.identity.job_id)
            .unwrap()
            .identity
            .pending_prompts
            .is_empty());
    }

    #[test]
    fn a_stopped_or_busy_job_is_not_woken_and_does_not_hold_the_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        for status in [
            BackgroundJobStatus::Stopped,
            BackgroundJobStatus::Queued,
            BackgroundJobStatus::Running,
            BackgroundJobStatus::NeedsInput,
        ] {
            let job = reaped_job_with_wakeup(&store, status, 1_000);
            assert!(
                !deliver_hosted_wakeups(&store, &[job.clone()], 10_000).unwrap(),
                "{status:?}"
            );
            assert!(
                store
                    .read_state(&job.identity.job_id)
                    .unwrap()
                    .identity
                    .pending_prompts
                    .is_empty(),
                "{status:?}"
            );
        }
    }

    #[test]
    fn jobs_without_a_wakeup_cost_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let job = store
            .create_job("p".into(), PathBuf::from("."), runtime())
            .unwrap();
        assert!(!deliver_hosted_wakeups(&store, &[job], 0).unwrap());
    }
}
