//! Running one work item: acknowledge it, open its session, and pump both
//! ways until something ends it.
//!
//! Five tasks per session item:
//!
//! * **the pump** (this module's loop) owns the uplink state, reads the
//!   stream, and decides;
//! * **the writer** sends frames in order on whichever connection is
//!   current, resends the recently keyed ones on a new connection, and
//!   reports a failed send;
//! * **the heartbeat** extends the work lease and says when it is lost;
//! * **the follower** (on the blocking pool) follows the session's owner
//!   and forwards its events ([`crate::ports::SessionPort::follow`]);
//! * **the prompter** hands prompts to the session one at a time, in the
//!   order they arrived.
//!
//! ## Prompt order
//!
//! RC routes a session's prompts in the order it received them, and the
//! session host queues a prompt that arrives during a turn behind it. The
//! one place that order could be lost is here: each delivery is a
//! blocking call, and two of them on the blocking pool at once may reach
//! the owner in either order. So prompts go through one queue and one
//! task, and the next is not sent until the previous call has returned.
//!
//! ## Back-pressure
//!
//! The owner drops a subscriber that falls 512 events behind. The pump
//! turns events into frames in memory and hands them to the writer
//! through an unbounded queue whose *bytes* are counted: while the queue
//! holds less than [`WorkTiming::outbox_max_bytes`], a slow uplink slows
//! nothing on the owner side. Past it the pump stops reading the follower;
//! the owner eventually drops the subscription, the follower resubscribes
//! from its last cursor, and the gap the owner reports is backfilled from
//! the job's event log once the queue has drained.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rebon_bridge::remote_permission::RemotePermissionPolicy;
use rebon_bridge::session_stream::{SessionFrame, SessionRunState};
use rebon_bridge::stream_client::{CloseReason, SessionStreamError};
use rebon_bridge::work_secret::WorkSecret;
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::core::downlink::{self, ControlCommand, Inbound};
use crate::core::state::Reported;
use crate::core::uplink::{PermissionProjection, Uplink, UplinkOut};
use crate::core::work::{
    after_close, after_heartbeat, after_stream_error, Backoff, Exit, LeaseVerdict, ResumeTarget,
    SessionPlan, StreamVerdict, WorkPlan,
};
use crate::ledger::{Ledger, SessionEntry};
use crate::ports::{
    AnswerOutcome, EnvironmentApi, FrameReceiver, FrameSender, HostSignal, LocalSession,
    OpenRequest, SessionPort, StreamConnector,
};

/// The knobs of one work item's run.
#[derive(Debug, Clone, Copy)]
pub struct WorkTiming {
    /// How often the work lease is extended. Well inside RC's 90 s lease.
    pub heartbeat_interval: Duration,
    /// How long heartbeats may fail without an answer before the lease is
    /// taken as lost (RC reclaims it at 90 s by default).
    pub lease_grace: Duration,
    /// How long merged text waits before it is sent anyway.
    pub flush_interval: Duration,
    pub reconnect_base: Duration,
    pub reconnect_max: Duration,
    /// Bytes of frames the writer may have queued before the pump stops
    /// reading the owner.
    pub outbox_max_bytes: usize,
    /// How long a finishing item waits for its last frames to go out.
    pub drain_timeout: Duration,
}

impl Default for WorkTiming {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(20),
            lease_grace: Duration::from_secs(80),
            flush_interval: Duration::from_millis(250),
            reconnect_base: Duration::from_millis(500),
            reconnect_max: Duration::from_secs(30),
            outbox_max_bytes: 32 * 1024 * 1024,
            drain_timeout: Duration::from_secs(5),
        }
    }
}

/// How many recently sent keyed frames a new connection is sent again.
/// RC stores each once, so the only cost of a larger ring is bytes.
const RESEND_RING: usize = 64;

/// Buffer between the follower and the pump. Larger than the owner's own
/// subscriber backlog, so the pump, not the owner, is what applies
/// back-pressure first.
const SIGNAL_BUFFER: usize = 4096;

/// Everything a work item needs from the serve loop.
pub struct WorkContext {
    pub api: Arc<dyn EnvironmentApi>,
    pub streams: Arc<dyn StreamConnector>,
    pub host: Arc<dyn SessionPort>,
    pub ledger: Ledger,
    pub projection: PermissionProjection,
    pub environment_id: String,
    pub timing: WorkTiming,
    pub policy: RemotePermissionPolicy,
    pub pid: u32,
}

/// Why the serve loop wants an item to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    Running,
    /// The runner is exiting.
    Shutdown,
    /// A newer work item on this machine serves the same RC session.
    Superseded,
}

/// How an item ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkOutcome {
    pub work_id: String,
    pub rc_session_id: Option<String>,
    pub exit: Exit,
}

/// Run one planned work item to its end.
pub async fn run_work_item(
    ctx: Arc<WorkContext>,
    plan: WorkPlan,
    interrupt: watch::Receiver<Interrupt>,
) -> WorkOutcome {
    match plan {
        WorkPlan::Refuse { work_id, reason } => {
            tracing::warn!(%work_id, %reason, "rebon rc: refusing a work item");
            retire(&ctx, &work_id).await;
            WorkOutcome {
                work_id,
                rc_session_id: None,
                exit: Exit::Retire(reason),
            }
        }
        WorkPlan::Healthcheck { work_id, secret } => {
            if let Err(error) = ctx
                .api
                .acknowledge_work(&ctx.environment_id, &work_id, &secret.session_token)
                .await
            {
                tracing::warn!(%work_id, %error, "rebon rc: could not acknowledge a healthcheck");
            }
            retire(&ctx, &work_id).await;
            WorkOutcome {
                work_id,
                rc_session_id: None,
                exit: Exit::Retire("healthcheck answered".into()),
            }
        }
        WorkPlan::Session(plan) => {
            let work_id = plan.work_id.clone();
            let rc_session_id = plan.rc_session_id.clone();
            let exit = serve_session(&ctx, plan, interrupt).await;
            if exit.retires() {
                retire(&ctx, &work_id).await;
            }
            tracing::info!(%work_id, %rc_session_id, reason = exit.reason(), retired = exit.retires(), "rebon rc: work item ended");
            WorkOutcome {
                work_id,
                rc_session_id: Some(rc_session_id),
                exit,
            }
        }
    }
}

async fn retire(ctx: &WorkContext, work_id: &str) {
    if let Err(error) = ctx.api.stop_work(&ctx.environment_id, work_id, false).await {
        tracing::warn!(%work_id, %error, "rebon rc: could not stop a work item");
    }
}

async fn serve_session(
    ctx: &Arc<WorkContext>,
    plan: SessionPlan,
    interrupt: watch::Receiver<Interrupt>,
) -> Exit {
    if let Err(error) = ctx
        .api
        .acknowledge_work(
            &ctx.environment_id,
            &plan.work_id,
            &plan.secret.session_token,
        )
        .await
    {
        if !error.is_transient() {
            return Exit::StandDown(format!("the work item could not be acknowledged: {error}"));
        }
        // The heartbeat will say whether the lease is still ours.
        tracing::warn!(work_id = %plan.work_id, %error, "rebon rc: acknowledging a work item failed; carrying on");
    }
    let (lease_tx, lease_rx) = watch::channel(None);
    let heartbeat = tokio::spawn(heartbeat(
        Arc::clone(ctx),
        plan.work_id.clone(),
        plan.secret.session_token.clone(),
        lease_tx,
    ));
    let mut pump = Pump::start(Arc::clone(ctx), plan, interrupt, lease_rx);
    let exit = pump.run().await;
    heartbeat.abort();
    pump.finish().await;
    exit
}

async fn heartbeat(
    ctx: Arc<WorkContext>,
    work_id: String,
    token: String,
    lost: watch::Sender<Option<String>>,
) {
    let mut last_held = Instant::now();
    loop {
        tokio::time::sleep(ctx.timing.heartbeat_interval).await;
        let result = ctx
            .api
            .heartbeat_work(&ctx.environment_id, &work_id, &token)
            .await;
        match after_heartbeat(&result) {
            LeaseVerdict::Held => last_held = Instant::now(),
            LeaseVerdict::Lost(reason) => {
                let _ = lost.send(Some(reason));
                return;
            }
            LeaseVerdict::Unknown => {
                if last_held.elapsed() >= ctx.timing.lease_grace {
                    let _ = lost.send(Some(format!(
                        "no heartbeat succeeded for {}s",
                        ctx.timing.lease_grace.as_secs()
                    )));
                    return;
                }
            }
        }
    }
}

// ─── The writer ────────────────────────────────────────────────────────

type Sink = Option<(u64, Arc<dyn FrameSender>)>;

fn frame_bytes(frame: &SessionFrame) -> usize {
    serde_json::to_vec(frame)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

struct Writer {
    frames: mpsc::UnboundedSender<SessionFrame>,
    queued: Arc<AtomicUsize>,
    sink: watch::Sender<Sink>,
    failures: mpsc::UnboundedReceiver<(u64, SessionStreamError)>,
    task: JoinHandle<()>,
}

impl Writer {
    fn start() -> Self {
        let (frames, frames_rx) = mpsc::unbounded_channel();
        let (sink, sink_rx) = watch::channel(None);
        let (failures_tx, failures) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(write_frames(
            frames_rx,
            Arc::clone(&queued),
            sink_rx,
            failures_tx,
        ));
        Self {
            frames,
            queued,
            sink,
            failures,
            task,
        }
    }

    fn push(&self, frame: SessionFrame) {
        self.queued
            .fetch_add(frame_bytes(&frame), Ordering::Relaxed);
        if self.frames.send(frame).is_err() {
            tracing::warn!("rebon rc: the stream writer is gone; a frame was dropped");
        }
    }

    fn queued_bytes(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }
}

async fn write_frames(
    mut frames: mpsc::UnboundedReceiver<SessionFrame>,
    queued: Arc<AtomicUsize>,
    mut sink: watch::Receiver<Sink>,
    failures: mpsc::UnboundedSender<(u64, SessionStreamError)>,
) {
    let mut ring: VecDeque<SessionFrame> = VecDeque::new();
    let mut resent_for = 0u64;
    'frames: while let Some(frame) = frames.recv().await {
        'deliver: loop {
            let Some((generation, sender)) = current_sink(&mut sink).await else {
                return;
            };
            if generation != resent_for {
                for keyed in &ring {
                    if let Err(error) = sender.send(keyed).await {
                        let _ = failures.send((generation, error));
                        if !await_new_sink(&mut sink, generation).await {
                            return;
                        }
                        continue 'deliver;
                    }
                }
                resent_for = generation;
            }
            match sender.send(&frame).await {
                Ok(()) => break 'deliver,
                // A frame the client refused to encode or send as too big
                // would be refused again on any connection.
                Err(SessionStreamError::Protocol(reason)) => {
                    tracing::warn!(%reason, frame = frame.frame_type(), "rebon rc: a frame could not be sent and was dropped");
                    queued.fetch_sub(frame_bytes(&frame), Ordering::Relaxed);
                    continue 'frames;
                }
                Err(error) => {
                    let _ = failures.send((generation, error));
                    if !await_new_sink(&mut sink, generation).await {
                        return;
                    }
                }
            }
        }
        queued.fetch_sub(frame_bytes(&frame), Ordering::Relaxed);
        if frame.idempotency_key().is_some() {
            ring.push_back(frame);
            if ring.len() > RESEND_RING {
                ring.pop_front();
            }
        }
    }
}

/// The current connection, waiting for one if there is none. `None` when
/// the pump is gone.
async fn current_sink(sink: &mut watch::Receiver<Sink>) -> Option<(u64, Arc<dyn FrameSender>)> {
    loop {
        if let Some(current) = sink.borrow_and_update().clone() {
            return Some(current);
        }
        if sink.changed().await.is_err() {
            return None;
        }
    }
}

/// Wait until the connection is no longer `generation`. `false` when the
/// pump is gone.
async fn await_new_sink(sink: &mut watch::Receiver<Sink>, generation: u64) -> bool {
    loop {
        if sink
            .borrow_and_update()
            .as_ref()
            .is_some_and(|(current, _)| *current != generation)
        {
            return true;
        }
        if sink.changed().await.is_err() {
            return false;
        }
    }
}

// ─── The pump ──────────────────────────────────────────────────────────

/// One prompt on its way to the session.
struct PromptJob {
    local: Arc<LocalSession>,
    text: String,
    images: Vec<rebon_session_host::BackgroundImageAttachment>,
}

/// Deliver prompts in the order they were queued; see the module docs.
async fn deliver_prompts(
    mut jobs: mpsc::UnboundedReceiver<PromptJob>,
    host: Arc<dyn SessionPort>,
    done: mpsc::UnboundedSender<Done>,
) {
    while let Some(job) = jobs.recv().await {
        let host = Arc::clone(&host);
        let delivered =
            tokio::task::spawn_blocking(move || host.send_prompt(&job.local, job.text, job.images))
                .await
                .unwrap_or_else(|panic| {
                    Err(anyhow::anyhow!("delivering the prompt panicked: {panic}"))
                });
        if let Err(error) = delivered {
            let _ = done.send(Done::PromptFailed(format!(
                "the prompt was not delivered: {error:#}"
            )));
        }
    }
}

/// What a command on the blocking pool sends back.
enum Done {
    Frames(Vec<SessionFrame>),
    PromptFailed(String),
    Backfill {
        before: u64,
        lines: Vec<(u64, Value)>,
    },
}

type Connecting =
    JoinHandle<Result<(Arc<dyn FrameSender>, Box<dyn FrameReceiver>), SessionStreamError>>;

struct Pump {
    ctx: Arc<WorkContext>,
    plan: SessionPlan,
    interrupt: watch::Receiver<Interrupt>,
    lease: watch::Receiver<Option<String>>,
    writer: Writer,
    uplink: Uplink,
    local: Option<Arc<LocalSession>>,
    follower: Option<JoinHandle<()>>,
    signals: Option<mpsc::Receiver<HostSignal>>,
    done_tx: mpsc::UnboundedSender<Done>,
    done_rx: mpsc::UnboundedReceiver<Done>,
    prompts: mpsc::UnboundedSender<PromptJob>,
    prompter: JoinHandle<()>,
    receiver: Option<Box<dyn FrameReceiver>>,
    sender: Option<Arc<dyn FrameSender>>,
    generation: u64,
    connecting: Option<Connecting>,
    reconnect_at: Option<Instant>,
    backoff: Backoff,
}

impl Pump {
    fn start(
        ctx: Arc<WorkContext>,
        plan: SessionPlan,
        interrupt: watch::Receiver<Interrupt>,
        lease: watch::Receiver<Option<String>>,
    ) -> Self {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let backoff = Backoff::new(ctx.timing.reconnect_base, ctx.timing.reconnect_max);
        let uplink = Uplink::new(Arc::clone(&ctx.projection));
        let (prompts, jobs) = mpsc::unbounded_channel();
        let prompter = tokio::spawn(deliver_prompts(
            jobs,
            Arc::clone(&ctx.host),
            done_tx.clone(),
        ));
        let mut pump = Self {
            writer: Writer::start(),
            uplink,
            local: None,
            follower: None,
            signals: None,
            done_tx,
            done_rx,
            prompts,
            prompter,
            receiver: None,
            sender: None,
            generation: 0,
            connecting: None,
            reconnect_at: None,
            backoff,
            ctx,
            plan,
            interrupt,
            lease,
        };
        pump.connect();
        pump
    }

    fn connect(&mut self) {
        let streams = Arc::clone(&self.ctx.streams);
        let secret: WorkSecret = self.plan.secret.clone();
        self.connecting = Some(tokio::spawn(async move {
            streams
                .connect(&secret.ingress_url, &secret.session_token)
                .await
        }));
    }

    fn send_all(&self, outs: Vec<UplinkOut>) {
        for out in outs {
            match out {
                UplinkOut::Frame(frame) => self.writer.push(frame),
                UplinkOut::Backfill {
                    epoch,
                    after,
                    before,
                } => self.backfill(epoch, after, before),
            }
        }
    }

    fn backfill(&self, epoch: u64, after: u64, before: u64) {
        let (Some(local), host, done) = (
            self.local.clone(),
            Arc::clone(&self.ctx.host),
            self.done_tx.clone(),
        ) else {
            return;
        };
        tokio::task::spawn_blocking(move || {
            let lines = host
                .backfill(&local, epoch, after, before)
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "rebon rc: could not backfill missed updates");
                    Vec::new()
                });
            let _ = done.send(Done::Backfill { before, lines });
        });
    }

    fn report(&mut self, reported: Reported) {
        let outs = self.uplink.report(reported);
        self.send_all(outs);
    }

    async fn run(&mut self) -> Exit {
        self.report(Reported::new(SessionRunState::Starting));
        // Nothing is opened, and no prompt runs, before the server has
        // accepted this worker on the session: a lease that is already gone
        // is refused right here (409).
        if let Some(exit) = self.first_connection().await {
            return exit;
        }
        if let Some(exit) = self.open().await {
            return exit;
        }
        let mut flush = tokio::time::interval(self.ctx.timing.flush_interval);
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let saturated = self.writer.queued_bytes() > self.ctx.timing.outbox_max_bytes
                || self.uplink.held_bytes() > self.ctx.timing.outbox_max_bytes;
            let reconnect_at = self.reconnect_at;
            tokio::select! {
                biased;
                changed = self.interrupt.changed() => {
                    let interrupt = if changed.is_err() { Interrupt::Shutdown } else { *self.interrupt.borrow() };
                    match interrupt {
                        Interrupt::Running => {}
                        Interrupt::Shutdown => return Exit::StandDown("the runner is shutting down".into()),
                        Interrupt::Superseded => return Exit::Retire("a newer work item on this machine serves this session".into()),
                    }
                }
                changed = self.lease.changed() => {
                    if changed.is_err() {
                        // The heartbeat only ends by saying why, so this is
                        // one that died: nobody is keeping the lease.
                        return Exit::StandDown("the lease heartbeat stopped".into());
                    }
                    let lost = self.lease.borrow().clone();
                    if let Some(reason) = lost {
                        return Exit::StandDown(reason);
                    }
                }
                Some((generation, error)) = self.writer.failures.recv() => {
                    if generation == self.generation {
                        if let Some(exit) = self.disconnected(after_stream_error(&error)).await {
                            return exit;
                        }
                    }
                }
                connected = join_connecting(&mut self.connecting) => {
                    self.connecting = None;
                    match connected {
                        Ok((sender, receiver)) => self.connected(sender, receiver),
                        Err(error) => {
                            if let Some(exit) = self.disconnected(after_stream_error(&error)).await {
                                return exit;
                            }
                        }
                    }
                }
                frame = recv_frame(&mut self.receiver) => match frame {
                    Some(Ok(frame)) => self.inbound(frame),
                    Some(Err(error)) => {
                        tracing::warn!(%error, "rebon rc: an unreadable frame from the server was skipped");
                    }
                    None => {
                        let reason = self
                            .receiver
                            .take()
                            .and_then(|receiver| receiver.close_reason())
                            .unwrap_or(CloseReason::Network);
                        if let Some(exit) = self.disconnected(after_close(&reason)).await {
                            return exit;
                        }
                    }
                },
                Some(done) = self.done_rx.recv() => self.done(done),
                signal = recv_signal(&mut self.signals), if !saturated => match signal {
                    Some(signal) => {
                        if let Some(exit) = self.signal(signal) {
                            return exit;
                        }
                    }
                    None => {
                        self.signals = None;
                        return Exit::StandDown("the session follower stopped".into());
                    }
                },
                _ = sleep_until(reconnect_at), if reconnect_at.is_some() && self.connecting.is_none() => {
                    self.reconnect_at = None;
                    self.connect();
                }
                _ = flush.tick() => {
                    let outs = self.uplink.flush();
                    self.send_all(outs);
                }
            }
        }
    }

    /// Wait for the first stream connection. `Some` when the item ends
    /// before it.
    async fn first_connection(&mut self) -> Option<Exit> {
        loop {
            tokio::select! {
                biased;
                changed = self.interrupt.changed() => {
                    let interrupt = if changed.is_err() { Interrupt::Shutdown } else { *self.interrupt.borrow() };
                    match interrupt {
                        Interrupt::Running => {}
                        Interrupt::Shutdown => return Some(Exit::StandDown("the runner is shutting down".into())),
                        Interrupt::Superseded => return Some(Exit::Retire("a newer work item on this machine serves this session".into())),
                    }
                }
                changed = self.lease.changed() => {
                    if changed.is_err() {
                        return Some(Exit::StandDown("the lease heartbeat stopped".into()));
                    }
                    let lost = self.lease.borrow().clone();
                    if let Some(reason) = lost {
                        return Some(Exit::StandDown(reason));
                    }
                }
                connected = join_connecting(&mut self.connecting) => {
                    self.connecting = None;
                    match connected {
                        Ok((sender, receiver)) => {
                            self.connected(sender, receiver);
                            return None;
                        }
                        Err(error) => {
                            if let Some(exit) = self.disconnected(after_stream_error(&error)).await {
                                return Some(exit);
                            }
                        }
                    }
                }
                _ = sleep_until(self.reconnect_at), if self.reconnect_at.is_some() && self.connecting.is_none() => {
                    self.reconnect_at = None;
                    self.connect();
                }
            }
        }
    }

    /// Open the session, bind it, deliver the prompt, start following.
    /// `Some` when the item ends here.
    async fn open(&mut self) -> Option<Exit> {
        let request = OpenRequest {
            project: self.plan.project.clone(),
            resume: self.plan.resume.session_id().map(str::to_string),
        };
        let host = Arc::clone(&self.ctx.host);
        let opened = tokio::task::spawn_blocking(move || host.open(&request))
            .await
            .unwrap_or_else(|panic| Err(anyhow::anyhow!("opening the session panicked: {panic}")));
        let local = match opened {
            Ok(local) => Arc::new(local),
            Err(error) => {
                let detail = format!("the session could not be opened: {error:#}");
                tracing::warn!(work_id = %self.plan.work_id, %detail, "rebon rc: open failed");
                self.report(Reported::with_detail(
                    SessionRunState::Failed,
                    detail.clone(),
                ));
                return Some(Exit::Retire(detail));
            }
        };
        tracing::info!(
            work_id = %self.plan.work_id,
            rc_session = %self.plan.rc_session_id,
            session_id = %local.rebon_session_id,
            resume = ?self.plan.resume,
            "rebon rc: serving a session"
        );
        if let ResumeTarget::Requested(requested) | ResumeTarget::Remembered(requested) =
            &self.plan.resume
        {
            debug_assert_eq!(requested, &local.rebon_session_id);
        }
        let entry = SessionEntry {
            rebon_session_id: local.rebon_session_id.clone(),
            project: self.plan.project.clone(),
            cwd: local.cwd.clone(),
            job_id: local.link.job_id(),
            environment_id: self.ctx.environment_id.clone(),
            updated_at_ms: rebon_types::wall_clock_ms(),
        };
        if let Err(error) = self
            .ctx
            .ledger
            .record_session(&self.plan.rc_session_id, entry)
        {
            tracing::warn!(%error, "rebon rc: could not record the session in the ledger");
        }
        self.writer
            .push(SessionFrame::bound(local.rebon_session_id.clone()));
        self.local = Some(Arc::clone(&local));

        // Follow before prompting, so a turn the prompt starts on a live
        // owner is seen from its first event.
        let (signals_tx, signals_rx) = mpsc::channel(SIGNAL_BUFFER);
        let host = Arc::clone(&self.ctx.host);
        let follow = Arc::clone(&local);
        self.follower = Some(tokio::task::spawn_blocking(move || {
            host.follow(&follow, signals_tx)
        }));
        self.signals = Some(signals_rx);

        if let Some(prompt) = self.plan.prompt.clone() {
            self.deliver_initial_prompt(prompt);
        }
        None
    }

    fn deliver_initial_prompt(&self, prompt: String) {
        // Claimed before it is sent: an item handed out again after its
        // lease lapsed must not run its prompt twice.
        match self
            .ctx
            .ledger
            .claim_prompt(&self.plan.work_id, rebon_types::wall_clock_ms())
        {
            Ok(true) => self.prompt(prompt, Vec::new()),
            Ok(false) => tracing::info!(
                work_id = %self.plan.work_id,
                "rebon rc: this item's prompt already ran; resuming without it"
            ),
            Err(error) => {
                tracing::warn!(%error, "rebon rc: could not claim the prompt; not sending it");
                let _ = self.done_tx.send(Done::PromptFailed(format!(
                    "the prompt was not delivered: {error:#}"
                )));
            }
        }
    }

    fn prompt(&self, text: String, images: Vec<rebon_session_host::BackgroundImageAttachment>) {
        let Some(local) = self.local.clone() else {
            return;
        };
        if self
            .prompts
            .send(PromptJob {
                local,
                text,
                images,
            })
            .is_err()
        {
            tracing::warn!("rebon rc: the prompt queue is gone; a prompt was dropped");
        }
    }

    fn connected(&mut self, sender: Arc<dyn FrameSender>, receiver: Box<dyn FrameReceiver>) {
        self.generation += 1;
        self.backoff.reset();
        self.sender = Some(Arc::clone(&sender));
        self.receiver = Some(receiver);
        let _ = self.writer.sink.send(Some((self.generation, sender)));
        tracing::debug!(work_id = %self.plan.work_id, generation = self.generation, "rebon rc: session stream connected");
        if self.generation > 1 {
            if let Some(local) = &self.local {
                self.writer
                    .push(SessionFrame::bound(local.rebon_session_id.clone()));
            }
            for frame in self.uplink.resend_on_reconnect() {
                self.writer.push(frame);
            }
        }
    }

    /// The connection is gone. `Some` when the item ends with it.
    async fn disconnected(&mut self, verdict: StreamVerdict) -> Option<Exit> {
        let _ = self.writer.sink.send(None);
        self.receiver = None;
        if let Some(sender) = self.sender.take() {
            sender.close().await;
        }
        match verdict {
            StreamVerdict::Reconnect => {
                let delay = self.backoff.next_delay();
                tracing::info!(work_id = %self.plan.work_id, delay_ms = delay.as_millis() as u64, "rebon rc: session stream lost; reconnecting");
                self.reconnect_at = Some(Instant::now() + delay);
                None
            }
            StreamVerdict::Exit(exit) => Some(exit),
        }
    }

    fn inbound(&mut self, frame: SessionFrame) {
        match downlink::classify(frame, self.ctx.policy) {
            Inbound::Prompt {
                text,
                images,
                skipped_attachments,
            } => {
                if skipped_attachments > 0 {
                    tracing::warn!(
                        skipped_attachments,
                        "rebon rc: only image attachments can be passed to a session"
                    );
                }
                self.prompt(text, images);
            }
            Inbound::Cancel => {
                self.on_host(|host, local| {
                    match host.cancel_turn(local) {
                        Ok(cancelled) => tracing::info!(cancelled, "rebon rc: cancel requested"),
                        Err(error) => tracing::warn!(%error, "rebon rc: cancel refused"),
                    }
                    Vec::new()
                });
            }
            Inbound::Permission(command) => match command.decision {
                Err(reason) => self
                    .writer
                    .push(downlink::permission_refusal(&command.request_id, &reason)),
                Ok(answer) => self.answer(command.request_id, move |host, local, request_id| {
                    host.answer_permission(local, request_id, answer.option)
                }),
            },
            Inbound::Question(command) => match command.answers {
                Err(reason) => self
                    .writer
                    .push(downlink::permission_refusal(&command.request_id, &reason)),
                Ok(answers) => self.answer(command.request_id, move |host, local, request_id| {
                    host.answer_question(local, request_id, answers)
                }),
            },
            Inbound::Control(command) => {
                let pid = self.ctx.pid;
                self.on_host(move |host, local| {
                    let verdict = control_verdict(host, local, &command);
                    vec![downlink::control_response(&command, verdict, pid)]
                });
            }
            Inbound::Ignored(why) => tracing::debug!(why, "rebon rc: ignored a frame"),
        }
    }

    /// Apply a controller's answer to prompt `request_id`, and tell the
    /// controller when it did not land. An answer that lands says nothing:
    /// the session leaving `needs_input` is the acknowledgement.
    fn answer(
        &self,
        request_id: String,
        apply: impl FnOnce(&dyn SessionPort, &LocalSession, &str) -> anyhow::Result<AnswerOutcome>
            + Send
            + 'static,
    ) {
        if self.local.is_none() {
            // The session is opened before any frame is read, so this is
            // defensive: nothing open means nothing pending, and the
            // controller is told so rather than left waiting.
            self.writer.push(downlink::permission_refusal(
                &request_id,
                "the session is not open on this machine yet",
            ));
            return;
        }
        self.on_host(move |host, local| match apply(host, local, &request_id) {
            Ok(AnswerOutcome::Applied) => Vec::new(),
            Ok(AnswerOutcome::NotPending) => vec![downlink::permission_refusal(
                &request_id,
                "that prompt is no longer pending",
            )],
            Err(error) => vec![downlink::permission_refusal(
                &request_id,
                &format!("{error:#}"),
            )],
        });
    }

    /// Run `action` against the open session on the blocking pool, and send
    /// the frames it returns.
    fn on_host(
        &self,
        action: impl FnOnce(&dyn SessionPort, &LocalSession) -> Vec<SessionFrame> + Send + 'static,
    ) {
        let Some(local) = self.local.clone() else {
            return;
        };
        let host = Arc::clone(&self.ctx.host);
        let done = self.done_tx.clone();
        tokio::task::spawn_blocking(move || {
            let frames = action(host.as_ref(), &local);
            if !frames.is_empty() {
                let _ = done.send(Done::Frames(frames));
            }
        });
    }

    fn done(&mut self, done: Done) {
        match done {
            Done::Frames(frames) => {
                for frame in frames {
                    self.writer.push(frame);
                }
            }
            Done::PromptFailed(detail) => {
                let state = self.uplink.state_now().unwrap_or(SessionRunState::Idle);
                self.report(Reported::with_detail(state, detail));
            }
            Done::Backfill { before, lines } => {
                let outs = self.uplink.on_backfill(before, lines);
                self.send_all(outs);
            }
        }
    }

    /// A follower signal. `Some` when the item ends with it.
    fn signal(&mut self, signal: HostSignal) -> Option<Exit> {
        let outs = match signal {
            HostSignal::Attached { generation } => self.uplink.on_attached(&generation),
            HostSignal::Event(event) => self.uplink.on_event(event),
            HostSignal::Detached => self.uplink.on_detached(),
            HostSignal::Waiting { reason } => self
                .uplink
                .report(Reported::with_detail(SessionRunState::Idle, reason)),
            HostSignal::Ended { reason } => {
                self.report(Reported::with_detail(
                    SessionRunState::Stopped,
                    reason.clone(),
                ));
                return Some(Exit::Retire(reason));
            }
        };
        self.send_all(outs);
        None
    }

    /// Stop following, send what is left, close the stream.
    ///
    /// The worker is released as a client that went away, not as one that
    /// is done: it lingers for its placement's window, which is what lets a
    /// lapsed item that RC hands out again find it still up.
    async fn finish(&mut self) {
        if let Some(local) = &self.local {
            local.link.stop(false);
        }
        let outs = self.uplink.flush();
        self.send_all(outs);
        let deadline = Instant::now() + self.ctx.timing.drain_timeout;
        // An item that ends before its first connection (an open that
        // failed) still owes the controller its last state.
        if self.sender.is_none() {
            if let Some(connecting) = self.connecting.take() {
                match tokio::time::timeout_at(deadline, connecting).await {
                    Ok(Ok(Ok((sender, receiver)))) => self.connected(sender, receiver),
                    Ok(_) => {}
                    Err(_) => {
                        tracing::debug!("rebon rc: gave up connecting to send the last frames")
                    }
                }
            }
        }
        if let Some(connecting) = self.connecting.take() {
            connecting.abort();
        }
        if self.sender.is_some() {
            while self.writer.queued_bytes() > 0 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let _ = self.writer.sink.send(None);
        if let Some(sender) = self.sender.take() {
            sender.close().await;
        }
        self.writer.task.abort();
        // A delivery already on the blocking pool finishes; the ones
        // behind it are not started for a session this item has left.
        self.prompter.abort();
        // Dropping the receiver lets a follower parked on a full channel
        // see that nobody is listening.
        self.signals = None;
        if let Some(follower) = self.follower.take() {
            if tokio::time::timeout(Duration::from_secs(5), follower)
                .await
                .is_err()
            {
                tracing::warn!("rebon rc: the session follower did not stop in time");
            }
        }
    }
}

/// What the runner makes of a control request, by trying it.
fn control_verdict(
    host: &dyn SessionPort,
    local: &LocalSession,
    command: &ControlCommand,
) -> Option<rebon_bridge::control_request::ControlVerdict> {
    use rebon_bridge::control_request::{ControlEffect, ControlVerdict};
    match command {
        ControlCommand::Initialize { .. }
        | ControlCommand::SetMaxThinkingTokens { .. }
        | ControlCommand::Unsupported { .. } => None,
        ControlCommand::SetModel { model: None, .. } => {
            Some(downlink::missing_parameter("set_model", "model"))
        }
        ControlCommand::SetModel {
            model: Some(model), ..
        } => Some(match host.set_model(local, model) {
            Ok(applies) => downlink::option_verdict(applies),
            Err(error) => ControlVerdict::Rejected(format!("{error:#}")),
        }),
        ControlCommand::SetPermissionMode { mode: None, .. } => {
            Some(downlink::missing_parameter("set_permission_mode", "mode"))
        }
        ControlCommand::SetPermissionMode {
            mode: Some(mode), ..
        } => Some(match host.set_permission_mode(local, mode) {
            Ok(()) => ControlVerdict::Applied(ControlEffect::Now),
            Err(error) => ControlVerdict::Rejected(format!("{error:#}")),
        }),
        ControlCommand::Interrupt { .. } => Some(downlink::interrupt_verdict(
            host.cancel_turn(local)
                .map_err(|error| format!("{error:#}")),
        )),
    }
}

async fn join_connecting(
    connecting: &mut Option<Connecting>,
) -> Result<(Arc<dyn FrameSender>, Box<dyn FrameReceiver>), SessionStreamError> {
    match connecting.as_mut() {
        Some(task) => task.await.unwrap_or_else(|error| {
            Err(SessionStreamError::Transport(format!(
                "the connect task failed: {error}"
            )))
        }),
        None => std::future::pending().await,
    }
}

async fn recv_frame(
    receiver: &mut Option<Box<dyn FrameReceiver>>,
) -> Option<Result<SessionFrame, SessionStreamError>> {
    match receiver.as_mut() {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_signal(signals: &mut Option<mpsc::Receiver<HostSignal>>) -> Option<HostSignal> {
    match signals.as_mut() {
        Some(signals) => signals.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
