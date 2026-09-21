//! The session side of handing a session to a worker and of what a terminal
//! may do to the job it is looking at.
//!
//! Three answers that need a session or a job store and no screen: which
//! worker a dispatch is being made *from*, whether a handover that never
//! started can be undone, and stopping a job together with what it started.
//!
//! The retake is the load-bearing one. It is the single place a session comes
//! back into this process, and only because it never left — a session a worker
//! did resume is never taken back, it gets a replacement worker instead.
//! It also restarts the cron scheduler, because a session
//! built as a mirror started none, and that pairing is the mirror image of the
//! release performed when a session becomes a mirror.
//!
//! Deliberately not in [`crate::session_shell::hosted_startup`]: that module is
//! defined by never taking the session lock, and this one exists to take it.

use crate::background::RuntimeFieldsExt;
use crate::session::EngineSession;

/// The worker a dispatch is being made *from*, if this session runs in one.
///
/// Only a mirrored session answers, because that is the case where the work
/// being dispatched belongs to a worker rather than to this terminal. An
/// attach-here job (`Shift+Enter`) records a job id too, but its turn runs in
/// this process — nothing it starts outlives the terminal anyway, so there is
/// no ownership to record.
pub(crate) fn dispatching_worker_job_id(
    session: &crate::session_shell::TuiEngineSession,
) -> Option<String> {
    session
        .remote_background_attachment
        .as_ref()
        .map(|remote| remote.job_id.clone())
}

/// Undo a handover that never started: the lock was released for a worker
/// that was not spawned, so nothing else can have the session unless
/// something already took it.
///
/// This is the one place a session is taken back into this process, and
/// only because it never left. A session a worker did resume never comes
/// back here — a lost worker gets replaced, not replaced by the terminal.
/// Returns whether the session is this terminal's again; otherwise the
/// caller parks it on the job.
pub(crate) fn retake_lock_after_failed_handover(session: &mut EngineSession, job_id: &str) -> bool {
    if session.session_active_lock.is_some() {
        return true;
    }
    match rebon_session::try_acquire_session_active_lock(
        &session.projects_root,
        &session.cwd,
        &session.session_id,
    ) {
        // The session runs here after all, so its cwd's scheduled tasks fire
        // here: a session built as a mirror started no scheduler.
        Ok(Some(lock)) => adopt_session_as_owner(session, Some(lock)),
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(
                %err,
                %job_id,
                session_id = %session.session_id,
                "hosted handover: could not tell whether the session is still claimed"
            );
            false
        }
    }
}

pub(crate) fn stop_agent_view_job_in_store(
    store: &crate::background::BackgroundStore,
    job_id: &str,
) -> anyhow::Result<crate::background::StoppedJobTree> {
    let mut state = store.read_state(job_id)?;
    crate::background::stop_background_job_tree_in_store(store, &mut state)
}

/// The runtime a background job inherits from the session on screen.
///
/// Everything but three values comes off the session itself. Those three --
/// the ui mode, the effort level and the permission mode -- live on the
/// terminal, so they are passed in rather than reached for, which is what lets
/// this answer be built without one.
pub(crate) fn background_runtime_from_session(
    session: &EngineSession,
    ui_mode: crate::ui_config::UiMode,
    effort_level: Option<rebon_types::ReasoningEffort>,
    permission_mode: rebon_permissions::PermissionMode,
) -> crate::background::BackgroundRuntimeFields {
    crate::background::BackgroundRuntimeFields::from_runtime_override(
        &crate::rebon_config::RuntimeOverride {
            provider: if session.model.provider_name == "env" {
                None
            } else {
                Some(session.model.provider_name.clone())
            },
            model: Some(session.model.name.clone()),
            fast_mode: Some(session.model.service_tier.is_fast()),
            resume: None,
            cwd: None,
            channels: session.startup.channels.clone(),
            development_channels: session.startup.development_channels.clone(),
            settings: session.startup.settings.clone(),
            add_dirs: session.startup.add_dirs.clone(),
            plugin_dirs: session.startup.plugin_dirs.clone(),
            mcp_configs: session.startup.mcp_configs.clone(),
            strict_mcp_config: session.startup.strict_mcp_config,
            ui_mode: Some(ui_mode),
            effort_level,
            permission_mode: Some(permission_mode),
            queue_session: session.startup.queue_session,
            startup_agent_view: false,
            startup_hosted: false,
            startup_local: false,
            startup_agent_view_cwd_scope: None,
            startup_notices: Vec::new(),
            attached_background_job_id: None,
            // Detaching to the background does not re-open the
            // session, so there is no switch to re-request; the
            // sidecar already records which agent it is running on.
            remote: None,
            remote_path: None,
        },
    )
    .with_provider_format(session.model.provider_format)
}

/// What `/stop` leaves the session as, once its worker has been let go and
/// stopped.
///
/// **The order inside [`release_then_stop_worker`] is load-bearing, and is why
/// the release and the stop are one call rather than two a caller sequences.**
/// The goodbye — the lease release — has to reach a process that is still there
/// to hear it, so `worker_gone` runs first and the job tree is stopped second.
/// Reversed, the release lands on a process that is already gone: no error
/// surfaces, and the owner keeps the client's lease until it expires, lingering
/// its full TTL holding a plugin and MCP stack. RFC-0004 §16.7 U3-4 states the
/// rule as "`/stop` 先放再停".
pub(crate) enum StoppedSession {
    /// The attachment was here and has let its worker go. The session is the
    /// job's and stays parked on it, every row it had kept.
    Parked,
    /// No attachment, but this session was waiting on this very job. Parking it
    /// means building an attachment over the rows already on screen, which
    /// needs a screen — so the caller does that half and reports back whether
    /// it took.
    ParkOnJob,
    /// The session was never the job's: a dispatch it was only waiting to
    /// mirror, or an attach-here turn whose work ran in this process. It stays
    /// a local session.
    NotTheJobs,
}

/// The worker `/stop` stopped, and what it started.
pub(crate) struct StoppedWorker {
    pub(crate) job_id: String,
    pub(crate) tree: crate::background::StoppedJobTree,
    pub(crate) session_is: StoppedSession,
}

/// Let go of the attached worker, then stop it and everything it started.
///
/// See [`StoppedSession`] for why the two are one call. Fails only when this
/// session has no attached job, which is `/stop` asked of a session that never
/// had one.
pub(crate) fn release_then_stop_worker(
    session: &mut crate::session_shell::TuiEngineSession,
    store: &crate::background::BackgroundStore,
) -> anyhow::Result<StoppedWorker> {
    release_then_stop_worker_with(session, store, |store, state, _session| {
        crate::background::stop_background_job_tree_in_store(store, state)
    })
}

/// The seam the ordering test needs.
///
/// `stop` is handed the session as it stands at the moment the job tree is
/// stopped, because that instant is the only place the order is observable at
/// all: both orders end with the job stopped and the attachment dead, so an
/// assertion about the end state cannot tell them apart.
fn release_then_stop_worker_with(
    session: &mut crate::session_shell::TuiEngineSession,
    store: &crate::background::BackgroundStore,
    stop: impl FnOnce(
        &crate::background::BackgroundStore,
        &mut crate::background::BackgroundJobState,
        &crate::session_shell::TuiEngineSession,
    ) -> anyhow::Result<crate::background::StoppedJobTree>,
) -> anyhow::Result<StoppedWorker> {
    let Some(job_id) = session.attached_background_job_id.clone() else {
        anyhow::bail!("/stop only stops an attached background job");
    };
    let mut state = store.read_state(&job_id)?;
    // Let go of the worker before stopping it, so the goodbye — the lease
    // release — reaches a process that is still there to hear it.
    if let Some(remote) = session.remote_background_attachment.as_mut() {
        remote.worker_gone(remote.status);
    }
    // Stopping a worker stops what it started. Agents running inside it die
    // with the process either way; the ones it gave a worker of their own
    // have to be told.
    let tree = stop(store, &mut state, &*session)?;
    // A wait for this very job — a handover, or a replacement worker —
    // ends here. The session went with the job, and stays with it.
    let was_waiting_on_it = session
        .pending_hosted_session
        .as_ref()
        .is_some_and(|pending| {
            pending.job_id == job_id
                && matches!(
                    pending.kind,
                    crate::background::PendingHostedKind::Handover
                        | crate::background::PendingHostedKind::Reattach { keep_view: true }
                )
        });
    session.pending_hosted_session = None;
    let session_is = if let Some(remote) = session.remote_background_attachment.as_mut() {
        remote.worker_gone(crate::background::BackgroundJobStatus::Stopped);
        StoppedSession::Parked
    } else if was_waiting_on_it {
        StoppedSession::ParkOnJob
    } else {
        StoppedSession::NotTheJobs
    };
    Ok(StoppedWorker {
        job_id,
        tree,
        session_is,
    })
}

/// Take a session into this process as its owner.
///
/// The mirror image of [`release_session_to_owner`], and it exists for the same
/// reason. Holding the lock is what makes this process the one that runs the
/// session, and this directory's scheduled tasks have to reach the loop that
/// runs it. A path that installs the lock and leaves the scheduler stopped
/// leaves the directory's cron unattended — the same RFC-0004 §16.13 failure as
/// keeping the scheduler after losing the lock, in the other direction.
///
/// `None` means the caller had no lock to install, and nothing is started:
/// a process that does not own the session must not run its scheduler.
///
/// Adopting always rebuilds the scheduler rather than keeping one that is
/// already running: this is the moment the session, and possibly its directory,
/// changes, and a scheduler is bound to the directory it was built for.
pub(crate) fn adopt_session_as_owner(
    session: &mut EngineSession,
    lock: Option<rebon_session::SessionActiveLock>,
) -> bool {
    let Some(lock) = lock else {
        return false;
    };
    session.session_active_lock = Some(lock);
    // Stopped first on purpose. The scheduler binds its directory when it is
    // built and `start_cron_scheduler` returns early when one already exists,
    // so adopting a session in another directory would otherwise keep firing
    // the previous one's tasks and none of this one's.
    session.stop_cron_scheduler();
    session.start_cron_scheduler();
    true
}

/// What this side let go of when the session became another process's.
///
/// **The three things are one thing, and that is why they are one function.**
/// The lock says this process no longer writes the session; the other two are
/// what that sentence means in practice. A second set of MCP servers would only
/// sit idle beside the owner's, and the cwd's cron scheduler has to feed the
/// loop that actually runs the session — which is the owner's, not this one's.
///
/// RFC-0004 §16.13 records separating them as a production defect rather than
/// waste: with a hosted session the terminal and the worker start together, and
/// which of them wins the lock is a coin toss. A terminal that kept the
/// scheduler after losing the toss feeds this directory's scheduled tasks to a
/// loop that is not running the session, and deletes the one-shots as it fires
/// them.
pub(crate) enum SessionHandedOff {
    /// This process held the session and has let all three go.
    Released,
    /// This process never held it — a session that started as a mirror, for
    /// instance — so there was nothing to release.
    NeverHeld,
}

/// Let go of a session that another process now owns.
///
/// See [`SessionHandedOff`] for why the three releases travel together. The
/// lock goes first: while it is still held, `is_session_active` says this
/// process owns a session it has already stopped hosting servers for.
pub(crate) fn release_session_to_owner(session: &mut EngineSession) -> SessionHandedOff {
    let held = session.session_active_lock.is_some();
    session.session_active_lock = None;
    session.release_mcp();
    session.stop_cron_scheduler();
    if held {
        SessionHandedOff::Released
    } else {
        SessionHandedOff::NeverHeld
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    fn empty_background_runtime_fields() -> crate::background::BackgroundRuntimeFields {
        crate::background::BackgroundRuntimeFields {
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

    #[test]
    fn stop_job_from_agent_view_persists_the_stop() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "running prompt".into(),
                PathBuf::from("."),
                empty_background_runtime_fields(),
            )
            .unwrap();
        job.process.status = crate::background::BackgroundJobStatus::Running;
        job.identity.session_id = Some("session-running".into());
        job.process.turn_generation = 1;
        job.process.pid = Some(std::process::id());
        job.process.pid_identity = rebon_session_host::process_identity(std::process::id());
        store.write_state(&job).unwrap();

        stop_agent_view_job_in_store(&store, &job.identity.job_id).unwrap();

        let stopped = store.read_state(&job.identity.job_id).unwrap();
        assert_eq!(
            stopped.process.status,
            crate::background::BackgroundJobStatus::Stopped
        );
        assert!(stopped.process.pid.is_none());
        assert!(!stopped.process.process_owner_fenced);
    }
    /// The lease release has to reach a process that is still running, so the
    /// worker is let go before the job tree is stopped. No assertion about the
    /// end state can tell the two orders apart — both finish with the job
    /// stopped and the attachment dead — so this one looks at the session at
    /// the instant the stop runs.
    #[test]
    fn the_worker_is_let_go_before_the_job_tree_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut session = crate::tui::runner::test_support::make_test_tui_session();
        let mut job = store
            .create_job(
                "stop this one".into(),
                PathBuf::from("."),
                empty_background_runtime_fields(),
            )
            .unwrap();
        job.process.status = crate::background::BackgroundJobStatus::Idle;
        job.identity.session_id = Some(session.session_id.clone());
        store.write_state(&job).unwrap();
        session.attached_background_job_id = Some(job.identity.job_id.clone());
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                job.identity.job_id.clone(),
                session.session_id.clone(),
                session.cwd.clone(),
                job.process.status,
                job.outcome.event_count,
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "remote-order".into(),
                },
            ));
        assert!(
            session
                .remote_background_attachment
                .as_ref()
                .is_some_and(|remote| remote.is_live()),
            "the fixture starts with a worker to let go of"
        );

        let mut worker_was_live_when_stopped = None;
        let stopped = release_then_stop_worker_with(&mut session, &store, |store, state, seen| {
            worker_was_live_when_stopped = Some(
                seen.remote_background_attachment
                    .as_ref()
                    .is_some_and(|remote| remote.is_live()),
            );
            crate::background::stop_background_job_tree_in_store(store, state)
        })
        .unwrap();

        assert_eq!(
            worker_was_live_when_stopped,
            Some(false),
            "the worker must already be let go when the stop runs, or the lease \
             release lands on a process that is no longer there to hear it"
        );
        assert!(matches!(stopped.session_is, StoppedSession::Parked));
        assert_eq!(
            store
                .read_state(&job.identity.job_id)
                .unwrap()
                .process
                .status,
            crate::background::BackgroundJobStatus::Stopped
        );
    }
    /// The lock, the MCP servers and the cron scheduler go together or the
    /// terminal keeps feeding this directory's scheduled tasks to a loop that
    /// is not running the session. No assertion about order
    /// is possible here — three field writes with nothing between them that
    /// could observe a half-done state — but each of the three is separately
    /// visible afterwards, so dropping any one of them turns this red.
    #[test]
    fn handing_a_session_over_releases_the_lock_the_servers_and_the_scheduler() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let mut session = crate::tui::runner::test_support::make_test_tui_session();
        session.cwd = dir.path().to_string_lossy().into_owned();
        session.session_active_lock = rebon_session::try_acquire_session_active_lock(
            &session.projects_root,
            &session.cwd,
            &session.session_id,
        )
        .expect("the lock is free in a fresh temp dir");
        session.start_cron_scheduler();

        assert!(
            session.session_active_lock.is_some(),
            "the fixture starts as the owner"
        );
        assert!(
            session.engine_half.cron_scheduler.is_some(),
            "an owner runs the scheduler"
        );

        let handed = release_session_to_owner(&mut session);

        assert!(matches!(handed, SessionHandedOff::Released));
        assert!(
            session.session_active_lock.is_none(),
            "the lock says this process no longer writes the session"
        );
        assert!(
            session.engine_half.mcp.is_none(),
            "a mirror hosts no servers of its own"
        );
        assert!(
            session.engine_half.cron_scheduler.is_none(),
            "scheduled tasks have to reach the loop that runs the session"
        );
    }

    /// A session that started as a mirror never held any of the three, and
    /// says so rather than reporting a release that did not happen.
    #[test]
    fn handing_over_a_session_this_process_never_held_releases_nothing() {
        let mut session = crate::tui::runner::test_support::make_test_tui_session();
        session.session_active_lock = None;

        assert!(matches!(
            release_session_to_owner(&mut session),
            SessionHandedOff::NeverHeld
        ));
    }
}
