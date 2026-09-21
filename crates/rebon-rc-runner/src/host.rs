//! [`SessionPort`] over the machine's session host.
//!
//! The runner never hosts a session: it asks the session host to start or
//! resume one in a worker ([`rebon_session_host::start_hosted_session`],
//! [`rebon_session_host::host_existing_session`],
//! [`rebon_session_host::attach_background_job_in_store`]), follows the
//! worker's owner the way `rebon serve` does, and commands it through the
//! one client every endpoint uses. Owner decisions are
//! [`crate::core::owner`]'s.
//!
//! ## Lease
//!
//! The follower holds a client lease of kind
//! [`ClientLeaseKind::Serve`] while it is subscribed. `rebon rc serve` is
//! what `rebon serve` is — a process serving sessions to a remote page —
//! and the local control plane admits no new variant (only
//! `CancelCall` was allowed): a worker from before a new kind would fail to
//! decode every lease this runner sent. The lease client id
//! (`rebon-rc:<pid>:<rc session>`) says which surface it is.
//!
//! Sessions are opened as `Foreground` jobs, so a worker whose last lease
//! went lingers ten minutes — longer than an RC work lease (90 s by
//! default) takes to lapse and be handed out again, so a runner that
//! restarts, or loses its lease and gets the session back, finds the same
//! worker still up. The runner never releases a lease as deliberate: a
//! session it stops serving is still the user's, and its worker ends on
//! its own schedule, not the runner's.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use rebon_bridge::remote_permission::RebonPermissionOption;
use rebon_session_host::{
    BackgroundImageAttachment, BackgroundIpcRequest, BackgroundJobState, BackgroundJobStatus,
    BackgroundRuntimeFields, ClientLeaseKind, ForegroundQuestionAnswer, HostCallError, OwnerState,
    SessionHostClient, SessionHostConnection, SessionOptionAppliesFrom,
};
use serde_json::Value;

use crate::core::downlink::{plan_permission_answer, plan_question_answer, AnswerPlan};
use crate::core::ids;
use crate::core::owner::{self, FollowAction, JobView, OpenAction, OwnerView};
use crate::ports::{
    AnswerOutcome, HostSignal, LocalSession, OpenRequest, ReplayPolicy, SessionLink, SessionPort,
};

/// Checked before a session is started or resumed, and before a remote
/// permission-mode change: the binary's unattended-launch authorization.
pub type LaunchGate = Arc<dyn Fn(&BackgroundRuntimeFields) -> anyhow::Result<()> + Send + Sync>;

/// How often the follower looks for an owner.
#[derive(Debug, Clone, Copy)]
pub struct HostTiming {
    /// While a worker is starting or went away.
    pub owner_poll: Duration,
    /// While somebody else holds the session.
    pub read_only_poll: Duration,
    /// Between two attempts to bring a worker back.
    pub revive_interval: Duration,
}

impl Default for HostTiming {
    fn default() -> Self {
        Self {
            owner_poll: Duration::from_millis(250),
            read_only_poll: Duration::from_secs(2),
            revive_interval: Duration::from_secs(5),
        }
    }
}

/// Bytes of the event log read per step of a backfill.
const BACKFILL_READ_BYTES: u64 = 1024 * 1024;

pub struct LocalSessionHost {
    client: SessionHostClient,
    runtime: BackgroundRuntimeFields,
    gate: LaunchGate,
    timing: HostTiming,
    pid: u32,
}

impl LocalSessionHost {
    pub fn new(
        client: SessionHostClient,
        runtime: BackgroundRuntimeFields,
        gate: LaunchGate,
        timing: HostTiming,
    ) -> Self {
        Self {
            client,
            runtime,
            gate,
            timing,
            pid: std::process::id(),
        }
    }

    fn store(&self) -> &rebon_session_host::BackgroundStore {
        self.client.store()
    }

    fn exe(&self) -> &Path {
        self.client.worker_exe()
    }

    fn lease_id(&self, session: &LocalSession) -> String {
        ids::lease_client_id(self.pid, &session.rebon_session_id)
    }

    fn home_job(&self, session_id: &str) -> anyhow::Result<Option<BackgroundJobState>> {
        let mut job = rebon_session_host::home_job_for_session(self.store(), session_id)?;
        if let Some(job) = job.as_mut() {
            // A worker that died without saying so reads as running
            // forever otherwise.
            self.store().reconcile_stale_pid(job)?;
        }
        Ok(job)
    }

    fn view(
        &self,
        cwd: &str,
        session_id: &str,
    ) -> anyhow::Result<(OwnerView, OwnerState, Option<BackgroundJobState>)> {
        let state = self.client.resolve_uncached(cwd, session_id);
        let (view, job) = match &state {
            OwnerState::OwnedReachable { .. } => (OwnerView::Reachable, None),
            OwnerState::OwnedUnreachable { .. } => (OwnerView::Unreachable, None),
            OwnerState::OwnedOpaque { .. } => (OwnerView::Opaque, None),
            OwnerState::Free => {
                let job = self.home_job(session_id)?;
                (OwnerView::Free(job_view(job.as_ref())), job)
            }
        };
        Ok((view, state, job))
    }

    fn new_session(&self, project: &str) -> anyhow::Result<LocalSession> {
        let hosted = rebon_session_host::start_hosted_session(
            self.store(),
            self.client.projects_root(),
            self.runtime.clone(),
            project,
            self.exe(),
        )?;
        let link = SessionLink::new(Some(hosted.job_id.clone()));
        // A new job: its event log starts empty.
        link.set_backfill_offset(hosted.job_id, 0);
        Ok(LocalSession {
            rebon_session_id: hosted.session_id,
            cwd: project.to_string(),
            replay: ReplayPolicy::FromStart,
            link: Arc::new(link),
        })
    }

    fn existing_session(&self, project: &str, session_id: &str) -> anyhow::Result<LocalSession> {
        let home = self.home_job(session_id)?;
        let transcript_here =
            rebon_session::transcript_file_path(self.client.projects_root(), project, session_id)
                .is_file();
        let job_here = home
            .as_ref()
            .is_some_and(|job| job.cwd() == project || job_cwd_is_worktree_of(job, project));
        if !transcript_here && !job_here {
            anyhow::bail!("session {session_id} is not a session of {project} on this machine");
        }
        // The owner is keyed by the cwd the transcript lives under, which
        // for a job that ran in a worktree is the worktree.
        let cwd = if transcript_here {
            project.to_string()
        } else {
            home.as_ref()
                .map(|job| job.cwd().to_string())
                .unwrap_or_else(|| project.to_string())
        };
        let (view, state, job) = self.view(&cwd, session_id)?;
        let job = job.or(home);
        let job_id = match &state {
            OwnerState::OwnedReachable { owner } => owner.job_id.clone(),
            _ => None,
        }
        .or_else(|| job.as_ref().map(|job| job.job_id().to_string()));
        let replay = match owner::open_action(view) {
            OpenAction::Attach => ReplayPolicy::LiveOnly,
            OpenAction::AwaitWorker => ReplayPolicy::FromStart,
            OpenAction::Revive => {
                let job = job.as_ref().expect("a revive has a job");
                rebon_session_host::attach_background_job_in_store(
                    self.store(),
                    job.job_id(),
                    true,
                    self.exe(),
                )?;
                ReplayPolicy::FromStart
            }
            OpenAction::Host => {
                let job_id = rebon_session_host::host_existing_session(
                    self.store(),
                    session_id,
                    &cwd,
                    self.runtime.clone(),
                    true,
                    self.exe(),
                )?;
                let link = SessionLink::new(Some(job_id.clone()));
                link.set_backfill_offset(job_id, 0);
                return Ok(LocalSession {
                    rebon_session_id: session_id.to_string(),
                    cwd,
                    replay: ReplayPolicy::FromStart,
                    link: Arc::new(link),
                });
            }
            OpenAction::Refuse(refusal) => anyhow::bail!(refusal.describe()),
        };
        let link = SessionLink::new(job_id.clone());
        if let Some(job_id) = job_id {
            // Old lines of a reused job are not this item's to backfill.
            let offset = self.store().events_len(&job_id).unwrap_or(0);
            link.set_backfill_offset(job_id, offset);
        }
        Ok(LocalSession {
            rebon_session_id: session_id.to_string(),
            cwd,
            replay,
            link: Arc::new(link),
        })
    }

    /// Nobody holds the session: bring a worker to it, at most once per
    /// revive interval.
    fn bring_a_worker_back(
        &self,
        session: &LocalSession,
        action: FollowAction,
        last_revive: &mut Instant,
    ) {
        if last_revive.elapsed() < self.timing.revive_interval {
            return;
        }
        *last_revive = Instant::now();
        let result = match action {
            FollowAction::Host => rebon_session_host::host_existing_session(
                self.store(),
                &session.rebon_session_id,
                &session.cwd,
                self.runtime.clone(),
                true,
                self.exe(),
            )
            .map(|job_id| session.link.set_job_id(job_id)),
            FollowAction::Revive => match session.link.job_id() {
                Some(job_id) => rebon_session_host::attach_background_job_in_store(
                    self.store(),
                    &job_id,
                    true,
                    self.exe(),
                )
                .map(|_| ()),
                None => Ok(()),
            },
            _ => Ok(()),
        };
        if let Err(error) = result {
            tracing::warn!(
                session_id = %session.rebon_session_id,
                %error,
                "rebon rc: could not bring the session's worker back"
            );
        }
    }

    /// One connection's worth of following. Returns once the subscription
    /// ends; `false` when the caller is gone and following should stop.
    fn follow_owner(
        &self,
        session: &LocalSession,
        owner: rebon_session_host::OwnerHandle,
        since: &mut Option<u64>,
        signals: &tokio::sync::mpsc::Sender<HostSignal>,
    ) -> bool {
        let generation = ids::endpoint_generation(&owner.endpoint());
        if let Some(job_id) = owner.job_id.clone() {
            session.link.set_job_id(job_id);
        }
        let connection = Arc::new(SessionHostConnection::new(owner.clone()));
        connection.hold_lease(&self.lease_id(session), ClientLeaseKind::Serve);
        let stream = match owner.subscribe(*since) {
            Ok(stream) => stream,
            Err(error) => {
                tracing::debug!(
                    session_id = %session.rebon_session_id,
                    %error,
                    "rebon rc: could not subscribe to the session's owner"
                );
                connection.close();
                return true;
            }
        };
        session
            .link
            .attach(Arc::clone(&connection), stream.closer());
        if session.link.stopped() {
            session.link.detach();
            return false;
        }
        if signals
            .blocking_send(HostSignal::Attached { generation })
            .is_err()
        {
            session.link.detach();
            return false;
        }
        for event in stream {
            match &event {
                rebon_session_host::SessionEvent::Gap { to, .. } => {
                    *since = Some(to.saturating_sub(1));
                }
                other => {
                    if let Some(cursor) = other.cursor() {
                        *since = Some(cursor);
                    }
                }
            }
            // Blocking on purpose: a runner that cannot keep up stops
            // reading, the owner drops this subscriber after its backlog,
            // and the gap it reports on the next subscription is
            // backfilled from the event log.
            if signals.blocking_send(HostSignal::Event(event)).is_err() {
                session.link.detach();
                return false;
            }
            if session.link.stopped() {
                break;
            }
        }
        session.link.detach();
        !session.link.stopped() && signals.blocking_send(HostSignal::Detached).is_ok()
    }

    fn live_connection(
        &self,
        session: &LocalSession,
    ) -> anyhow::Result<Arc<SessionHostConnection>> {
        session
            .link
            .connection()
            .context("the session's host is not reachable right now")
    }
}

fn job_view(job: Option<&BackgroundJobState>) -> JobView {
    match job {
        None => JobView::None,
        Some(job) if job.status() == BackgroundJobStatus::Stopped => JobView::Stopped,
        Some(job) if job.pid().is_some() || job.status() == BackgroundJobStatus::Queued => {
            JobView::Starting
        }
        Some(_) => JobView::Gone,
    }
}

/// Whether a job ran in a worktree of `project`: its recorded cwd is the
/// worktree, and the worktree hangs off the project.
fn job_cwd_is_worktree_of(job: &BackgroundJobState, project: &str) -> bool {
    job.worktree_path().is_some()
        && job.cwd() != project
        && rebon_tools_core::strip_windows_verbatim_prefix(PathBuf::from(job.cwd()))
            .starts_with(project)
}

impl SessionPort for LocalSessionHost {
    fn open(&self, request: &OpenRequest) -> anyhow::Result<LocalSession> {
        (self.gate)(&self.runtime)?;
        match request.resume.as_deref() {
            None => self.new_session(&request.project),
            Some(session_id) => self.existing_session(&request.project, session_id),
        }
    }

    fn follow(&self, session: &LocalSession, signals: tokio::sync::mpsc::Sender<HostSignal>) {
        let mut since = match session.replay {
            ReplayPolicy::FromStart => Some(0),
            ReplayPolicy::LiveOnly => None,
        };
        let mut generation: Option<String> = None;
        let mut last_revive = Instant::now()
            .checked_sub(self.timing.revive_interval)
            .unwrap_or_else(Instant::now);
        let mut waiting_for: Option<String> = None;
        while !session.link.stopped() && !signals.is_closed() {
            let (view, state, _) = match self.view(&session.cwd, &session.rebon_session_id) {
                Ok(found) => found,
                Err(error) => {
                    tracing::warn!(%error, "rebon rc: could not read the session's job");
                    sleep_unless_stopped(&session.link, self.timing.owner_poll);
                    continue;
                }
            };
            let action = owner::follow_action(view);
            let pause = match action {
                FollowAction::Connect => {
                    let OwnerState::OwnedReachable { owner } = state else {
                        unreachable!("connect is only chosen for a reachable owner");
                    };
                    waiting_for = None;
                    let this = ids::endpoint_generation(&owner.endpoint());
                    if generation.as_deref() != Some(this.as_str()) {
                        // A different process numbers from one again, and
                        // all of it is new to this item.
                        if generation.is_some() {
                            since = Some(0);
                        }
                        generation = Some(this);
                    }
                    if !self.follow_owner(session, owner, &mut since, &signals) {
                        return;
                    }
                    self.timing.owner_poll
                }
                FollowAction::Wait => self.timing.owner_poll,
                FollowAction::WaitReadOnly(refusal) => {
                    let reason = refusal.describe().to_string();
                    if waiting_for.as_deref() != Some(reason.as_str())
                        && signals
                            .blocking_send(HostSignal::Waiting {
                                reason: reason.clone(),
                            })
                            .is_err()
                    {
                        return;
                    }
                    waiting_for = Some(reason);
                    self.timing.read_only_poll
                }
                FollowAction::Revive | FollowAction::Host => {
                    self.bring_a_worker_back(session, action, &mut last_revive);
                    self.timing.owner_poll
                }
                FollowAction::End(refusal) => {
                    let _ = signals.blocking_send(HostSignal::Ended {
                        reason: refusal.describe().to_string(),
                    });
                    return;
                }
            };
            sleep_unless_stopped(&session.link, pause);
        }
    }

    fn send_prompt(
        &self,
        session: &LocalSession,
        text: String,
        images: Vec<BackgroundImageAttachment>,
    ) -> anyhow::Result<()> {
        let job_id = session.link.job_id();
        self.client
            .send_prompt(
                &session.cwd,
                &session.rebon_session_id,
                job_id.as_deref(),
                text,
                images,
            )
            .map(|_| ())
            .map_err(anyhow::Error::new)
    }

    fn cancel_turn(&self, session: &LocalSession) -> anyhow::Result<bool> {
        // `cancel_turn`, never a stop: a stop ends the worker, and the
        // controller asked for the turn.
        self.live_connection(session)?.owner().cancel_turn()
    }

    fn set_model(
        &self,
        session: &LocalSession,
        model: &str,
    ) -> anyhow::Result<SessionOptionAppliesFrom> {
        self.live_connection(session)?
            .owner()
            .set_session_option("model", model)
    }

    fn set_permission_mode(&self, session: &LocalSession, mode: &str) -> anyhow::Result<()> {
        // A remote controller gets no more than an unattended launch would:
        // a mode the user has not accepted for background sessions is
        // refused here, before the owner is asked.
        let mut runtime = self.runtime.clone();
        runtime.permission_mode = Some(mode.to_string());
        (self.gate)(&runtime)?;
        self.live_connection(session)?
            .owner()
            .set_permission_mode(mode)
    }

    fn answer_permission(
        &self,
        session: &LocalSession,
        request_id: &str,
        option: RebonPermissionOption,
    ) -> anyhow::Result<AnswerOutcome> {
        let connection = self.live_connection(session)?;
        let status = connection.status().map_err(anyhow::Error::new)?;
        let plan = plan_permission_answer(status.pending_permission.as_ref(), request_id, option)
            .map_err(anyhow::Error::msg)?;
        let AnswerPlan::Permission {
            query_id,
            option_id,
        } = plan
        else {
            return Ok(AnswerOutcome::NotPending);
        };
        match connection
            .answer_permission(query_id, option_id.as_deref(), None)
            .map_err(anyhow::Error::new)?
        {
            rebon_session_host::PermissionOutcome::Applied(_) => Ok(AnswerOutcome::Applied),
            rebon_session_host::PermissionOutcome::AlreadyResolved => Ok(AnswerOutcome::NotPending),
        }
    }

    fn answer_question(
        &self,
        session: &LocalSession,
        request_id: &str,
        answers: Vec<ForegroundQuestionAnswer>,
    ) -> anyhow::Result<AnswerOutcome> {
        let connection = self.live_connection(session)?;
        let status = connection.status().map_err(anyhow::Error::new)?;
        let plan = plan_question_answer(status.pending_permission.as_ref(), request_id, &answers)
            .map_err(anyhow::Error::msg)?;
        let AnswerPlan::Questions {
            query_id,
            turn_generation,
        } = plan
        else {
            return Ok(AnswerOutcome::NotPending);
        };
        // The request the terminal and the ACP surface already send: the
        // owner checks the fence and the answers again, and builds the
        // tool input from them.
        match connection.call(BackgroundIpcRequest::AnswerQuestions {
            query_id,
            turn_generation,
            answers,
        }) {
            Ok(_) => Ok(AnswerOutcome::Applied),
            // The prompt was answered or replaced between the status read
            // and the call.
            Err(HostCallError::StaleGeneration) => Ok(AnswerOutcome::NotPending),
            Err(error) => Err(anyhow::Error::new(error)),
        }
    }

    fn backfill(
        &self,
        session: &LocalSession,
        epoch: u64,
        after: u64,
        before: u64,
    ) -> anyhow::Result<Vec<(u64, Value)>> {
        let Some(job_id) = session.link.job_id() else {
            return Ok(Vec::new());
        };
        let mut offset = match session.link.backfill_offset() {
            Some((known, offset)) if known == job_id => offset,
            _ => 0,
        };
        let mut lines = Vec::new();
        loop {
            let (events, next) = self.store().read_events_from_offset_bounded(
                &job_id,
                offset,
                BACKFILL_READ_BYTES,
            )?;
            if next < offset {
                // The log was replaced under us: start over on the new one.
                offset = 0;
                continue;
            }
            for event in events {
                if event.kind != "session_update" {
                    continue;
                }
                let Some(stamp) = event.stream_stamp() else {
                    continue;
                };
                if stamp.epoch != epoch || stamp.cursor <= after || stamp.cursor >= before {
                    continue;
                }
                let mut update = event.data;
                if let Some(object) = update.as_object_mut() {
                    object.remove("streamEpoch");
                    object.remove("streamCursor");
                }
                lines.push((stamp.cursor, update));
            }
            if next == offset {
                break;
            }
            offset = next;
        }
        session.link.set_backfill_offset(job_id, offset);
        Ok(lines)
    }
}

fn sleep_unless_stopped(link: &SessionLink, pause: Duration) {
    let deadline = Instant::now() + pause;
    while !link.stopped() {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_session_host::BackgroundStore;

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

    struct Fixture {
        _home: tempfile::TempDir,
        store: BackgroundStore,
        host: LocalSessionHost,
        project: String,
    }

    /// A host whose executable does not exist: nothing it could spawn
    /// would start, so a test never launches a worker.
    fn fixture(gate: LaunchGate) -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(home.path().join("jobs"));
        let project = home.path().join("app");
        std::fs::create_dir_all(&project).unwrap();
        let client = SessionHostClient::new(
            store.clone(),
            home.path().join("projects"),
            home.path().join("missing").join("rebon-test-never-run.exe"),
        );
        Fixture {
            project: project.to_string_lossy().into_owned(),
            host: LocalSessionHost::new(client, runtime(), gate, HostTiming::default()),
            store,
            _home: home,
        }
    }

    fn open_gate() -> LaunchGate {
        Arc::new(|_| Ok(()))
    }

    fn refusing_gate() -> LaunchGate {
        Arc::new(
            |runtime: &BackgroundRuntimeFields| match runtime.permission_mode.as_deref() {
                Some("bypassPermissions") => {
                    anyhow::bail!("bypassPermissions has not been accepted for unattended sessions")
                }
                _ => Ok(()),
            },
        )
    }

    fn local_session(fixture: &Fixture, job_id: Option<String>) -> LocalSession {
        LocalSession {
            rebon_session_id: "sess-local".into(),
            cwd: fixture.project.clone(),
            replay: ReplayPolicy::LiveOnly,
            link: Arc::new(SessionLink::new(job_id)),
        }
    }

    #[test]
    fn a_closed_gate_opens_nothing() {
        let fixture = fixture(Arc::new(|_| anyhow::bail!("background jobs are disabled")));
        let error = fixture
            .host
            .open(&OpenRequest {
                project: fixture.project.clone(),
                resume: None,
            })
            .unwrap_err();
        assert!(error.to_string().contains("disabled"));
        assert!(fixture.store.list_jobs().unwrap().is_empty());
    }

    #[test]
    fn a_session_of_another_project_is_not_resumed() {
        let fixture = fixture(open_gate());
        let error = fixture
            .host
            .open(&OpenRequest {
                project: fixture.project.clone(),
                resume: Some("sess-elsewhere".into()),
            })
            .unwrap_err();
        assert!(error.to_string().contains("not a session of"), "{error}");
        assert!(fixture.store.list_jobs().unwrap().is_empty());
    }

    #[test]
    fn a_stopped_session_is_not_revived() {
        let fixture = fixture(open_gate());
        let mut job = fixture
            .store
            .create_job("x".into(), PathBuf::from(&fixture.project), runtime())
            .unwrap();
        job.identity.session_id = Some("sess-stopped".into());
        job.process.status = BackgroundJobStatus::Stopped;
        fixture.store.write_state(&job).unwrap();
        let error = fixture
            .host
            .open(&OpenRequest {
                project: fixture.project.clone(),
                resume: Some("sess-stopped".into()),
            })
            .unwrap_err();
        assert!(error.to_string().contains("stopped"), "{error}");
        let after = fixture.store.read_state(job.job_id()).unwrap();
        assert_eq!(after.status(), BackgroundJobStatus::Stopped);
        assert!(after.pid().is_none());
    }

    #[test]
    fn job_views_follow_the_record() {
        let fixture = fixture(open_gate());
        assert_eq!(job_view(None), JobView::None);
        let mut job = fixture
            .store
            .create_job("x".into(), PathBuf::from("."), runtime())
            .unwrap();
        job.process.status = BackgroundJobStatus::Queued;
        assert_eq!(job_view(Some(&job)), JobView::Starting);
        job.process.status = BackgroundJobStatus::Idle;
        assert_eq!(job_view(Some(&job)), JobView::Gone);
        job.process.pid = Some(1);
        assert_eq!(job_view(Some(&job)), JobView::Starting);
        job.process.status = BackgroundJobStatus::Stopped;
        assert_eq!(job_view(Some(&job)), JobView::Stopped);
    }

    #[test]
    fn commands_without_a_live_owner_say_so() {
        let fixture = fixture(refusing_gate());
        let session = local_session(&fixture, None);
        assert!(fixture.host.cancel_turn(&session).is_err());
        assert!(fixture.host.set_model(&session, "m").is_err());
        assert!(fixture
            .host
            .answer_permission(&session, "perm-x", RebonPermissionOption::AllowOnce)
            .is_err());
        let unanswered = fixture
            .host
            .answer_question(
                &session,
                "perm-x",
                vec![ForegroundQuestionAnswer {
                    selected_options: vec![0],
                    other_text: None,
                }],
            )
            .unwrap_err();
        assert!(!unanswered.to_string().is_empty());
        // The gate is asked before the owner: a refused mode never reaches
        // it, whatever the owner's state.
        let refused = fixture
            .host
            .set_permission_mode(&session, "bypassPermissions")
            .unwrap_err();
        assert!(
            refused.to_string().contains("not been accepted"),
            "{refused}"
        );
        let unreachable = fixture
            .host
            .set_permission_mode(&session, "plan")
            .unwrap_err();
        assert!(
            unreachable.to_string().contains("not reachable"),
            "{unreachable}"
        );
    }

    #[test]
    fn a_prompt_for_a_session_with_no_job_is_refused_not_lost() {
        let fixture = fixture(open_gate());
        let error = fixture
            .host
            .send_prompt(&local_session(&fixture, None), "hi".into(), Vec::new())
            .unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn a_backfill_reads_only_the_stamped_range_and_remembers_where_it_got() {
        let fixture = fixture(open_gate());
        let job = fixture
            .store
            .create_job("x".into(), PathBuf::from(&fixture.project), runtime())
            .unwrap();
        let job_id = job.job_id().to_string();
        let line = |epoch: u64, cursor: u64| {
            serde_json::json!({
                "sessionId": "sess-local",
                "update": {"sessionUpdate": "plan", "n": cursor},
                "turnGeneration": 1,
                "streamEpoch": epoch,
                "streamCursor": cursor,
            })
        };
        fixture
            .store
            .append_event(&job_id, "session_update", line(9, 1))
            .unwrap();
        fixture
            .store
            .append_event(&job_id, "session_update", line(8, 2))
            .unwrap();
        fixture
            .store
            .append_event(&job_id, "turn_started", serde_json::json!({}))
            .unwrap();
        fixture
            .store
            .append_event(&job_id, "session_update", line(9, 3))
            .unwrap();
        fixture
            .store
            .append_event(&job_id, "session_update", serde_json::json!({"update": {}}))
            .unwrap();
        fixture
            .store
            .append_event(&job_id, "session_update", line(9, 4))
            .unwrap();
        fixture
            .store
            .append_event(&job_id, "session_update", line(9, 7))
            .unwrap();

        let session = local_session(&fixture, Some(job_id.clone()));
        let lines = fixture.host.backfill(&session, 9, 1, 7).unwrap();
        let cursors: Vec<u64> = lines.iter().map(|(cursor, _)| *cursor).collect();
        assert_eq!(cursors, vec![3, 4]);
        assert!(lines[0].1.get("streamEpoch").is_none());
        assert_eq!(lines[0].1["turnGeneration"], 1);
        assert_eq!(lines[0].1["update"]["n"], 3);

        // The next backfill starts where this one stopped.
        let (known, offset) = session.link.backfill_offset().unwrap();
        assert_eq!(known, job_id);
        assert!(offset > 0);
        fixture
            .store
            .append_event(&job_id, "session_update", line(9, 8))
            .unwrap();
        let later = fixture.host.backfill(&session, 9, 7, 10).unwrap();
        assert_eq!(later.iter().map(|(c, _)| *c).collect::<Vec<_>>(), vec![8]);
        // No job, nothing to read.
        assert!(fixture
            .host
            .backfill(&local_session(&fixture, None), 9, 0, 100)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_worktree_job_belongs_to_its_project() {
        let fixture = fixture(open_gate());
        let mut job = fixture
            .store
            .create_job(
                "x".into(),
                PathBuf::from(format!("{}/.rebon/worktrees/w1", fixture.project)),
                runtime(),
            )
            .unwrap();
        assert!(!job_cwd_is_worktree_of(&job, &fixture.project));
        job.workspace.worktree_path = Some(job.identity.cwd.clone());
        assert!(job_cwd_is_worktree_of(&job, &fixture.project));
        assert!(!job_cwd_is_worktree_of(&job, "/somewhere/else"));
    }
}
