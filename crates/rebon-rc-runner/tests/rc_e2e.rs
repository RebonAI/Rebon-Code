//! `rebon rc` end to end: the real RC server, the real runner, the real
//! bridge transports — RFC-0008 §14.7's manual smoke, automated.
//!
//! ```text
//! controller ──HTTP (bridge client)──▶ rc-server (in process, sqlite file) ◀──HTTP── serve loop
//!     │                                    ▲        ▲                                 │
//!     └──────────── WS (direct) ───────────┘        └──── WS via a cuttable proxy ────┘
//!                                                                                     ▼
//!                                                               scripted session host (no model)
//! ```
//!
//! Real: `rebon_rc_server::app` on a loopback socket with an on-disk
//! database and its cleanup sweeper; `login::login`; `serve::serve` with
//! `HttpEnvironmentApi` and `WsConnector`, the ledger and the machine lock
//! under a temporary config home; the controller's `HttpBridgeApiClient`
//! and `SessionStream`.
//!
//! Scripted: the session host. A real one starts a `rebon` worker that
//! talks to a model provider; here a [`ScriptedHost`] plays the owner — a
//! deterministic "model" that answers each prompt from its text, asks for
//! a permission, streams a long reply, and turns a cancel into an idle
//! turn — and feeds its events to the runner through the same
//! [`SessionPort`] the real host implements. No worker process, no
//! provider, no tokens spent.
//!
//! The runner's session stream goes through a TCP proxy (the server's
//! `session_ingress_url` points at it) so a test can cut the connection
//! the way a network would and watch the runner reconnect.
//!
//! Every wait is bounded ([`STEP`]) and so is every test ([`WHOLE`]).

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rebon_bridge::api_client::BridgeApiError;
use rebon_bridge::config::EnqueueWorkRequest;
use rebon_bridge::config::PermissionResponseBody;
use rebon_bridge::devices::{IssueDeviceRequest, IssuedDevice};
use rebon_bridge::history::PageRequest;
use rebon_bridge::http_client::{DeviceCredentials, HttpBridgeApiClient, HttpClientConfig};
use rebon_bridge::remote_permission::{RebonPermissionOption, RemotePermissionPolicy};
use rebon_bridge::session_stream::{DeliveredFrame, FrameOrigin, SessionFrame, SessionRunState};
use rebon_bridge::stream_client::{
    SessionStream, SessionStreamOptions, SessionStreamRx, SessionStreamTx,
};
use rebon_rc_runner::core::uplink::PermissionProjection;
use rebon_rc_runner::files::RcDir;
use rebon_rc_runner::ledger::Ledger;
use rebon_rc_runner::login::{self, LoginCredential};
use rebon_rc_runner::ports::{
    AnswerOutcome, HostSignal, LocalSession, OpenRequest, ReplayPolicy, SessionLink, SessionPort,
};
use rebon_rc_runner::serve::{serve, ServeConfig, ServeTiming};
use rebon_rc_runner::session::{WorkContext, WorkTiming};
use rebon_rc_runner::transport::{HttpEnvironmentApi, WsConnector};
use rebon_rc_server::{app, Config, RcState};
use rebon_session_host::{
    BackgroundImageAttachment, BackgroundPermissionQuerySnapshot, ForegroundQuestionAnswer,
    SessionEvent, SessionOptionAppliesFrom, SessionStatusSnapshot, TurnStreamState,
};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

/// The longest any single expected thing may take to happen.
const STEP: Duration = Duration::from_secs(20);
/// The longest a whole test may take.
const WHOLE: Duration = Duration::from_secs(120);
/// How long an RC work lease lives without a heartbeat. Short, so a
/// runner restart gets its item back within seconds.
const LEASE_TTL: Duration = Duration::from_secs(2);

/// The one local session the scripted host hands out.
const LOCAL_SESSION: &str = "rc-e2e-local-1";
const JOB_ID: &str = "bg-e2e-1";
/// The owner's stream epoch: every `message_id` starts `e7.`.
const EPOCH: u64 = 7;

// ─── The server ────────────────────────────────────────────────────────

struct RcServer {
    base: String,
    ws_base: String,
    bootstrap_token: String,
    proxy: Proxy,
    task: JoinHandle<()>,
    _state: Arc<RcState>,
    _database: tempfile::TempDir,
}

impl Drop for RcServer {
    fn drop(&mut self) {
        self.task.abort();
        self.proxy.task.abort();
    }
}

/// Start the server on a random loopback port, with an on-disk database
/// and the cleanup sweeper that returns lapsed leases to the queue. Its
/// session ingress — what a work secret tells a worker to dial — is a
/// proxy in front of it.
async fn start_server() -> RcServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind rc");
    let address = listener.local_addr().expect("rc address");
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let proxy_address = proxy_listener.local_addr().expect("proxy address");
    let bootstrap_token = rebon_rc_server::ids::generate_token();
    let config = Config {
        bind: address,
        public_url: format!("http://{address}"),
        session_ingress_url: format!("ws://{proxy_address}"),
        lease_ttl: LEASE_TTL,
        access_token_ttl: Duration::from_secs(3_600),
        default_poll_wait: Duration::from_secs(1),
        max_poll_wait: Duration::from_secs(60),
        max_body_bytes: 262_144,
        session_replay_events: 2_000,
        page_limit_default: 100,
        page_limit_max: 500,
        auth_rate_burst: 1_000,
        auth_rate_refill_per_minute: 1_000,
        bootstrap_token: Some(bootstrap_token.clone()),
        trust_forwarded_for: false,
    };
    config.validate().expect("test configuration is valid");
    let database = tempfile::tempdir().expect("database dir");
    let state = Arc::new(
        RcState::open(config, database.path().join("rc.sqlite3"), [7u8; 32])
            .expect("open database"),
    );
    Arc::clone(&state).start_cleanup();
    let serve_state = Arc::clone(&state);
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app(serve_state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("rc server");
    });
    RcServer {
        base: format!("http://{address}"),
        ws_base: format!("ws://{address}"),
        bootstrap_token,
        proxy: Proxy::start(proxy_listener, address),
        task,
        _state: state,
        _database: database,
    }
}

// ─── A cuttable TCP proxy ──────────────────────────────────────────────

struct Proxy {
    /// Bumped to drop every connection open at that moment.
    cut: Arc<watch::Sender<u64>>,
    accepted: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Proxy {
    fn start(listener: TcpListener, upstream: SocketAddr) -> Self {
        let cut = Arc::new(watch::channel(0u64).0);
        let accepted = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn({
            let cut = Arc::clone(&cut);
            let accepted = Arc::clone(&accepted);
            async move {
                loop {
                    let Ok((mut inbound, _)) = listener.accept().await else {
                        return;
                    };
                    accepted.fetch_add(1, Ordering::SeqCst);
                    let mut cut = cut.subscribe();
                    cut.borrow_and_update();
                    tokio::spawn(async move {
                        let Ok(mut outbound) = TcpStream::connect(upstream).await else {
                            return;
                        };
                        tokio::select! {
                            _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                            _ = cut.changed() => {}
                        }
                        // Both sockets drop here: the runner and the server
                        // each see their end go without a close frame.
                    });
                }
            }
        });
        Self {
            cut,
            accepted,
            task,
        }
    }

    fn cut_all(&self) {
        self.cut.send_modify(|generation| *generation += 1);
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

// ─── The scripted session host ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum HostCall {
    Open(OpenRequest),
    Prompt(String),
    Cancel,
    SetModel(String),
    Answer(String, RebonPermissionOption),
}

#[derive(Default)]
struct Owner {
    calls: Vec<HostCall>,
    /// Everything the owner ever emitted, in cursor order (cursor = index + 1).
    log: Vec<SessionEvent>,
    turn: u64,
    busy: bool,
    pending: Option<BackgroundPermissionQuerySnapshot>,
    next_query: u64,
    cwd: String,
}

impl Owner {
    fn cursor(&self) -> u64 {
        self.log.len() as u64
    }

    fn update(&mut self, text: &str) {
        let cursor = self.cursor() + 1;
        self.log.push(SessionEvent::SessionUpdate {
            cursor,
            update: json!({
                "sessionId": LOCAL_SESSION,
                "turnGeneration": self.turn,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text}
                }
            }),
        });
    }

    fn start_turn(&mut self) {
        self.turn += 1;
        self.busy = true;
        let cursor = self.cursor() + 1;
        self.log.push(SessionEvent::Turn {
            cursor,
            state: TurnStreamState::Running,
            stop_reason: None,
            stop_refused: None,
        });
    }

    fn end_turn(&mut self, stop_reason: &str) {
        self.busy = false;
        self.pending = None;
        let cursor = self.cursor() + 1;
        self.log.push(SessionEvent::Turn {
            cursor,
            state: TurnStreamState::Idle,
            stop_reason: Some(stop_reason.to_string()),
            stop_refused: None,
        });
    }

    fn status(&self) -> Box<SessionStatusSnapshot> {
        let status = if self.pending.is_some() {
            "needs_input"
        } else if self.busy {
            "running"
        } else {
            "idle"
        };
        Box::new(
            serde_json::from_value(json!({
                "jobId": JOB_ID,
                "sessionId": LOCAL_SESSION,
                "cwd": self.cwd,
                "status": status,
                "busy": self.busy,
                "turnGeneration": self.turn,
                "pendingPermission": self.pending,
                "updatedAtMs": 0,
            }))
            .expect("a status snapshot"),
        )
    }

    /// The "model": what a prompt does is decided by its text.
    fn prompt(&mut self, text: &str) {
        self.start_turn();
        if text.starts_with("[permission]") {
            self.update("I need to run a command first.");
            self.next_query += 1;
            let query: BackgroundPermissionQuerySnapshot = serde_json::from_value(json!({
                "queryId": self.next_query,
                "turnGeneration": self.turn,
                "endpoint": {"pid": 7, "port": 4000, "token": "owner-ipc-token"},
                "tool": "Bash",
                "toolCallId": format!("call-{}", self.next_query),
                "sessionId": LOCAL_SESSION,
                "title": "Run ls",
                "toolInput": {"command": "ls"},
                "options": [
                    {"optionId": "allow", "label": "Allow", "kind": "allow_once"},
                    {"optionId": "always", "label": "Always", "kind": "allow_always"},
                    {"optionId": "deny", "label": "Deny", "kind": "reject_once"},
                    {"optionId": "never", "label": "Never", "kind": "reject_always"}
                ]
            }))
            .expect("a permission query");
            self.pending = Some(query.clone());
            let cursor = self.cursor() + 1;
            self.log.push(SessionEvent::Permission {
                cursor,
                query: Box::new(query),
            });
        } else if text.starts_with("[long]") {
            self.update(&long_reply());
            self.end_turn("end_turn");
        } else if text.starts_with("[slow]") {
            // Runs until somebody cancels it.
            self.update("working on it");
        } else {
            // Two deltas the runner merges into one message.
            self.update("echo: ");
            self.update(text);
            self.end_turn("end_turn");
        }
    }

    fn answer(&mut self, request_id: &str, option: RebonPermissionOption) -> AnswerOutcome {
        let Some(query) = self.pending.as_ref() else {
            return AnswerOutcome::NotPending;
        };
        if rebon_rc_runner::core::ids::permission_request_id(query) != request_id {
            return AnswerOutcome::NotPending;
        }
        self.pending = None;
        let snapshot = self.status();
        let cursor = self.cursor() + 1;
        self.log.push(SessionEvent::Status { cursor, snapshot });
        let verdict = match option {
            RebonPermissionOption::AllowOnce => "allowed",
            _ => "denied",
        };
        self.update(&format!("the command was {verdict}"));
        self.end_turn("end_turn");
        AnswerOutcome::Applied
    }
}

/// The machine's session host, as far as the runner can tell: one owner
/// that outlives any runner, answering through [`SessionPort`].
#[derive(Default)]
struct ScriptedHost {
    owner: Mutex<Owner>,
    changed: Condvar,
    followers: AtomicUsize,
}

impl ScriptedHost {
    fn with<R>(&self, action: impl FnOnce(&mut Owner) -> R) -> R {
        let mut owner = self.owner.lock().expect("owner lock");
        let result = action(&mut owner);
        drop(owner);
        self.changed.notify_all();
        result
    }

    fn calls(&self) -> Vec<HostCall> {
        self.owner.lock().expect("owner lock").calls.clone()
    }

    fn count(&self, wanted: &HostCall) -> usize {
        self.calls().iter().filter(|call| *call == wanted).count()
    }

    async fn wait_for(&self, wanted: HostCall) {
        let deadline = tokio::time::Instant::now() + STEP;
        while self.count(&wanted) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the host never saw {wanted:?}; it saw {:?}",
                self.calls()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Something the owner does on its own (a background turn's output).
    fn say(&self, text: &str) {
        self.with(|owner| owner.update(text));
    }
}

impl SessionPort for ScriptedHost {
    fn open(&self, request: &OpenRequest) -> anyhow::Result<LocalSession> {
        let replay = self.with(|owner| {
            owner.calls.push(HostCall::Open(request.clone()));
            if let Some(resume) = &request.resume {
                anyhow::ensure!(resume == LOCAL_SESSION, "no session {resume} here");
                // The worker is still up: only what happens from now on.
                Ok(ReplayPolicy::LiveOnly)
            } else {
                anyhow::ensure!(owner.log.is_empty(), "the test opens one new session");
                owner.cwd = request.project.clone();
                Ok(ReplayPolicy::FromStart)
            }
        })?;
        Ok(LocalSession {
            rebon_session_id: LOCAL_SESSION.to_string(),
            cwd: request.project.clone(),
            replay,
            link: Arc::new(SessionLink::new(Some(JOB_ID.to_string()))),
        })
    }

    fn follow(&self, session: &LocalSession, signals: tokio::sync::mpsc::Sender<HostSignal>) {
        let generation = self.followers.fetch_add(1, Ordering::SeqCst) + 1;
        if signals
            .blocking_send(HostSignal::Attached {
                generation: format!("gen-{generation}"),
            })
            .is_err()
        {
            return;
        }
        let (hello, mut sent) = {
            let owner = self.owner.lock().expect("owner lock");
            let from = match session.replay {
                ReplayPolicy::FromStart => 0,
                ReplayPolicy::LiveOnly => owner.cursor(),
            };
            let hello = SessionEvent::Hello {
                cursor: from,
                turn_generation: owner.turn,
                status: owner.status(),
                epoch: EPOCH,
            };
            (hello, from as usize)
        };
        if signals.blocking_send(HostSignal::Event(hello)).is_err() {
            return;
        }
        loop {
            let fresh: Vec<SessionEvent> = {
                let owner = self.owner.lock().expect("owner lock");
                let (owner, _) = self
                    .changed
                    .wait_timeout_while(owner, Duration::from_millis(50), |owner| {
                        owner.log.len() <= sent && !session.link.stopped()
                    })
                    .expect("owner lock");
                owner.log[sent..].to_vec()
            };
            if session.link.stopped() {
                return;
            }
            sent += fresh.len();
            for event in fresh {
                if signals.blocking_send(HostSignal::Event(event)).is_err() {
                    return;
                }
            }
        }
    }

    fn send_prompt(
        &self,
        _session: &LocalSession,
        text: String,
        _images: Vec<BackgroundImageAttachment>,
    ) -> anyhow::Result<()> {
        self.with(|owner| {
            owner.calls.push(HostCall::Prompt(text.clone()));
            owner.prompt(&text);
        });
        Ok(())
    }

    fn cancel_turn(&self, _session: &LocalSession) -> anyhow::Result<bool> {
        Ok(self.with(|owner| {
            owner.calls.push(HostCall::Cancel);
            let running = owner.busy;
            if running {
                owner.end_turn("cancelled");
            }
            running
        }))
    }

    fn set_model(
        &self,
        _session: &LocalSession,
        model: &str,
    ) -> anyhow::Result<SessionOptionAppliesFrom> {
        self.with(|owner| owner.calls.push(HostCall::SetModel(model.to_string())));
        Ok(SessionOptionAppliesFrom::NextTurn)
    }

    fn set_permission_mode(&self, _session: &LocalSession, _mode: &str) -> anyhow::Result<()> {
        anyhow::bail!("not exercised here")
    }

    fn answer_permission(
        &self,
        _session: &LocalSession,
        request_id: &str,
        option: RebonPermissionOption,
    ) -> anyhow::Result<AnswerOutcome> {
        Ok(self.with(|owner| {
            owner
                .calls
                .push(HostCall::Answer(request_id.to_string(), option));
            owner.answer(request_id, option)
        }))
    }

    fn answer_question(
        &self,
        _session: &LocalSession,
        _request_id: &str,
        _answers: Vec<ForegroundQuestionAnswer>,
    ) -> anyhow::Result<AnswerOutcome> {
        // The script never asks a question, so no answer can land.
        Ok(AnswerOutcome::NotPending)
    }

    fn backfill(
        &self,
        _session: &LocalSession,
        _epoch: u64,
        _after: u64,
        _before: u64,
    ) -> anyhow::Result<Vec<(u64, Value)>> {
        // The follower never drops events, so the owner never reports a gap.
        Ok(Vec::new())
    }
}

/// A reply long enough to be split across frames, with multi-byte
/// characters so a split on a byte boundary would show.
fn long_reply() -> String {
    "块ab".repeat(20_000)
}

fn projection() -> PermissionProjection {
    Arc::new(|query: &BackgroundPermissionQuerySnapshot| {
        json!({
            "sessionId": query.session_id,
            "toolCall": {"toolCallId": query.tool_call_id, "title": query.title},
            "options": query.options.iter().map(|option| json!({
                "optionId": option.option_id,
                "name": option.label,
                "kind": option.kind,
            })).collect::<Vec<_>>(),
        })
    })
}

// ─── The runner ────────────────────────────────────────────────────────

struct Runner {
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Runner {
    /// What `rebon rc serve --project <project>` does, with the scripted
    /// host in place of the local session host and shorter timings.
    async fn start(home: &Path, project: &Path, host: Arc<ScriptedHost>) -> Self {
        let dir = RcDir::new(home);
        let credentials = login::require_credentials(&dir).expect("logged in");
        let mut http = HttpClientConfig::new(credentials.server.clone());
        http.poll_wait = Duration::from_secs(1);
        http.poll_timeout_margin = Duration::from_secs(5);
        http.request_timeout = Duration::from_secs(10);
        let client = HttpBridgeApiClient::new(
            http,
            DeviceCredentials::new(String::new(), credentials.refresh_token.clone()),
        )
        .expect("runner client");
        client.refresh_now().await.expect("the device is accepted");
        let context = WorkContext {
            api: Arc::new(HttpEnvironmentApi(client)),
            streams: Arc::new(WsConnector),
            host,
            ledger: Ledger::new(dir.clone()),
            projection: projection(),
            environment_id: String::new(),
            timing: WorkTiming {
                heartbeat_interval: Duration::from_millis(300),
                lease_grace: Duration::from_secs(10),
                flush_interval: Duration::from_millis(50),
                reconnect_base: Duration::from_millis(100),
                reconnect_max: Duration::from_secs(1),
                outbox_max_bytes: 32 * 1024 * 1024,
                drain_timeout: Duration::from_secs(3),
            },
            policy: RemotePermissionPolicy::one_shot(),
            pid: std::process::id(),
        };
        let config = ServeConfig {
            server: credentials.server,
            machine_name: "rc-e2e".into(),
            max_sessions: 2,
            configured: Arc::new(|| Ok(Vec::new())),
            project_flags: vec![project.to_path_buf()],
            start_dir: project.to_path_buf(),
            timing: ServeTiming {
                project_refresh: Duration::from_secs(60),
                retry_base: Duration::from_millis(50),
                retry_max: Duration::from_millis(500),
                shutdown_grace: Duration::from_secs(5),
            },
        };
        let (shutdown, stop) = oneshot::channel();
        let task = tokio::spawn(async move {
            serve(&dir, context, config, async {
                let _ = stop.await;
            })
            .await
        });
        Self { shutdown, task }
    }

    /// Ctrl+C: every item stands down, and `serve` returns cleanly.
    async fn stop(self) {
        let _ = self.shutdown.send(());
        tokio::time::timeout(STEP, self.task)
            .await
            .expect("the runner stopped in time")
            .expect("the runner task")
            .expect("the runner exited cleanly");
    }
}

// ─── The controller ────────────────────────────────────────────────────

struct Controller {
    api: HttpBridgeApiClient,
    tx: SessionStreamTx,
    rx: SessionStreamRx,
    /// Every frame delivered on the socket, in order.
    seen: Vec<DeliveredFrame>,
}

fn http_config(server: &RcServer) -> HttpClientConfig {
    let mut config = HttpClientConfig::new(server.base.clone());
    config.request_timeout = Duration::from_secs(10);
    config
}

/// The first device of the instance: the controller's.
async fn controller_device(server: &RcServer) -> IssuedDevice {
    HttpBridgeApiClient::issue_device(
        &http_config(server),
        &server.bootstrap_token,
        &IssueDeviceRequest {
            label: Some("controller".into()),
        },
    )
    .await
    .expect("bootstrap the controller device")
}

impl Controller {
    fn api(server: &RcServer, device: &IssuedDevice) -> HttpBridgeApiClient {
        HttpBridgeApiClient::new(
            http_config(server),
            DeviceCredentials::new(device.access_token.clone(), device.refresh_token.clone()),
        )
        .expect("controller client")
    }

    async fn attach(server: &RcServer, api: HttpBridgeApiClient, session_id: &str) -> Self {
        let url = format!("{}/v1/sessions/{session_id}/stream", server.ws_base);
        let stream = SessionStream::connect(&SessionStreamOptions::new(url, api.access_token()))
            .await
            .expect("the controller attaches");
        let (tx, rx) = stream.split();
        Self {
            api,
            tx,
            rx,
            seen: Vec::new(),
        }
    }

    async fn send(&self, frame: SessionFrame) {
        self.tx.send(&frame).await.expect("the controller sends");
    }

    async fn next(&mut self) -> SessionFrame {
        let delivered = tokio::time::timeout(STEP, self.rx.recv_delivered())
            .await
            .unwrap_or_else(|_| panic!("no frame in time; so far: {:#?}", self.kinds()))
            .expect("the controller's stream is open")
            .expect("a readable frame");
        if let SessionFrame::StreamError { code, message, .. } = &delivered.frame {
            panic!("the server refused a frame: {code}: {message}");
        }
        self.seen.push(delivered.clone());
        delivered.frame
    }

    /// Frames until one matches; the ones before it are kept in `seen`.
    async fn until(&mut self, wanted: impl Fn(&SessionFrame) -> bool) -> SessionFrame {
        loop {
            let frame = self.next().await;
            if wanted(&frame) {
                return frame;
            }
        }
    }

    async fn until_state(&mut self, word: SessionRunState) {
        self.until(
            |frame| matches!(frame, SessionFrame::SessionState { state, .. } if *state == word),
        )
        .await;
    }

    /// Like [`Self::until_state`], but a matching frame already seen at or
    /// after `index` counts.
    async fn until_state_since(&mut self, index: usize, word: SessionRunState) {
        let seen = self.seen[index..].iter().any(|delivered| {
            matches!(&delivered.frame, SessionFrame::SessionState { state, .. } if *state == word)
        });
        if !seen {
            self.until_state(word).await;
        }
    }

    fn kinds(&self) -> Vec<String> {
        self.seen
            .iter()
            .map(|delivered| match &delivered.frame {
                SessionFrame::SessionState { state, .. } => format!("session_state {state:?}"),
                SessionFrame::SessionMessage { message_id, .. } => {
                    format!("session_message {message_id:?}")
                }
                other => other.frame_type().to_string(),
            })
            .collect()
    }

    /// The text of every message seen, pieces joined, by base message id.
    fn texts(&self) -> BTreeMap<String, String> {
        let mut pieces: BTreeMap<String, BTreeMap<usize, String>> = BTreeMap::new();
        for delivered in &self.seen {
            let SessionFrame::SessionMessage {
                message_id: Some(id),
                message,
            } = &delivered.frame
            else {
                continue;
            };
            let text = message["update"]["content"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let (base, index) = match id.rsplit_once(".p") {
                Some((base, index)) if index.parse::<usize>().is_ok() => {
                    (base.to_string(), index.parse().unwrap())
                }
                _ => (id.clone(), 0),
            };
            let previous = pieces.entry(base).or_default().insert(index, text);
            assert!(previous.is_none(), "message {id} was delivered twice");
        }
        pieces
            .into_iter()
            .map(|(base, parts)| (base, parts.into_values().collect()))
            .collect()
    }

    fn saw_text(&self, text: &str) -> bool {
        self.texts().values().any(|seen| seen == text)
    }

    async fn until_text(&mut self, text: &str) {
        while !self.saw_text(text) {
            self.next().await;
        }
    }

    /// The session's whole persisted history, oldest first.
    async fn history(&self, session_id: &str) -> Vec<rebon_bridge::history::SessionEventRecord> {
        let mut events = Vec::new();
        let mut page = PageRequest::first().with_limit(500);
        loop {
            let got = self
                .api
                .session_events(session_id, &page)
                .await
                .expect("read the history");
            events.extend(got.events);
            match got.next_cursor {
                Some(cursor) => page = PageRequest::after(cursor).with_limit(500),
                None => break,
            }
        }
        events.sort_by_key(|event| event.event_id);
        events
    }

    /// What the socket delivered agrees with what RC stored: event ids
    /// only go up, every worker frame in the history was delivered exactly
    /// once and under the same id, no `message_id` is stored twice, and
    /// what is stored beyond that is the controller's own frames.
    async fn check_against_history(&self, session_id: &str) {
        let history = self.history(session_id).await;
        let live: Vec<u64> = self
            .seen
            .iter()
            .map(|delivered| {
                delivered
                    .event_id
                    .expect("a delivered frame carries its event id")
            })
            .collect();
        assert!(
            live.windows(2).all(|pair| pair[0] < pair[1]),
            "event ids went backwards or repeated on the socket: {live:?}"
        );
        assert!(
            history
                .windows(2)
                .all(|pair| pair[0].event_id < pair[1].event_id),
            "the history repeats an event id"
        );
        let stored: BTreeMap<u64, &rebon_bridge::history::SessionEventRecord> = history
            .iter()
            .map(|event| (event.event_id, event))
            .collect();
        for delivered in &self.seen {
            let id = delivered.event_id.unwrap();
            let record = stored
                .get(&id)
                .unwrap_or_else(|| panic!("event {id} was delivered but is not stored"));
            assert_eq!(
                record.frame().expect("a stored frame"),
                delivered.frame,
                "event {id} differs between the socket and the history"
            );
        }
        let live: BTreeSet<u64> = live.into_iter().collect();
        let mut message_ids = BTreeSet::new();
        for record in &history {
            let frame = record.frame().expect("a stored frame");
            match frame.origin() {
                Some(FrameOrigin::Worker) => assert!(
                    live.contains(&record.event_id),
                    "worker event {} ({}) was stored but never delivered",
                    record.event_id,
                    record.kind
                ),
                _ => assert!(
                    !live.contains(&record.event_id),
                    "the controller was sent its own frame back"
                ),
            }
            if let SessionFrame::SessionMessage {
                message_id: Some(id),
                ..
            } = &frame
            {
                assert!(
                    message_ids.insert(id.clone()),
                    "message {id} is stored twice"
                );
            }
        }
    }
}

// ─── Setup shared by the tests ─────────────────────────────────────────

struct Machine {
    home: tempfile::TempDir,
    project: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("config home"),
            project: tempfile::tempdir().expect("project"),
        }
    }

    fn rc_dir(&self) -> RcDir {
        RcDir::new(self.home.path())
    }

    fn project(&self) -> PathBuf {
        self.project.path().to_path_buf()
    }
}

/// `rebon rc login --token-kind access`: this machine joins the
/// controller's account as a device of its own.
async fn login_machine(server: &RcServer, machine: &Machine, controller: &IssuedDevice) {
    let stored = login::login(
        &machine.rc_dir(),
        &server.base,
        LoginCredential::AccessToken(controller.access_token.clone()),
        Some("rc-e2e machine".into()),
    )
    .await
    .expect("rebon rc login");
    assert_eq!(stored.account_id, controller.account_id);
    assert_ne!(stored.device_id, controller.device_id);
    assert!(machine.rc_dir().credentials_path().is_file());
}

/// The environment the runner registered, once it advertises its project.
async fn wait_for_environment(api: &HttpBridgeApiClient) -> (String, String) {
    let deadline = tokio::time::Instant::now() + STEP;
    loop {
        let list = api.list_environments().await.expect("list environments");
        if let Some(environment) = list.environments.iter().find(|environment| {
            environment.deregistered_at.is_none() && !environment.projects.is_empty()
        }) {
            assert_eq!(environment.worker_type, "rebon");
            assert_eq!(environment.projects.len(), 1);
            return (
                environment.environment_id.clone(),
                environment.projects[0].path.clone(),
            );
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the runner never registered: {list:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn enqueue(
    api: &HttpBridgeApiClient,
    environment_id: &str,
    project: &str,
    prompt: &str,
) -> String {
    let mut request = EnqueueWorkRequest::session(prompt);
    request.project = Some(project.to_string());
    let queued = api
        .enqueue_work(environment_id, &request)
        .await
        .expect("enqueue work");
    queued.session_id.expect("session work has a session")
}

fn bound_to(frame: &SessionFrame) -> bool {
    matches!(frame, SessionFrame::SessionBound { rebon_session_id } if rebon_session_id == LOCAL_SESSION)
}

async fn within<F: std::future::Future<Output = ()>>(test: F) {
    tokio::time::timeout(WHOLE, test)
        .await
        .expect("the test finished in time");
}

// ─── Tests ─────────────────────────────────────────────────────────────

/// §14.7 in one go: login, serve, queue work over HTTP, and drive the
/// session from a controller's socket — prompt, a long reply, a
/// permission answered remotely, a model option, a cancel — then lose the
/// runner's connection and carry on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_controller_drives_a_served_session_end_to_end() {
    within(async {
        let server = start_server().await;
        let controller_device = controller_device(&server).await;
        let machine = Machine::new();
        login_machine(&server, &machine, &controller_device).await;
        let host = Arc::new(ScriptedHost::default());
        let runner = Runner::start(machine.home.path(), &machine.project(), Arc::clone(&host)).await;

        let api = Controller::api(&server, &controller_device);
        let (environment_id, project) = wait_for_environment(&api).await;
        let session_id = enqueue(&api, &environment_id, &project, "hello").await;
        let mut controller = Controller::attach(&server, api, &session_id).await;

        // The item is taken, the session opened and bound, the first
        // prompt run and its reply uplinked.
        controller.until_state(SessionRunState::Starting).await;
        controller.until(bound_to).await;
        controller.until_text("echo: hello").await;
        controller.until_state(SessionRunState::Idle).await;
        assert_eq!(
            host.calls()[0],
            HostCall::Open(OpenRequest {
                project: project.clone(),
                resume: None,
            })
        );
        assert_eq!(host.count(&HostCall::Prompt("hello".into())), 1);
        let ledger = Ledger::new(machine.rc_dir());
        let entry = ledger
            .session(&session_id)
            .expect("read the ledger")
            .expect("the session is in the ledger");
        assert_eq!(entry.rebon_session_id, LOCAL_SESSION);
        assert_eq!(entry.environment_id, environment_id);

        // A prompt from the socket, with a reply too long for one frame:
        // it arrives in pieces that join back into the original.
        controller
            .send(SessionFrame::Prompt {
                text: "[long] tell me everything".into(),
                attachments: Vec::new(),
            })
            .await;
        controller.until_state(SessionRunState::Running).await;
        controller.until_text(&long_reply()).await;
        controller.until_state(SessionRunState::Idle).await;
        let pieces: Vec<String> = controller
            .seen
            .iter()
            .filter_map(|delivered| match &delivered.frame {
                SessionFrame::SessionMessage {
                    message_id: Some(id),
                    ..
                } if id.contains(".p") => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert!(pieces.len() >= 2, "the long reply was not split: {pieces:?}");
        assert!(
            pieces.iter().all(|id| id.starts_with(&format!("e{EPOCH}."))),
            "{pieces:?}"
        );

        // A permission prompt goes out, the remote allow comes back as a
        // one-shot answer, and the turn finishes.
        controller
            .send(SessionFrame::Prompt {
                text: "[permission] list the files".into(),
                attachments: Vec::new(),
            })
            .await;
        let request = controller
            .until(|frame| matches!(frame, SessionFrame::PermissionRequest { .. }))
            .await;
        let SessionFrame::PermissionRequest {
            request_id,
            request,
        } = request
        else {
            unreachable!()
        };
        assert!(request_id.starts_with("perm-"), "{request_id}");
        assert!(
            !request.to_string().contains("owner-ipc-token"),
            "the owner's IPC token left the machine"
        );
        let kinds: Vec<&str> = request["options"]
            .as_array()
            .expect("options")
            .iter()
            .filter_map(|option| option["kind"].as_str())
            .collect();
        assert_eq!(kinds, ["allow_once", "reject_once"], "{request}");
        assert_eq!(request["_meta"]["rebonRc"]["answerable"], true, "{request}");
        controller.until_state(SessionRunState::NeedsInput).await;
        let allow = SessionFrame::PermissionResponse {
            response: PermissionResponseBody::success(&request_id, json!({"behavior": "allow"})),
        };
        controller.send(allow.clone()).await;
        host.wait_for(HostCall::Answer(
            request_id.clone(),
            RebonPermissionOption::AllowOnce,
        ))
        .await;
        controller.until_text("the command was allowed").await;
        controller.until_state(SessionRunState::Idle).await;

        // Answering it again: the owner is no longer parked on it.
        controller.send(allow).await;
        let stale = controller
            .until(|frame| matches!(frame, SessionFrame::ControlResponse { .. }))
            .await;
        let SessionFrame::ControlResponse { response } = stale else {
            unreachable!()
        };
        assert_eq!(response.request_id, request_id);
        assert!(
            response.error.as_deref().unwrap_or_default().contains("no longer pending"),
            "{response:?}"
        );

        // A control request is tried on the owner and answered.
        controller
            .send(SessionFrame::ControlRequest {
                request_id: "ctl-1".into(),
                subtype: "set_model".into(),
                params: json!({"model": "scripted-2"}),
            })
            .await;
        let answered = controller
            .until(|frame| {
                matches!(frame, SessionFrame::ControlResponse { response } if response.request_id == "ctl-1")
            })
            .await;
        let SessionFrame::ControlResponse { response } = answered else {
            unreachable!()
        };
        assert_eq!(response.subtype, "success", "{response:?}");
        assert_eq!(host.count(&HostCall::SetModel("scripted-2".into())), 1);

        // A turn that would run forever, cancelled from the socket.
        controller
            .send(SessionFrame::Prompt {
                text: "[slow] count to a million".into(),
                attachments: Vec::new(),
            })
            .await;
        controller.until_text("working on it").await;
        controller.send(SessionFrame::Cancel).await;
        host.wait_for(HostCall::Cancel).await;
        controller.until_state(SessionRunState::Idle).await;

        // The network drops the runner's connection. What the owner says
        // meanwhile is not lost, the runner comes back on the same work
        // item and session, and the session takes prompts again.
        let connections = server.proxy.accepted();
        let before_cut = controller.seen.len();
        server.proxy.cut_all();
        host.say("said while the link was down");
        controller.until_text("said while the link was down").await;
        // A reconnect says the current state again — before or after the
        // message, depending on which the runner had queued first.
        controller
            .until_state_since(before_cut, SessionRunState::Idle)
            .await;
        assert!(
            server.proxy.accepted() > connections,
            "the runner never dialled again"
        );
        controller
            .send(SessionFrame::Prompt {
                text: "still there?".into(),
                attachments: Vec::new(),
            })
            .await;
        controller.until_text("echo: still there?").await;
        controller.until_state(SessionRunState::Idle).await;
        let opens = host
            .calls()
            .into_iter()
            .filter(|call| matches!(call, HostCall::Open(_)))
            .count();
        assert_eq!(opens, 1, "a reconnect must not open the session again");

        runner.stop().await;
        controller.check_against_history(&session_id).await;
        // No frame was sent twice for one delta, even across the reconnect.
        controller.texts();
    })
    .await;
}

/// A runner that stops and starts again gets its work item back once the
/// lease lapses, resumes the same local session, and does not run the
/// item's prompt a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_runner_resumes_the_same_session_without_rerunning_its_prompt() {
    within(async {
        let server = start_server().await;
        let controller_device = controller_device(&server).await;
        let machine = Machine::new();
        login_machine(&server, &machine, &controller_device).await;
        // The machine's session host: it outlives both runners.
        let host = Arc::new(ScriptedHost::default());
        let first = Runner::start(machine.home.path(), &machine.project(), Arc::clone(&host)).await;

        let api = Controller::api(&server, &controller_device);
        let (environment_id, project) = wait_for_environment(&api).await;
        let session_id = enqueue(&api, &environment_id, &project, "first prompt").await;
        let mut controller = Controller::attach(&server, api, &session_id).await;
        controller.until(bound_to).await;
        controller.until_text("echo: first prompt").await;
        controller.until_state(SessionRunState::Idle).await;

        // Ctrl+C. The work item is left leased, to lapse.
        first.stop().await;
        let second =
            Runner::start(machine.home.path(), &machine.project(), Arc::clone(&host)).await;

        // The same item comes back to the new runner, which resumes the
        // session the ledger remembers.
        let resumed = HostCall::Open(OpenRequest {
            project: project.clone(),
            resume: Some(LOCAL_SESSION.into()),
        });
        host.wait_for(resumed).await;
        controller.until_state(SessionRunState::Starting).await;
        controller.until_state(SessionRunState::Idle).await;
        let (environment_again, _) = wait_for_environment(&controller.api).await;
        assert_eq!(
            environment_again, environment_id,
            "the machine kept its environment"
        );

        controller
            .send(SessionFrame::Prompt {
                text: "second prompt".into(),
                attachments: Vec::new(),
            })
            .await;
        controller.until_text("echo: second prompt").await;
        controller.until_state(SessionRunState::Idle).await;
        assert_eq!(host.count(&HostCall::Prompt("first prompt".into())), 1);
        assert_eq!(host.count(&HostCall::Prompt("second prompt".into())), 1);

        second.stop().await;
        controller.check_against_history(&session_id).await;
        let summary = controller
            .api
            .list_sessions(Some(&environment_id), &PageRequest::first())
            .await
            .expect("list sessions");
        assert_eq!(summary.sessions.len(), 1, "one RC session throughout");
    })
    .await;
}

/// `rebon rc login` with the instance's bootstrap token creates the
/// account once; a second machine cannot reuse the token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bootstrap_token_is_spent_once() {
    within(async {
        let server = start_server().await;
        let machine = Machine::new();
        login::login(
            &machine.rc_dir(),
            &server.base,
            LoginCredential::Bootstrap(server.bootstrap_token.clone()),
            None,
        )
        .await
        .expect("the first login takes the bootstrap token");
        let again = login::login(
            &Machine::new().rc_dir(),
            &server.base,
            LoginCredential::Bootstrap(server.bootstrap_token.clone()),
            None,
        )
        .await
        .expect_err("the bootstrap token is spent");
        let unauthorized = again.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<BridgeApiError>(),
                Some(BridgeApiError::Unauthorized(_))
            )
        });
        assert!(unauthorized, "{again:#}");
    })
    .await;
}
