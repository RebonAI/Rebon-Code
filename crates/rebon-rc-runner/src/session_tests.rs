//! One work item end to end, against in-process doubles: the bridge's
//! in-memory API client, a scripted stream, and a scripted session host.
//! Nothing here opens a socket or starts a worker.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use rebon_bridge::api_client::{BridgeApiResult, InMemoryBridgeApiClient, RecordedMethod};
use rebon_bridge::config::{
    HeartbeatOutcome, PermissionResponseBody, SessionWork, WorkData, WorkDataType, WorkItem,
    WorkResponse,
};
use rebon_bridge::control_request::ControlEffect;
use rebon_bridge::projects::ProjectInfo;
use rebon_bridge::remote_permission::RebonPermissionOption;
use rebon_bridge::session_stream::{ControlResponseBody, QuestionAnswer};
use rebon_session_host::{
    BackgroundJobStatus, ForegroundQuestionAnswer, SessionEvent, SessionOptionAppliesFrom,
};
use serde_json::json;

use super::*;
use crate::core::ids;
use crate::core::state::tests::status;
use crate::core::uplink::tests::{projection, query, question};
use crate::core::work::plan_work;
use crate::files::RcDir;
use crate::ports::{ReplayPolicy, SessionLink};

const WAIT: Duration = Duration::from_secs(10);

// ─── The API ───────────────────────────────────────────────────────────

#[async_trait]
impl EnvironmentApi for InMemoryBridgeApiClient {
    async fn update_projects(
        &self,
        _environment_id: &str,
        _projects: &[ProjectInfo],
    ) -> BridgeApiResult<()> {
        Ok(())
    }

    fn set_environment_secret(&self, _secret: &str) {}
}

fn methods(api: &InMemoryBridgeApiClient) -> Vec<RecordedMethod> {
    api.calls().into_iter().map(|call| call.method).collect()
}

// ─── The stream ────────────────────────────────────────────────────────

enum ToWorker {
    Frame(SessionFrame),
    Close(CloseReason),
}

/// The server side of one fake connection.
struct Remote {
    sent: mpsc::UnboundedReceiver<SessionFrame>,
    inbound: mpsc::UnboundedSender<ToWorker>,
}

impl Remote {
    async fn next(&mut self) -> SessionFrame {
        tokio::time::timeout(WAIT, self.sent.recv())
            .await
            .expect("a frame in time")
            .expect("the connection is open")
    }

    /// Frames until one matches, returning it and dropping the rest.
    async fn until(&mut self, wanted: impl Fn(&SessionFrame) -> bool) -> SessionFrame {
        loop {
            let frame = self.next().await;
            if wanted(&frame) {
                return frame;
            }
        }
    }

    fn push(&self, frame: SessionFrame) {
        let _ = self.inbound.send(ToWorker::Frame(frame));
    }

    fn close(&self, reason: CloseReason) {
        let _ = self.inbound.send(ToWorker::Close(reason));
    }
}

struct FakeSender(mpsc::UnboundedSender<SessionFrame>);

#[async_trait]
impl FrameSender for FakeSender {
    async fn send(&self, frame: &SessionFrame) -> Result<(), SessionStreamError> {
        self.0
            .send(frame.clone())
            .map_err(|_| SessionStreamError::Closed(CloseReason::Network))
    }

    async fn close(&self) {}
}

struct FakeReceiver {
    inbound: mpsc::UnboundedReceiver<ToWorker>,
    reason: Option<CloseReason>,
}

#[async_trait]
impl FrameReceiver for FakeReceiver {
    async fn recv(&mut self) -> Option<Result<SessionFrame, SessionStreamError>> {
        if self.reason.is_some() {
            return None;
        }
        match self.inbound.recv().await {
            Some(ToWorker::Frame(frame)) => Some(Ok(frame)),
            Some(ToWorker::Close(reason)) => {
                self.reason = Some(reason);
                None
            }
            None => {
                self.reason = Some(CloseReason::Network);
                None
            }
        }
    }

    fn close_reason(&self) -> Option<CloseReason> {
        self.reason.clone()
    }
}

struct FakeStreams {
    refusals: Mutex<VecDeque<SessionStreamError>>,
    remotes: mpsc::UnboundedSender<Remote>,
    connects: AtomicUsize,
}

#[async_trait]
impl StreamConnector for FakeStreams {
    async fn connect(
        &self,
        _ingress_url: &str,
        token: &str,
    ) -> Result<(Arc<dyn FrameSender>, Box<dyn FrameReceiver>), SessionStreamError> {
        assert_eq!(
            token, "session-token",
            "the worker attaches with its session token"
        );
        self.connects.fetch_add(1, Ordering::Relaxed);
        if let Some(refusal) = self.refusals.lock().unwrap().pop_front() {
            return Err(refusal);
        }
        let (sent_tx, sent) = mpsc::unbounded_channel();
        let (inbound, inbound_rx) = mpsc::unbounded_channel();
        let _ = self.remotes.send(Remote { sent, inbound });
        Ok((
            Arc::new(FakeSender(sent_tx)),
            Box::new(FakeReceiver {
                inbound: inbound_rx,
                reason: None,
            }),
        ))
    }
}

// ─── The session host ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum HostCall {
    Open(OpenRequest),
    Prompt(String, usize),
    Cancel,
    SetModel(String),
    SetPermissionMode(String),
    Answer(String, RebonPermissionOption),
    Questions(String, Vec<ForegroundQuestionAnswer>),
    Backfill(u64, u64, u64),
}

#[derive(Default)]
struct FakeHost {
    open_failure: Mutex<Option<String>>,
    /// How long delivering a prompt with this text takes.
    slow_prompts: Mutex<Vec<(String, Duration)>>,
    calls: Mutex<Vec<HostCall>>,
    signals: Mutex<Option<std::sync::mpsc::Receiver<HostSignal>>>,
    answers: Mutex<VecDeque<AnswerOutcome>>,
    question_failure: Mutex<Option<String>>,
    backfill: Mutex<Vec<(u64, Value)>>,
    link: Mutex<Option<Arc<SessionLink>>>,
}

impl FakeHost {
    fn record(&self, call: HostCall) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<HostCall> {
        self.calls.lock().unwrap().clone()
    }

    async fn wait_for(&self, wanted: impl Fn(&HostCall) -> bool) -> HostCall {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(call) = self.calls().into_iter().find(|call| wanted(call)) {
                return call;
            }
            assert!(
                Instant::now() < deadline,
                "no such call: {:?}",
                self.calls()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn link(&self) -> Arc<SessionLink> {
        self.link
            .lock()
            .unwrap()
            .clone()
            .expect("a session was opened")
    }
}

impl SessionPort for FakeHost {
    fn open(&self, request: &OpenRequest) -> anyhow::Result<LocalSession> {
        self.record(HostCall::Open(request.clone()));
        if let Some(error) = self.open_failure.lock().unwrap().clone() {
            anyhow::bail!(error);
        }
        let link = Arc::new(SessionLink::new(Some("bg-1".into())));
        *self.link.lock().unwrap() = Some(Arc::clone(&link));
        Ok(LocalSession {
            rebon_session_id: request.resume.clone().unwrap_or_else(|| "local-new".into()),
            cwd: request.project.clone(),
            replay: ReplayPolicy::FromStart,
            link,
        })
    }

    fn follow(&self, session: &LocalSession, signals: mpsc::Sender<HostSignal>) {
        let source = self.signals.lock().unwrap().take();
        let Some(source) = source else {
            return;
        };
        while !session.link.stopped() {
            match source.recv_timeout(Duration::from_millis(10)) {
                Ok(signal) => {
                    let ended = matches!(signal, HostSignal::Ended { .. });
                    if signals.blocking_send(signal).is_err() || ended {
                        return;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    fn send_prompt(
        &self,
        _session: &LocalSession,
        text: String,
        images: Vec<rebon_session_host::BackgroundImageAttachment>,
    ) -> anyhow::Result<()> {
        let fail = text == "undeliverable";
        let delay = self
            .slow_prompts
            .lock()
            .unwrap()
            .iter()
            .find(|(slow, _)| *slow == text)
            .map(|(_, delay)| *delay);
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
        self.record(HostCall::Prompt(text, images.len()));
        if fail {
            anyhow::bail!("the session owner is not answering");
        }
        Ok(())
    }

    fn cancel_turn(&self, _session: &LocalSession) -> anyhow::Result<bool> {
        self.record(HostCall::Cancel);
        Ok(true)
    }

    fn set_model(
        &self,
        _session: &LocalSession,
        model: &str,
    ) -> anyhow::Result<SessionOptionAppliesFrom> {
        self.record(HostCall::SetModel(model.into()));
        Ok(SessionOptionAppliesFrom::NextTurn)
    }

    fn set_permission_mode(&self, _session: &LocalSession, mode: &str) -> anyhow::Result<()> {
        self.record(HostCall::SetPermissionMode(mode.into()));
        if mode == "bypassPermissions" {
            anyhow::bail!("bypassPermissions has not been accepted for unattended sessions");
        }
        Ok(())
    }

    fn answer_permission(
        &self,
        _session: &LocalSession,
        request_id: &str,
        option: RebonPermissionOption,
    ) -> anyhow::Result<AnswerOutcome> {
        self.record(HostCall::Answer(request_id.into(), option));
        Ok(self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(AnswerOutcome::Applied))
    }

    fn answer_question(
        &self,
        _session: &LocalSession,
        request_id: &str,
        answers: Vec<ForegroundQuestionAnswer>,
    ) -> anyhow::Result<AnswerOutcome> {
        self.record(HostCall::Questions(request_id.into(), answers));
        if let Some(error) = self.question_failure.lock().unwrap().take() {
            anyhow::bail!(error);
        }
        Ok(self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(AnswerOutcome::Applied))
    }

    fn backfill(
        &self,
        _session: &LocalSession,
        epoch: u64,
        after: u64,
        before: u64,
    ) -> anyhow::Result<Vec<(u64, Value)>> {
        self.record(HostCall::Backfill(epoch, after, before));
        Ok(self.backfill.lock().unwrap().clone())
    }
}

// ─── The harness ───────────────────────────────────────────────────────

struct Harness {
    _home: tempfile::TempDir,
    api: InMemoryBridgeApiClient,
    host: Arc<FakeHost>,
    streams: Arc<FakeStreams>,
    remotes: mpsc::UnboundedReceiver<Remote>,
    signals: std::sync::mpsc::Sender<HostSignal>,
    ctx: Arc<WorkContext>,
    interrupt: watch::Sender<Interrupt>,
}

impl Harness {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let api = InMemoryBridgeApiClient::new();
        let host = Arc::new(FakeHost::default());
        let (signals, source) = std::sync::mpsc::channel();
        *host.signals.lock().unwrap() = Some(source);
        let (remotes_tx, remotes) = mpsc::unbounded_channel();
        let streams = Arc::new(FakeStreams {
            refusals: Mutex::new(VecDeque::new()),
            remotes: remotes_tx,
            connects: AtomicUsize::new(0),
        });
        let ctx = Arc::new(WorkContext {
            api: Arc::new(api.clone()),
            streams: Arc::clone(&streams) as Arc<dyn StreamConnector>,
            host: Arc::clone(&host) as Arc<dyn SessionPort>,
            ledger: Ledger::new(RcDir::new(home.path())),
            projection: projection(),
            environment_id: "env_1".into(),
            timing: WorkTiming {
                heartbeat_interval: Duration::from_millis(30),
                lease_grace: Duration::from_secs(5),
                flush_interval: Duration::from_millis(20),
                reconnect_base: Duration::from_millis(10),
                reconnect_max: Duration::from_millis(50),
                outbox_max_bytes: 32 * 1024 * 1024,
                drain_timeout: Duration::from_secs(2),
            },
            policy: RemotePermissionPolicy::one_shot(),
            pid: 4242,
        });
        let (interrupt, _) = watch::channel(Interrupt::Running);
        Self {
            _home: home,
            api,
            host,
            streams,
            remotes,
            signals,
            ctx,
            interrupt,
        }
    }

    fn start(&self, item: WorkItem) -> JoinHandle<WorkOutcome> {
        let plan = plan_work(&item, &["/srv/app".to_string()], |rc| {
            self.ctx.ledger.remembered(rc)
        });
        tokio::spawn(run_work_item(
            Arc::clone(&self.ctx),
            plan,
            self.interrupt.subscribe(),
        ))
    }

    async fn remote(&mut self) -> Remote {
        tokio::time::timeout(WAIT, self.remotes.recv())
            .await
            .expect("a connection in time")
            .expect("the connector is alive")
    }

    fn signal(&self, signal: HostSignal) {
        self.signals.send(signal).unwrap();
    }
}

async fn ended(task: JoinHandle<WorkOutcome>) -> WorkOutcome {
    tokio::time::timeout(WAIT, task)
        .await
        .expect("the item ended in time")
        .expect("the item task")
}

fn secret(session: Option<&str>) -> String {
    WorkSecret {
        session_token: "session-token".into(),
        session_id: session.map(str::to_string),
        ingress_url: "ws://rc.invalid/v1/sessions/sess_1/stream".into(),
    }
    .encode()
}

fn item(prompt: Option<&str>, resume: Option<&str>) -> WorkItem {
    WorkItem {
        response: WorkResponse {
            id: "wrk_1".into(),
            response_type: "work".into(),
            environment_id: "env_1".into(),
            state: "leased".into(),
            data: WorkData {
                data_type: WorkDataType::Session,
                id: "sess_1".into(),
            },
            secret: secret(Some("sess_1")),
            created_at: "now".into(),
        },
        session: Some(SessionWork {
            project: "/srv/app".into(),
            prompt: prompt.map(str::to_string),
            resume_rebon_session_id: resume.map(str::to_string),
        }),
    }
}

fn in_state(word: SessionRunState) -> impl Fn(&SessionFrame) -> bool {
    move |frame| matches!(frame, SessionFrame::SessionState { state, .. } if *state == word)
}

fn is_response(frame: &SessionFrame) -> bool {
    matches!(frame, SessionFrame::ControlResponse { .. })
}

fn response(frame: SessionFrame) -> ControlResponseBody {
    match frame {
        SessionFrame::ControlResponse { response } => response,
        other => panic!("expected a control response, got {other:?}"),
    }
}

fn control(request_id: &str, subtype: &str, params: Value) -> SessionFrame {
    SessionFrame::ControlRequest {
        request_id: request_id.into(),
        subtype: subtype.into(),
        params,
    }
}

fn hello(epoch: u64) -> HostSignal {
    HostSignal::Event(SessionEvent::Hello {
        cursor: 0,
        turn_generation: 1,
        status: Box::new(status(BackgroundJobStatus::Idle, false)),
        epoch,
    })
}

fn chunk(cursor: u64, text: &str) -> HostSignal {
    HostSignal::Event(SessionEvent::SessionUpdate {
        cursor,
        update: json!({
            "sessionId": "local-new",
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
        }),
    })
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_session_is_opened_bound_prompted_and_uplinked() {
    let mut harness = Harness::new();
    let task = harness.start(item(Some("hello"), None));
    let mut remote = harness.remote().await;

    assert_eq!(
        remote.next().await,
        Reported::new(SessionRunState::Starting).frame()
    );
    assert_eq!(remote.next().await, SessionFrame::bound("local-new"));
    harness
        .host
        .wait_for(|call| *call == HostCall::Prompt("hello".into(), 0))
        .await;
    assert_eq!(
        harness.host.calls()[0],
        HostCall::Open(OpenRequest {
            project: "/srv/app".into(),
            resume: None
        })
    );

    harness.signal(HostSignal::Attached {
        generation: "g1".into(),
    });
    harness.signal(hello(5));
    harness.signal(chunk(1, "Hel"));
    harness.signal(chunk(2, "lo"));
    assert_eq!(
        remote.next().await,
        Reported::new(SessionRunState::Idle).frame()
    );
    let message = remote.next().await;
    assert_eq!(
        message,
        SessionFrame::message_with_id(
            "e5.1-2",
            json!({
                "sessionId": "local-new",
                "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hello"}}
            })
        )
    );

    // The ledger knows the session and that the prompt ran.
    let entry = harness.ctx.ledger.session("sess_1").unwrap().unwrap();
    assert_eq!(entry.rebon_session_id, "local-new");
    assert_eq!(entry.job_id.as_deref(), Some("bg-1"));
    assert!(!harness.ctx.ledger.claim_prompt("wrk_1", 0).unwrap());

    // The lease goes: the item stands down and leaves the work item be.
    harness.api.set_heartbeat_outcome(HeartbeatOutcome {
        lease_extended: false,
        state: "ready".into(),
    });
    let outcome = ended(task).await;
    assert!(matches!(outcome.exit, Exit::StandDown(_)), "{outcome:?}");
    assert_eq!(outcome.rc_session_id.as_deref(), Some("sess_1"));
    let calls = methods(&harness.api);
    assert_eq!(calls[0], RecordedMethod::Acknowledge);
    assert!(calls.contains(&RecordedMethod::Heartbeat));
    assert!(!calls.contains(&RecordedMethod::Stop));
    let link = harness.host.link();
    assert!(link.stopped());
    assert!(!link.deliberate(), "the worker is left to linger");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_is_answered_remotely_and_refusals_say_why() {
    let mut harness = Harness::new();
    let task = harness.start(item(Some("go"), None));
    let mut remote = harness.remote().await;
    remote
        .until(|frame| matches!(frame, SessionFrame::SessionBound { .. }))
        .await;
    harness.signal(HostSignal::Attached {
        generation: "g1".into(),
    });
    harness.signal(hello(5));
    harness.signal(HostSignal::Event(SessionEvent::Permission {
        cursor: 1,
        query: Box::new(question(6, 1)),
    }));
    let request = remote
        .until(|frame| matches!(frame, SessionFrame::PermissionRequest { .. }))
        .await;
    let SessionFrame::PermissionRequest {
        request_id,
        request,
    } = request
    else {
        unreachable!()
    };
    assert_eq!(request["_meta"]["rebonRc"]["kind"], "question");
    assert_eq!(request["_meta"]["rebonRc"]["answerable"], true);
    assert_eq!(
        request["_meta"]["rebonRc"]["answerWith"],
        "question_response"
    );
    assert_eq!(request["questions"][1]["question"], "When?");
    assert_eq!(
        remote.next().await,
        Reported::new(SessionRunState::NeedsInput).frame()
    );

    // The answers reach the host as the session host's own type.
    remote.push(SessionFrame::QuestionResponse {
        request_id: request_id.clone(),
        answers: vec![
            QuestionAnswer::options([1]),
            QuestionAnswer {
                selected_options: vec![0, 2],
                other_text: Some("mornings".into()),
            },
        ],
    });
    let answered = harness
        .host
        .wait_for(|call| matches!(call, HostCall::Questions(..)))
        .await;
    assert_eq!(
        answered,
        HostCall::Questions(
            request_id.clone(),
            vec![
                ForegroundQuestionAnswer {
                    selected_options: vec![1],
                    other_text: None,
                },
                ForegroundQuestionAnswer {
                    selected_options: vec![0, 2],
                    other_text: Some("mornings".into()),
                },
            ]
        )
    );

    // No answers at all: refused without asking the host.
    remote.push(SessionFrame::QuestionResponse {
        request_id: request_id.clone(),
        answers: Vec::new(),
    });
    let empty = response(remote.until(is_response).await);
    assert_eq!(empty.subtype, "error");
    assert_eq!(empty.request_id, request_id);
    assert!(empty.error.unwrap().contains("one answer per question"));

    // The host's refusal reaches the controller as it was given.
    *harness.host.question_failure.lock().unwrap() =
        Some("the answers do not fit the questions: out of range".into());
    remote.push(SessionFrame::QuestionResponse {
        request_id: request_id.clone(),
        answers: vec![QuestionAnswer::options([7]), QuestionAnswer::text("x")],
    });
    let misfit = response(remote.until(is_response).await);
    assert!(misfit.error.unwrap().contains("do not fit"));

    // Answered already, or replaced: says so.
    harness
        .host
        .answers
        .lock()
        .unwrap()
        .push_back(AnswerOutcome::NotPending);
    remote.push(SessionFrame::QuestionResponse {
        request_id: request_id.clone(),
        answers: vec![QuestionAnswer::options([0]), QuestionAnswer::options([0])],
    });
    let stale = response(remote.until(is_response).await);
    assert_eq!(stale.request_id, request_id);
    assert!(stale.error.unwrap().contains("no longer pending"));

    // A deny goes to the host as a decline; the host decides what that is.
    remote.push(SessionFrame::PermissionResponse {
        response: PermissionResponseBody::success(&request_id, json!({"behavior": "deny"})),
    });
    harness
        .host
        .wait_for(|call| {
            *call == HostCall::Answer(request_id.clone(), RebonPermissionOption::RejectOnce)
        })
        .await;
    let questions = harness
        .host
        .calls()
        .into_iter()
        .filter(|call| matches!(call, HostCall::Questions(..)))
        .count();
    assert_eq!(questions, 3, "the empty response never reached the host");

    harness.interrupt.send(Interrupt::Shutdown).unwrap();
    let outcome = ended(task).await;
    assert!(matches!(outcome.exit, Exit::StandDown(_)), "{outcome:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_and_control_requests_are_answered_from_what_happened() {
    let mut harness = Harness::new();
    let task = harness.start(item(Some("go"), None));
    let mut remote = harness.remote().await;
    remote
        .until(|frame| matches!(frame, SessionFrame::SessionBound { .. }))
        .await;
    harness.signal(HostSignal::Attached {
        generation: "g1".into(),
    });
    harness.signal(hello(5));
    harness.signal(HostSignal::Event(SessionEvent::Permission {
        cursor: 1,
        query: Box::new(query(3, 1)),
    }));
    let request = remote
        .until(|frame| matches!(frame, SessionFrame::PermissionRequest { .. }))
        .await;
    let SessionFrame::PermissionRequest { request_id, .. } = request else {
        unreachable!()
    };
    assert_eq!(request_id, ids::permission_request_id(&query(3, 1)));
    assert_eq!(
        remote.next().await,
        Reported::new(SessionRunState::NeedsInput).frame()
    );

    // An allow is answered once, with the one-shot option.
    remote.push(SessionFrame::PermissionResponse {
        response: PermissionResponseBody::success(&request_id, json!({"behavior": "allow"})),
    });
    harness
        .host
        .wait_for(|call| {
            *call == HostCall::Answer(request_id.clone(), RebonPermissionOption::AllowOnce)
        })
        .await;

    // A request for a standing rule is refused and never reaches the host.
    remote.push(SessionFrame::PermissionResponse {
        response: PermissionResponseBody::success(
            &request_id,
            json!({"behavior": "allow", "updated_permissions": [{"rule": "Bash(*)"}]}),
        ),
    });
    let refused = response(remote.until(is_response).await);
    assert_eq!(refused.subtype, "error");
    assert_eq!(refused.request_id, request_id);
    assert!(refused.error.unwrap().contains("this call only"));

    // An answer the owner no longer wants says so.
    harness
        .host
        .answers
        .lock()
        .unwrap()
        .push_back(AnswerOutcome::NotPending);
    remote.push(SessionFrame::PermissionResponse {
        response: PermissionResponseBody::success(&request_id, json!({"behavior": "deny"})),
    });
    let stale = response(remote.until(is_response).await);
    assert!(stale.error.unwrap().contains("no longer pending"));
    let answers = harness
        .host
        .calls()
        .into_iter()
        .filter(|call| matches!(call, HostCall::Answer(..)))
        .count();
    assert_eq!(answers, 2);

    // set_model: tried, and answered with when it applies.
    remote.push(control("r1", "set_model", json!({"model": "opus"})));
    let applied = response(remote.until(is_response).await);
    assert_eq!(applied.request_id, "r1");
    assert_eq!(applied.applied(), Some(ControlEffect::NextTurn));
    assert!(harness
        .host
        .calls()
        .contains(&HostCall::SetModel("opus".into())));

    // A missing model is an error, and nothing is tried.
    remote.push(control("r1b", "set_model", json!({})));
    let missing = response(remote.until(is_response).await);
    assert!(missing.error.unwrap().contains("params.model"));

    // No thinking-token setting: an error from the planner.
    remote.push(control(
        "r2",
        "set_max_thinking_tokens",
        json!({"max_thinking_tokens": 9}),
    ));
    let thinking = response(remote.until(is_response).await);
    assert_eq!(thinking.request_id, "r2");
    assert!(thinking.error.unwrap().contains("not supported"));

    // The unattended-launch gate refuses a bypass.
    remote.push(control(
        "r3",
        "set_permission_mode",
        json!({"mode": "bypassPermissions"}),
    ));
    let bypass = response(remote.until(is_response).await);
    assert_eq!(bypass.subtype, "error");
    assert!(bypass.error.unwrap().contains("not been accepted"));
    remote.push(control(
        "r4",
        "set_permission_mode",
        json!({"mode": "plan"}),
    ));
    let plan = response(remote.until(is_response).await);
    assert_eq!(plan.applied(), Some(ControlEffect::Now));

    // An interrupt cancels the turn, and so does a bare cancel.
    remote.push(control("r5", "interrupt", Value::Null));
    let interrupt = response(remote.until(is_response).await);
    assert_eq!(interrupt.applied(), Some(ControlEffect::Now));
    remote.push(SessionFrame::Cancel);
    let deadline = Instant::now() + WAIT;
    while harness
        .host
        .calls()
        .iter()
        .filter(|call| **call == HostCall::Cancel)
        .count()
        < 2
    {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // initialize is answered with the planner's body.
    remote.push(control("r6", "initialize", Value::Null));
    let init = response(remote.until(is_response).await);
    assert_eq!(init.response.unwrap()["pid"], 4242);

    // A prompt with an image goes to the host with it.
    remote.push(SessionFrame::Prompt {
        text: "look".into(),
        attachments: vec![json!({"type": "image", "mimeType": "image/png", "data": "AA"})],
    });
    harness
        .host
        .wait_for(|call| *call == HostCall::Prompt("look".into(), 1))
        .await;

    harness.interrupt.send(Interrupt::Shutdown).unwrap();
    let outcome = ended(task).await;
    assert!(matches!(outcome.exit, Exit::StandDown(_)));
    assert!(!methods(&harness.api).contains(&RecordedMethod::Stop));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_item_is_stopped_without_being_acknowledged() {
    let harness = Harness::new();
    let mut elsewhere = item(Some("x"), None);
    elsewhere.session.as_mut().unwrap().project = "/etc".into();
    let outcome = ended(harness.start(elsewhere)).await;
    assert!(outcome.exit.retires());
    assert!(outcome.exit.reason().contains("does not serve"));
    assert_eq!(methods(&harness.api), vec![RecordedMethod::Stop]);
    assert!(harness.host.calls().is_empty());
    assert_eq!(harness.streams.connects.load(Ordering::Relaxed), 0);

    let mut no_session = item(Some("x"), None);
    no_session.session = None;
    let outcome = ended(harness.start(no_session)).await;
    assert!(outcome.exit.retires());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_healthcheck_is_acknowledged_and_finished() {
    let harness = Harness::new();
    let mut probe = item(None, None);
    probe.session = None;
    probe.response.data = WorkData {
        data_type: WorkDataType::Healthcheck,
        id: "wrk_1".into(),
    };
    probe.response.secret = secret(None);
    let outcome = ended(harness.start(probe)).await;
    assert!(outcome.exit.retires());
    assert_eq!(outcome.rc_session_id, None);
    assert_eq!(
        methods(&harness.api),
        vec![RecordedMethod::Acknowledge, RecordedMethod::Stop]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_cannot_be_opened_is_reported_failed_and_stopped() {
    let mut harness = Harness::new();
    *harness.host.open_failure.lock().unwrap() =
        Some("the session was stopped on this machine".into());
    let task = harness.start(item(None, Some("local-old")));
    let mut remote = harness.remote().await;
    assert_eq!(
        remote.next().await,
        Reported::new(SessionRunState::Starting).frame()
    );
    let failed = remote.next().await;
    let SessionFrame::SessionState { state, detail } = failed else {
        panic!("expected a state, got {failed:?}");
    };
    assert_eq!(state, SessionRunState::Failed);
    assert!(detail.unwrap().contains("stopped on this machine"));
    let outcome = ended(task).await;
    assert!(outcome.exit.retires());
    assert_eq!(
        methods(&harness.api),
        vec![RecordedMethod::Acknowledge, RecordedMethod::Stop]
    );
    assert_eq!(
        harness.host.calls(),
        vec![HostCall::Open(OpenRequest {
            project: "/srv/app".into(),
            resume: Some("local-old".into())
        })]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_connection_is_reconnected_and_a_superseded_one_is_not() {
    let mut harness = Harness::new();
    let task = harness.start(item(Some("hi"), None));
    let mut first = harness.remote().await;
    first
        .until(|frame| matches!(frame, SessionFrame::SessionBound { .. }))
        .await;
    harness.signal(HostSignal::Attached {
        generation: "g1".into(),
    });
    harness.signal(hello(7));
    first.until(in_state(SessionRunState::Idle)).await;
    harness.signal(HostSignal::Event(SessionEvent::SessionUpdate {
        cursor: 1,
        update: json!({"update": {"sessionUpdate": "plan"}}),
    }));
    first
        .until(|frame| matches!(frame, SessionFrame::SessionMessage { .. }))
        .await;

    // The network drops: a new connection gets the keyed frames again, the
    // binding, and the current state.
    first.close(CloseReason::Network);
    drop(first);
    let mut second = harness.remote().await;
    let mut resent = Vec::new();
    while resent.len() < 4 {
        resent.push(second.next().await);
    }
    assert!(resent.contains(&SessionFrame::bound("local-new")));
    assert!(resent.contains(&SessionFrame::message_with_id(
        "e7.1-1",
        json!({"update": {"sessionUpdate": "plan"}})
    )));
    assert!(resent.contains(&Reported::new(SessionRunState::Idle).frame()));

    // Another worker took the session: stand down and retire the item.
    second.close(CloseReason::Superseded);
    let outcome = ended(task).await;
    assert!(outcome.exit.retires(), "{outcome:?}");
    assert!(methods(&harness.api).contains(&RecordedMethod::Stop));
    assert_eq!(harness.streams.connects.load(Ordering::Relaxed), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_gone_at_connect_stands_down() {
    let harness = Harness::new();
    harness
        .streams
        .refusals
        .lock()
        .unwrap()
        .push_back(SessionStreamError::Rejected {
            status: 409,
            error: rebon_bridge::api_client::BridgeApiError::Permanent("409".into()),
        });
    let outcome = ended(harness.start(item(Some("hi"), None))).await;
    assert!(matches!(outcome.exit, Exit::StandDown(_)), "{outcome:?}");
    assert!(!methods(&harness.api).contains(&RecordedMethod::Stop));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transient_connect_failure_is_retried() {
    let mut harness = Harness::new();
    harness
        .streams
        .refusals
        .lock()
        .unwrap()
        .push_back(SessionStreamError::Transport("reset".into()));
    let task = harness.start(item(Some("hi"), None));
    let mut remote = harness.remote().await;
    remote
        .until(|frame| matches!(frame, SessionFrame::SessionBound { .. }))
        .await;
    assert_eq!(harness.streams.connects.load(Ordering::Relaxed), 2);
    harness.interrupt.send(Interrupt::Shutdown).unwrap();
    ended(task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_that_already_ran_is_not_run_again() {
    let mut harness = Harness::new();
    // A previous run of this item claimed its prompt; RC handed it out
    // again after the lease lapsed.
    assert!(harness.ctx.ledger.claim_prompt("wrk_1", 1).unwrap());
    harness
        .ctx
        .ledger
        .record_session(
            "sess_1",
            crate::ledger::SessionEntry {
                rebon_session_id: "local-known".into(),
                project: "/srv/app".into(),
                cwd: "/srv/app".into(),
                job_id: None,
                environment_id: "env_1".into(),
                updated_at_ms: 1,
            },
        )
        .unwrap();
    let task = harness.start(item(Some("hello"), None));
    let mut remote = harness.remote().await;
    // It resumes the session the ledger remembers.
    assert_eq!(
        remote
            .until(|frame| matches!(frame, SessionFrame::SessionBound { .. }))
            .await,
        SessionFrame::bound("local-known")
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(harness
        .host
        .calls()
        .iter()
        .all(|call| !matches!(call, HostCall::Prompt(..))));
    harness.interrupt.send(Interrupt::Shutdown).unwrap();
    ended(task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_stopped_on_the_machine_ends_the_item() {
    let mut harness = Harness::new();
    let task = harness.start(item(Some("hi"), None));
    let mut remote = harness.remote().await;
    remote
        .until(|frame| matches!(frame, SessionFrame::SessionBound { .. }))
        .await;
    harness.signal(HostSignal::Ended {
        reason: "the session was stopped on this machine".into(),
    });
    let stopped = remote.until(in_state(SessionRunState::Stopped)).await;
    assert!(matches!(
        stopped,
        SessionFrame::SessionState {
            detail: Some(_),
            ..
        }
    ));
    let outcome = ended(task).await;
    assert!(outcome.exit.retires());
    assert!(methods(&harness.api).contains(&RecordedMethod::Stop));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_item_on_this_machine_retires_the_old_one() {
    let mut harness = Harness::new();
    let task = harness.start(item(Some("hi"), None));
    harness
        .remote()
        .await
        .until(|frame| matches!(frame, SessionFrame::SessionBound { .. }))
        .await;
    harness.interrupt.send(Interrupt::Superseded).unwrap();
    let outcome = ended(task).await;
    assert!(outcome.exit.retires());
    assert!(methods(&harness.api).contains(&RecordedMethod::Stop));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gap_is_backfilled_from_the_event_log() {
    let mut harness = Harness::new();
    *harness.host.backfill.lock().unwrap() = vec![(
        2,
        json!({"update": {"sessionUpdate": "plan", "from": "log"}}),
    )];
    let task = harness.start(item(Some("hi"), None));
    let mut remote = harness.remote().await;
    remote
        .until(|frame| matches!(frame, SessionFrame::SessionBound { .. }))
        .await;
    harness.signal(HostSignal::Attached {
        generation: "g1".into(),
    });
    harness.signal(hello(9));
    harness.signal(HostSignal::Event(SessionEvent::SessionUpdate {
        cursor: 1,
        update: json!({"update": {"sessionUpdate": "plan", "n": 1}}),
    }));
    harness.signal(HostSignal::Event(SessionEvent::Gap { from: 1, to: 3 }));
    harness.signal(HostSignal::Event(SessionEvent::SessionUpdate {
        cursor: 3,
        update: json!({"update": {"sessionUpdate": "plan", "n": 3}}),
    }));
    let mut ids = Vec::new();
    while ids.len() < 3 {
        if let SessionFrame::SessionMessage {
            message_id: Some(id),
            ..
        } = remote.next().await
        {
            ids.push(id);
        }
    }
    assert_eq!(ids, vec!["e9.1-1", "e9.2-2", "e9.3-3"]);
    assert!(harness.host.calls().contains(&HostCall::Backfill(9, 1, 3)));
    harness.interrupt.send(Interrupt::Shutdown).unwrap();
    ended(task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompts_reach_the_session_in_the_order_they_arrived() {
    let mut harness = Harness::new();
    // The first delivery is slow; the ones behind it must wait for it
    // rather than overtake it on another blocking thread.
    harness
        .host
        .slow_prompts
        .lock()
        .unwrap()
        .push(("one".into(), Duration::from_millis(300)));
    let task = harness.start(item(None, Some("local-old")));
    let mut remote = harness.remote().await;
    remote
        .until(|frame| matches!(frame, SessionFrame::SessionBound { .. }))
        .await;
    for text in ["one", "two", "undeliverable", "four"] {
        remote.push(SessionFrame::prompt(text));
    }
    harness
        .host
        .wait_for(|call| *call == HostCall::Prompt("four".into(), 0))
        .await;
    let prompts: Vec<HostCall> = harness
        .host
        .calls()
        .into_iter()
        .filter(|call| matches!(call, HostCall::Prompt(..)))
        .collect();
    assert_eq!(
        prompts,
        ["one", "two", "undeliverable", "four"]
            .map(|text| HostCall::Prompt(text.into(), 0))
            .to_vec()
    );
    // A failed delivery is reported and does not stop the ones after it.
    remote
        .until(|frame| {
            matches!(frame, SessionFrame::SessionState { detail: Some(detail), .. }
                if detail.contains("not delivered"))
        })
        .await;
    harness.interrupt.send(Interrupt::Shutdown).unwrap();
    ended(task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undeliverable_prompt_is_reported() {
    let mut harness = Harness::new();
    let task = harness.start(item(Some("undeliverable"), None));
    let mut remote = harness.remote().await;
    let reported = remote
        .until(|frame| {
            matches!(frame, SessionFrame::SessionState { detail: Some(detail), .. }
                if detail.contains("not delivered"))
        })
        .await;
    assert!(matches!(
        reported,
        SessionFrame::SessionState {
            state: SessionRunState::Starting,
            ..
        }
    ));
    // Waiting on another holder is shown the same way.
    harness.signal(HostSignal::Waiting {
        reason: "held by a terminal".into(),
    });
    let waiting = remote
        .until(|frame| {
            matches!(frame, SessionFrame::SessionState { detail: Some(detail), .. }
                if detail.contains("terminal"))
        })
        .await;
    assert!(matches!(
        waiting,
        SessionFrame::SessionState {
            state: SessionRunState::Idle,
            ..
        }
    ));
    harness.interrupt.send(Interrupt::Shutdown).unwrap();
    ended(task).await;
}
