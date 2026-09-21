//! The seams between the runner's orchestration and the two things it
//! drives: the RC server, and the machine's session host.
//!
//! The real implementations are [`crate::transport`] (HTTP + WebSocket) and
//! [`crate::host`] (`rebon-session-host`). Tests put in-process doubles
//! behind the same traits, so no test here spawns a worker or opens a
//! socket.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rebon_bridge::api_client::{BridgeApiClient, BridgeApiResult};
use rebon_bridge::projects::ProjectInfo;
use rebon_bridge::remote_permission::RebonPermissionOption;
use rebon_bridge::session_stream::SessionFrame;
use rebon_bridge::stream_client::{CloseReason, SessionStreamError};
use rebon_session_host::{
    BackgroundImageAttachment, ForegroundQuestionAnswer, SessionEvent, SessionHostConnection,
    SessionOptionAppliesFrom, SessionStreamCloser,
};
use serde_json::Value;

// ─── RC server ─────────────────────────────────────────────────────────

/// The environment side of the RC API: the trait every bridge transport
/// implements, plus the two things only a concrete client has.
#[async_trait]
pub trait EnvironmentApi: BridgeApiClient {
    /// Replace the projects this environment advertises.
    async fn update_projects(
        &self,
        environment_id: &str,
        projects: &[ProjectInfo],
    ) -> BridgeApiResult<()>;

    /// Hold `secret` as the environment secret for the calls that take
    /// none. A re-registration rotates it; every holder must follow.
    fn set_environment_secret(&self, secret: &str);
}

/// Sends frames on one session-stream connection.
#[async_trait]
pub trait FrameSender: Send + Sync {
    async fn send(&self, frame: &SessionFrame) -> Result<(), SessionStreamError>;
    /// Close with 1000. Best effort.
    async fn close(&self);
}

/// Reads frames from one session-stream connection.
#[async_trait]
pub trait FrameReceiver: Send {
    /// The next frame, or `None` once the connection ended.
    async fn recv(&mut self) -> Option<Result<SessionFrame, SessionStreamError>>;
    /// Why it ended, once `recv` returned `None`.
    fn close_reason(&self) -> Option<CloseReason>;
}

/// Opens session-stream connections.
#[async_trait]
pub trait StreamConnector: Send + Sync {
    async fn connect(
        &self,
        ingress_url: &str,
        token: &str,
    ) -> Result<(Arc<dyn FrameSender>, Box<dyn FrameReceiver>), SessionStreamError>;
}

// ─── Session host ──────────────────────────────────────────────────────

/// Where a work item's session should come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    pub project: String,
    /// An existing local session, or `None` for a new one.
    pub resume: Option<String>,
}

/// Which of an owner's events a first subscription should deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayPolicy {
    /// Everything the owner still holds: the worker was started (or
    /// revived) for this work item, so all of it is this item's.
    FromStart,
    /// Only what happens from now on: the owner was already running, and
    /// what it did before is not this item's to upload.
    LiveOnly,
}

/// One session this runner has open.
#[derive(Debug)]
pub struct LocalSession {
    pub rebon_session_id: String,
    /// The cwd the session's transcript is keyed by.
    pub cwd: String,
    pub replay: ReplayPolicy,
    pub link: Arc<SessionLink>,
}

/// The live half of a [`LocalSession`], shared by the follower thread and
/// the commands.
#[derive(Default)]
pub struct SessionLink {
    stop: AtomicBool,
    deliberate: AtomicBool,
    job_id: Mutex<Option<String>>,
    connection: Mutex<Option<Arc<SessionHostConnection>>>,
    closer: Mutex<Option<SessionStreamCloser>>,
    /// `(job id, byte offset)` the event log has been read to.
    backfill: Mutex<Option<(String, u64)>>,
}

impl std::fmt::Debug for SessionLink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionLink")
            .field("stopped", &self.stopped())
            .field("job_id", &self.job_id())
            .finish_non_exhaustive()
    }
}

impl SessionLink {
    pub fn new(job_id: Option<String>) -> Self {
        Self {
            job_id: Mutex::new(job_id),
            ..Self::default()
        }
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn deliberate(&self) -> bool {
        self.deliberate.load(Ordering::Relaxed)
    }

    pub fn job_id(&self) -> Option<String> {
        self.job_id
            .lock()
            .expect("session link job poisoned")
            .clone()
    }

    pub fn set_job_id(&self, job_id: String) {
        *self.job_id.lock().expect("session link job poisoned") = Some(job_id);
    }

    /// The live connection to the owner, if there is one.
    pub fn connection(&self) -> Option<Arc<SessionHostConnection>> {
        self.connection
            .lock()
            .expect("session link connection poisoned")
            .clone()
    }

    pub fn attach(&self, connection: Arc<SessionHostConnection>, closer: SessionStreamCloser) {
        *self
            .connection
            .lock()
            .expect("session link connection poisoned") = Some(connection);
        *self.closer.lock().expect("session link closer poisoned") = Some(closer);
    }

    /// Drop the live connection: the subscription ends and the lease is
    /// released (fast, when the release was marked deliberate).
    pub fn detach(&self) {
        if let Some(closer) = self
            .closer
            .lock()
            .expect("session link closer poisoned")
            .take()
        {
            closer.close();
        }
        let connection = self
            .connection
            .lock()
            .expect("session link connection poisoned")
            .take();
        if let Some(connection) = connection {
            if self.deliberate() {
                connection.mark_lease_deliberate();
            }
            connection.close();
        }
    }

    /// Stop following. `deliberate` says the session is finished rather
    /// than handed on, so its worker need not linger for this client.
    pub fn stop(&self, deliberate: bool) {
        self.deliberate.store(deliberate, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        self.detach();
    }

    pub fn backfill_offset(&self) -> Option<(String, u64)> {
        self.backfill
            .lock()
            .expect("session link backfill poisoned")
            .clone()
    }

    pub fn set_backfill_offset(&self, job_id: String, offset: u64) {
        *self
            .backfill
            .lock()
            .expect("session link backfill poisoned") = Some((job_id, offset));
    }
}

/// What the follower thread reports.
#[derive(Debug, Clone, PartialEq)]
pub enum HostSignal {
    /// Subscribed to the owner endpoint `generation`.
    Attached {
        generation: String,
    },
    Event(SessionEvent),
    /// The subscription ended; the follower is looking for the owner again.
    Detached,
    /// Somebody else holds the session; the follower waits.
    Waiting {
        reason: String,
    },
    /// The session is over on this machine (stopped by the user). The
    /// follower has returned.
    Ended {
        reason: String,
    },
}

/// Whether a permission answer landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerOutcome {
    Applied,
    /// The owner is not parked on that prompt (any more).
    NotPending,
}

/// The session host, as the runner uses it. Every method blocks (file
/// locks, loopback IPC); the orchestration calls them on the blocking pool.
pub trait SessionPort: Send + Sync + 'static {
    /// Start or resume the session a work item names, and queue nothing
    /// yet. Checked against the unattended-launch gate.
    fn open(&self, request: &OpenRequest) -> anyhow::Result<LocalSession>;

    /// Follow the session's owner until `session.link` is stopped or
    /// `signals` is closed: connect, hold a lease, subscribe, forward
    /// every event, and find the owner again when it goes.
    fn follow(&self, session: &LocalSession, signals: tokio::sync::mpsc::Sender<HostSignal>);

    fn send_prompt(
        &self,
        session: &LocalSession,
        text: String,
        images: Vec<BackgroundImageAttachment>,
    ) -> anyhow::Result<()>;

    /// Interrupt the running turn. `Ok(false)`: nothing was running.
    fn cancel_turn(&self, session: &LocalSession) -> anyhow::Result<bool>;

    fn set_model(
        &self,
        session: &LocalSession,
        model: &str,
    ) -> anyhow::Result<SessionOptionAppliesFrom>;

    /// Put `mode` in force, after the unattended-launch authorization gate
    /// has accepted it.
    fn set_permission_mode(&self, session: &LocalSession, mode: &str) -> anyhow::Result<()>;

    /// Answer the prompt whose request id is `request_id` — recomputed
    /// from the prompt the owner holds now — with the option of kind
    /// `option`. A question can only be declined this way
    /// ([`crate::core::downlink::plan_permission_answer`]).
    fn answer_permission(
        &self,
        session: &LocalSession,
        request_id: &str,
        option: RebonPermissionOption,
    ) -> anyhow::Result<AnswerOutcome>;

    /// Answer the question prompt whose request id is `request_id` with
    /// `answers`, one per question. `Err` when it is not a question, the
    /// answers do not fit it, or the owner cannot be reached; the prompt
    /// stays pending.
    fn answer_question(
        &self,
        session: &LocalSession,
        request_id: &str,
        answers: Vec<ForegroundQuestionAnswer>,
    ) -> anyhow::Result<AnswerOutcome>;

    /// The session updates the job's event log holds, stamped `epoch`,
    /// with a cursor strictly between `after` and `before`.
    fn backfill(
        &self,
        session: &LocalSession,
        epoch: u64,
        after: u64,
        before: u64,
    ) -> anyhow::Result<Vec<(u64, Value)>>;
}
