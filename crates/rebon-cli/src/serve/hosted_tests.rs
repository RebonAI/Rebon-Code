//! `rebon serve` against a real session owner.
//!
//! Nothing here is a mock of the owner: the session's active lock is taken,
//! `<sid>.owner.json` is published beside it, an endpoint listens on
//! loopback, and the events the tabs see are pushed through the same ring a
//! worker's turn pushes them through. Only the engine behind it is scripted
//! — which is the one thing a test must not spend a provider call on.
//!
//! What that buys is the acceptance row for stage 4: three clients on one
//! session (two ACP tabs and a terminal-shaped subscriber), one stream,
//! first-answer-wins on a permission, and the other two converging on the
//! answer.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long these tests wait for a step that takes milliseconds when nothing
/// is wrong.
///
/// Thirty seconds rather than five. Every wait here is for a hosted call, and
/// a hosted call costs one connection: the owner's accept loop polls a
/// non-blocking listener every 50 ms, so a connection waits up to that long to
/// be accepted before anything it asked for begins. A `session/load` that
/// attaches an owner and reads its metadata is a chain of those, and under a
/// full-suite load the chain stretches. Nothing about that is this test's
/// subject, and it is not a bug this test can catch.
///
/// Raising it costs nothing when the test passes -- the correct path is 0.3
/// seconds -- and costs a slower red when it fails. That trade is right for a
/// wait whose expiry has never once meant a real defect here.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

use rebon_core::permission::{
    ChannelPermissionBroker, OutboundPermissionQuery, PermissionAnswer, PermissionOptionKind,
    PermissionQueryOption,
};
use rebon_proto::types::{ConfigOption, ConfigOptionType, ConfigOptionValue};
use rebon_session_host::{
    BackgroundJobState, BackgroundJobStatus, BackgroundRuntimeFields, BackgroundStore, SessionEvent,
};
use rebon_types::SessionUpdateParams;

use crate::background::{start_background_ipc_server, BackgroundIpcServer};
use crate::serve::hosted::HostedSessions;
use crate::serve::mux::{AcpMux, ClientId};

const SESSION_ID: &str = "sess-serve-hosted";

fn runtime_fields() -> BackgroundRuntimeFields {
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

/// The option list this process owns, in the shape the ACP handler keeps it.
fn config_options() -> Vec<ConfigOption> {
    vec![
        ConfigOption {
            id: "permissions".to_string(),
            name: "Permissions".to_string(),
            description: None,
            category: None,
            option_type: ConfigOptionType::Select,
            current_value: "default".to_string(),
            options: ["default", "plan", "bypassPermissions"]
                .into_iter()
                .map(|value| ConfigOptionValue {
                    value: value.to_string(),
                    name: value.to_string(),
                    description: None,
                })
                .collect(),
        },
        ConfigOption {
            id: "effort".to_string(),
            name: "Effort".to_string(),
            description: None,
            category: None,
            option_type: ConfigOptionType::Select,
            current_value: "auto".to_string(),
            options: ["auto", "high"]
                .into_iter()
                .map(|value| ConfigOptionValue {
                    value: value.to_string(),
                    name: value.to_string(),
                    description: None,
                })
                .collect(),
        },
        ConfigOption {
            id: "sub_agents".to_string(),
            name: "Sub-agents".to_string(),
            description: None,
            category: None,
            option_type: ConfigOptionType::Select,
            current_value: "on".to_string(),
            options: ["on", "off"]
                .into_iter()
                .map(|value| ConfigOptionValue {
                    value: value.to_string(),
                    name: value.to_string(),
                    description: None,
                })
                .collect(),
        },
    ]
}

/// One session, one owner, one mux — everything but the model.
struct Owned {
    _dir: tempfile::TempDir,
    projects_root: PathBuf,
    cwd: String,
    store: BackgroundStore,
    state: BackgroundJobState,
    ipc: BackgroundIpcServer,
    _lock: rebon_session::SessionActiveLock,
    hosted: Arc<HostedSessions>,
    mux: AcpMux,
    to_server: UnboundedReceiver<Value>,
    applied_locally: Arc<Mutex<Vec<(String, String)>>>,
}

impl Owned {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let projects_root = dir.path().join("projects");
        let cwd_path = dir.path().join("work");
        std::fs::create_dir_all(&cwd_path).unwrap();
        let cwd = cwd_path.to_string_lossy().into_owned();

        // The transcript the owner resumes, and the lock that says it is its.
        let transcript =
            rebon_session::ensure_session_file_path(&projects_root, &cwd, SESSION_ID).unwrap();
        std::fs::write(&transcript, b"").unwrap();

        let store = BackgroundStore::new(dir.path().join("jobs"));
        let mut state = store
            .create_job("hosted".into(), cwd_path.clone(), runtime_fields())
            .unwrap();
        state.identity.session_id = Some(SESSION_ID.to_string());
        state.lease.placement = rebon_session_host::JobPlacement::Foreground;
        let ipc = start_background_ipc_server(&store, &state.identity.job_id).unwrap();
        let owner = ipc.owner();
        state.process.pid = Some(owner.endpoint.pid);
        state.process.pid_identity = owner.pid_identity.clone();
        state.process.ipc_port = Some(owner.endpoint.port);
        state.process.ipc_token = Some(owner.endpoint.token.clone());
        store.write_state(&state).unwrap();

        let lock = rebon_session::try_acquire_session_active_lock(&projects_root, &cwd, SESSION_ID)
            .unwrap()
            .expect("the owner takes the session's lock");
        rebon_session::write_session_owner(
            &projects_root,
            &cwd,
            SESSION_ID,
            &rebon_session::SessionOwnerDescriptor {
                version: rebon_session::SESSION_OWNER_VERSION,
                pid: std::process::id(),
                pid_identity: ipc.pid_identity.clone(),
                surface: rebon_session::SessionOwnerSurface::Worker,
                job_id: Some(state.identity.job_id.clone()),
                ipc_port: Some(ipc.port),
                ipc_token: Some(ipc.token.clone()),
                started_at_ms: rebon_session_host::now_ms(),
            },
        )
        .unwrap();

        let (to_server_tx, mut to_server) = tokio::sync::mpsc::unbounded_channel();
        let mux = AcpMux::new(to_server_tx);
        let init = to_server.try_recv().expect("the mux initializes first");
        mux.on_server_message(
            json!({ "jsonrpc": "2.0", "id": init["id"], "result": { "protocolVersion": 1 } }),
        );

        let applied_locally = Arc::new(Mutex::new(Vec::new()));
        let sink = applied_locally.clone();
        let hosted = HostedSessions::with_store(
            projects_root.clone(),
            cwd.clone(),
            store.clone(),
            runtime_fields(),
            Arc::new(Mutex::new(config_options())),
            Some(Arc::new(move |id: &str, value: &str| {
                sink.lock()
                    .unwrap()
                    .push((id.to_string(), value.to_string()))
            })),
            Arc::new(rebon_acp::ServerState::new()),
            tokio::runtime::Handle::current(),
        );
        hosted.attach_mux(mux.clone());
        mux.attach_hosted(hosted.clone());

        Self {
            _dir: dir,
            projects_root,
            cwd,
            store,
            state,
            ipc,
            _lock: lock,
            hosted,
            mux,
            to_server,
            applied_locally,
        }
    }

    /// Open the session from one tab, answering the metadata request the
    /// router makes of the ACP server, and wait until the owner is attached.
    async fn open(&mut self, client: ClientId, id: i64) -> Value {
        self.send_load(client, id);
        let request = self.next_server_request().await;
        self.answer_load(&request)
    }

    /// The three steps `open` takes, separately.
    ///
    /// A test that needs two loads in flight at once has to be able to stop
    /// between them; `open` runs all three and cannot be interleaved with
    /// itself.
    fn send_load(&self, client: ClientId, id: i64) {
        self.mux.on_client_message(
            client,
            &json!({
                "jsonrpc": "2.0", "id": id, "method": "session/load",
                "params": { "sessionId": SESSION_ID, "cwd": self.cwd, "mcpServers": [] }
            })
            .to_string(),
        );
    }

    async fn next_server_request(&mut self) -> Value {
        let request = tokio::time::timeout(SETUP_TIMEOUT, self.to_server.recv())
            .await
            .expect("the router asks the server for the session's metadata")
            .unwrap();
        assert_eq!(request["method"], "session/load");
        assert_eq!(request["params"]["sessionId"], SESSION_ID);
        request
    }

    fn answer_load(&self, request: &Value) -> Value {
        let result = json!({
            "sessionId": SESSION_ID,
            "configOptions": config_options(),
            "slashCommands": [],
        });
        self.mux
            .on_server_message(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }));
        result
    }

    /// Wait until the owner's record shows this process holding a lease.
    async fn wait_for_lease(&self) {
        let deadline = Instant::now() + SETUP_TIMEOUT;
        while self.serve_leases() == 0 {
            assert!(Instant::now() < deadline, "no lease was ever taken");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn serve_leases(&self) -> usize {
        self.store
            .read_state(&self.state.identity.job_id)
            .unwrap()
            .lease
            .client_leases
            .len()
    }

    async fn wait_for_owner(&self) {
        let deadline = Instant::now() + SETUP_TIMEOUT;
        while self.hosted.owner_for(SESSION_ID).is_none() {
            assert!(Instant::now() < deadline, "the router never attached");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn publish_update(&self, text: &str) {
        self.ipc.events.publish_update(&SessionUpdateParams {
            session_id: SESSION_ID.to_string(),
            update: rebon_types::SessionUpdate::AgentMessageChunk {
                content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: text.to_string(),
                    annotations: None,
                }),
            },
        });
    }
}

/// Drain whatever has arrived for a client, as parsed messages.
fn drained(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(text) = rx.try_recv() {
        out.push(serde_json::from_str(&text).unwrap());
    }
    out
}

/// Wait until `predicate` holds over everything a client has been sent.
async fn wait_for(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    seen: &mut Vec<Value>,
    what: &str,
    predicate: impl Fn(&[Value]) -> bool,
) {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        seen.extend(drained(rx));
        if predicate(seen) {
            return;
        }
        assert!(Instant::now() < deadline, "never saw {what}: {seen:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn has_method(seen: &[Value], method: &str) -> bool {
    seen.iter().any(|value| value["method"] == method)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tab_opens_a_session_the_worker_owns_and_both_tabs_see_the_same_stream() {
    let mut owned = Owned::start();
    let (a, mut a_rx) = owned.mux.connect();
    let (_b, mut b_rx) = owned.mux.connect();

    owned.open(a, 1).await;
    owned.wait_for_owner().await;

    // This process took no lock of its own: the owner still holds the only one.
    assert!(rebon_session::is_session_active(
        &owned.projects_root,
        &owned.cwd,
        SESSION_ID
    ));
    // And it said so: the lease is in the owner's job record.
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        let leases = owned
            .store
            .read_state(&owned.state.identity.job_id)
            .unwrap()
            .lease
            .client_leases;
        if leases
            .iter()
            .any(|lease| lease.kind == rebon_session_host::ClientLeaseKind::Serve)
        {
            break;
        }
        assert!(Instant::now() < deadline, "serve never took a lease");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    owned.publish_update("hello from the worker");
    owned
        .ipc
        .events
        .publish_turn(rebon_session_host::TurnStreamState::Running, None);

    let mut a_seen = Vec::new();
    let mut b_seen = Vec::new();
    let is_chunk = |value: &Value| {
        value["method"] == "session/update"
            && value["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
    };
    wait_for(&mut a_rx, &mut a_seen, "the update in tab A", |seen| {
        seen.iter().any(is_chunk)
    })
    .await;
    wait_for(&mut b_rx, &mut b_seen, "the update in tab B", |seen| {
        seen.iter().any(is_chunk)
    })
    .await;
    // A tab that never opened the session is still on the same stream — it
    // filters by session id, the way it always has.
    for seen in [&a_seen, &b_seen] {
        let update = seen.iter().find(|value| is_chunk(value)).unwrap();
        assert_eq!(update["params"]["sessionId"], SESSION_ID);
        assert_eq!(
            update["params"]["update"]["content"]["text"],
            "hello from the worker"
        );
        // Opening the session also handed the tab the owner's own values for
        // everything both of them have to agree about (invariant I4).
        assert!(
            seen.iter()
                .any(|value| value["params"]["update"]["sessionUpdate"] == "config_option_update"),
            "the owner's config options reach the tabs: {seen:?}"
        );
    }
    wait_for(&mut a_rx, &mut a_seen, "the turn in tab A", |seen| {
        seen.iter()
            .any(|value| value["method"] == "_serve/turn" && value["params"]["state"] == "running")
    })
    .await;
    assert!(owned.mux.is_turn_running(SESSION_ID));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_reaches_the_owner_and_is_answered_by_the_turn_it_started() {
    let mut owned = Owned::start();
    let (a, mut a_rx) = owned.mux.connect();
    owned.open(a, 1).await;
    owned.wait_for_owner().await;
    let _ = drained(&mut a_rx);

    owned.mux.on_client_message(
        a,
        &json!({
            "jsonrpc": "2.0", "id": 42, "method": "session/prompt",
            "params": { "sessionId": SESSION_ID, "prompt": [{ "type": "text", "text": "do the thing" }] }
        })
        .to_string(),
    );

    // It became the owner's pending prompt, not a turn in this process.
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        let state = owned
            .store
            .read_state(&owned.state.identity.job_id)
            .unwrap();
        if state.pending_prompt().map(|prompt| prompt.text.as_str()) == Some("do the thing") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the prompt never reached the owner"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        owned.to_server.try_recv().is_err(),
        "the ACP server behind the mux runs no turn for a hosted session"
    );

    // The waiter is armed by the turn starting, and answered by it ending.
    owned
        .ipc
        .events
        .publish_turn(rebon_session_host::TurnStreamState::Running, None);
    owned.publish_update("working");
    owned.ipc.events.publish_turn(
        rebon_session_host::TurnStreamState::Idle,
        Some("end_turn".into()),
    );

    let mut seen = Vec::new();
    wait_for(&mut a_rx, &mut seen, "the prompt response", |seen| {
        seen.iter().any(|value| value["id"] == 42)
    })
    .await;
    let response = seen.iter().find(|value| value["id"] == 42).unwrap();
    assert_eq!(response["result"]["stopReason"], "end_turn");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_option_the_owner_holds_goes_to_it_and_one_it_does_not_stays_here() {
    let mut owned = Owned::start();
    let (a, mut a_rx) = owned.mux.connect();
    owned.open(a, 1).await;
    owned.wait_for_owner().await;
    let _ = drained(&mut a_rx);

    owned.mux.on_client_message(
        a,
        &json!({
            "jsonrpc": "2.0", "id": 7, "method": "session/set_config_option",
            "params": { "sessionId": SESSION_ID, "configId": "permissions", "value": "bypassPermissions" }
        })
        .to_string(),
    );
    let mut seen = Vec::new();
    wait_for(&mut a_rx, &mut seen, "the option response", |seen| {
        seen.iter().any(|value| value["id"] == 7)
    })
    .await;
    // The owner is now enforcing it, and this process persisted nothing.
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        let status = owned
            .hosted
            .owner_for(SESSION_ID)
            .unwrap()
            .status()
            .unwrap();
        if status.permission_mode.as_deref() == Some("bypassPermissions") {
            break;
        }
        assert!(Instant::now() < deadline, "the owner never took the mode");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(owned.applied_locally.lock().unwrap().is_empty());

    // An option the owner records rather than switches (`effort` lands on the
    // next turn) is shown as chosen straight away: the answer names the value
    // this server just set and the owner accepted, without waiting for the
    // status that will carry it back.
    owned.mux.on_client_message(
        a,
        &json!({
            "jsonrpc": "2.0", "id": 11, "method": "session/set_config_option",
            "params": { "sessionId": SESSION_ID, "configId": "effort", "value": "high" }
        })
        .to_string(),
    );
    wait_for(&mut a_rx, &mut seen, "the effort response", |seen| {
        seen.iter().any(|value| value["id"] == 11)
    })
    .await;
    let effort = seen.iter().find(|value| value["id"] == 11).unwrap()["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == "effort")
        .unwrap()["currentValue"]
        .clone();
    assert_eq!(effort, "high");
    assert_eq!(
        owned
            .store
            .read_state(&owned.state.identity.job_id)
            .unwrap()
            .identity
            .runtime
            .effort_level
            .as_deref(),
        Some("high"),
        "and the owner is the one holding it"
    );

    // A `config.json` setting is not the session's to hold, and is applied here.
    owned.mux.on_client_message(
        a,
        &json!({
            "jsonrpc": "2.0", "id": 8, "method": "session/set_config_option",
            "params": { "sessionId": SESSION_ID, "configId": "sub_agents", "value": "off" }
        })
        .to_string(),
    );
    wait_for(&mut a_rx, &mut seen, "the local option response", |seen| {
        seen.iter().any(|value| value["id"] == 8)
    })
    .await;
    assert_eq!(
        *owned.applied_locally.lock().unwrap(),
        vec![("sub_agents".to_string(), "off".to_string())]
    );
    let response = seen.iter().find(|value| value["id"] == 8).unwrap();
    let options = response["result"]["configOptions"].as_array().unwrap();
    let sub_agents = options
        .iter()
        .find(|option| option["id"] == "sub_agents")
        .unwrap();
    assert_eq!(sub_agents["currentValue"], "off");
}

/// The acceptance row for stage 4: three clients on one session, one stream,
/// first-answer-wins, and the losers converging on the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn three_clients_share_one_permission_and_the_first_answer_wins() {
    let mut owned = Owned::start();
    owned.state.process.status = BackgroundJobStatus::Running;
    owned.state.process.turn_generation = 1;
    owned.store.write_state(&owned.state).unwrap();

    let (a, mut a_rx) = owned.mux.connect();
    let (b, mut b_rx) = owned.mux.connect();
    owned.open(a, 1).await;
    owned.open(b, 2).await;
    owned.wait_for_owner().await;

    // The third client is a terminal: it does not speak ACP at all, it
    // subscribes to the owner the way a mirror does.
    let owner = owned.hosted.owner_for(SESSION_ID).unwrap();
    let (mirror_tx, mirror_rx) = std::sync::mpsc::channel();
    let (subscribed_tx, subscribed_rx) = std::sync::mpsc::channel();
    let mirror_owner = owner.clone();
    std::thread::spawn(move || {
        let Ok(stream) = mirror_owner.subscribe(None) else {
            return;
        };
        if subscribed_tx.send(()).is_err() {
            return;
        }
        for event in stream {
            if mirror_tx.send(event).is_err() {
                return;
            }
        }
    });
    subscribed_rx
        .recv_timeout(SETUP_TIMEOUT)
        .expect("the terminal subscribes to the owner");

    let _turn = owned.ipc.start_turn();
    let (broker, receiver) = ChannelPermissionBroker::new("serve-three-clients");
    owned.ipc.attach_permission_receiver(1, receiver);
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    broker.forward_direct(OutboundPermissionQuery {
        id: 11,
        tool_name: "Bash".into(),
        tool_call_id: "call-serve".into(),
        session_id: SESSION_ID.into(),
        title: "Run a command".into(),
        message: "rm -rf /tmp/x".into(),
        tool_input: Some(json!({ "command": "rm -rf /tmp/x" })),
        metadata: None,
        options: vec![
            PermissionQueryOption {
                option_id: "allow_once".into(),
                label: "Allow once".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionQueryOption {
                option_id: "reject_once".into(),
                label: "Reject".into(),
                kind: PermissionOptionKind::RejectOnce,
            },
        ],
        response_tx,
    });

    let mut a_seen = Vec::new();
    let mut b_seen = Vec::new();
    for (rx, seen, who) in [
        (&mut a_rx, &mut a_seen, "tab A"),
        (&mut b_rx, &mut b_seen, "tab B"),
    ] {
        wait_for(rx, seen, who, |seen| {
            has_method(seen, "session/request_permission")
        })
        .await;
    }
    let prompt = a_seen
        .iter()
        .find(|value| value["method"] == "session/request_permission")
        .unwrap()
        .clone();
    assert_eq!(prompt["params"]["sessionId"], SESSION_ID);
    assert_eq!(prompt["params"]["toolName"], "Bash");
    assert_eq!(prompt["params"]["options"][0]["kind"], "allow_once");
    assert_eq!(
        b_seen
            .iter()
            .find(|value| value["method"] == "session/request_permission")
            .unwrap()["id"],
        prompt["id"],
        "both tabs are shown one prompt, not one each"
    );

    // The terminal is on the same stream, and sees the same query.
    let mirror_query_id = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + SETUP_TIMEOUT;
        while Instant::now() < deadline {
            match mirror_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(SessionEvent::Permission { query, .. }) => return Some(query.query_id),
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        None
    })
    .await
    .unwrap();
    // The owner gives each prompt an id of its own, scoped to its endpoint;
    // the job record is where every client reads which one is outstanding.
    let recorded = owned
        .store
        .read_state(&owned.state.identity.job_id)
        .unwrap()
        .outcome
        .pending_permission
        .expect("the owner is parked on the prompt")
        .query_id;
    assert_eq!(
        mirror_query_id,
        Some(recorded),
        "the terminal sees the same permission the tabs do"
    );
    assert_eq!(
        prompt["id"],
        json!(format!("perm-{SESSION_ID}-{recorded}")),
        "and the tabs were shown that same prompt"
    );

    // Tab B answers first; tab A's answer arrives after and is dropped.
    owned.mux.on_client_message(
        b,
        &json!({
            "jsonrpc": "2.0", "id": prompt["id"],
            "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } }
        })
        .to_string(),
    );
    let answer = tokio::time::timeout(SETUP_TIMEOUT, response_rx)
        .await
        .expect("the owner is answered")
        .unwrap();
    match answer {
        PermissionAnswer::Selected { option_id, .. } => assert_eq!(option_id, "allow_once"),
        other => panic!("expected the tab's selection, got {other:?}"),
    }

    owned.mux.on_client_message(
        a,
        &json!({
            "jsonrpc": "2.0", "id": prompt["id"],
            "result": { "outcome": { "outcome": "cancelled" } }
        })
        .to_string(),
    );

    // I4: the tab that did not win is told the prompt is gone.
    for (rx, seen, who) in [
        (&mut a_rx, &mut a_seen, "tab A"),
        (&mut b_rx, &mut b_seen, "tab B"),
    ] {
        wait_for(rx, seen, who, |seen| {
            has_method(seen, "_serve/permission_resolved")
        })
        .await;
    }
    let resolved = a_seen
        .iter()
        .find(|value| value["method"] == "_serve/permission_resolved")
        .unwrap();
    assert_eq!(resolved["params"]["requestId"], prompt["id"]);
    assert_eq!(resolved["params"]["sessionId"], SESSION_ID);
    // And the owner is no longer parked on it.
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        if owned
            .store
            .read_state(&owned.state.identity.job_id)
            .unwrap()
            .outcome
            .pending_permission
            .is_none()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the owner still holds the prompt"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    owned.ipc.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_last_tab_of_a_session_releases_its_lease() {
    let mut owned = Owned::start();
    let (a, _a_rx) = owned.mux.connect();
    let (b, _b_rx) = owned.mux.connect();
    owned.open(a, 1).await;
    owned.open(b, 2).await;
    owned.wait_for_owner().await;

    let leases = |owned: &Owned| {
        owned
            .store
            .read_state(&owned.state.identity.job_id)
            .unwrap()
            .lease
            .client_leases
            .len()
    };
    let deadline = Instant::now() + SETUP_TIMEOUT;
    while leases(&owned) == 0 {
        assert!(Instant::now() < deadline, "no lease was ever taken");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Two tabs, one session, one client of the owner.
    assert_eq!(leases(&owned), 1);

    owned.mux.disconnect(a);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(owned.hosted.is_open(SESSION_ID), "tab B is still watching");
    assert_eq!(leases(&owned), 1);

    owned.mux.disconnect(b);
    let deadline = Instant::now() + SETUP_TIMEOUT;
    while leases(&owned) != 0 {
        assert!(
            Instant::now() < deadline,
            "the last tab left and the lease stayed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!owned.hosted.is_open(SESSION_ID));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_held_by_a_host_that_will_not_answer_is_refused_not_taken() {
    let dir = tempfile::tempdir().unwrap();
    let projects_root = dir.path().join("projects");
    let cwd_path = dir.path().join("work");
    std::fs::create_dir_all(&cwd_path).unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let transcript =
        rebon_session::ensure_session_file_path(&projects_root, &cwd, SESSION_ID).unwrap();
    std::fs::write(&transcript, b"").unwrap();

    // A lock with a descriptor whose endpoint nobody is listening on: alive
    // (the OS would have released the lock otherwise) but not answering.
    let _lock = rebon_session::try_acquire_session_active_lock(&projects_root, &cwd, SESSION_ID)
        .unwrap()
        .unwrap();
    let dead_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    rebon_session::write_session_owner(
        &projects_root,
        &cwd,
        SESSION_ID,
        &rebon_session::SessionOwnerDescriptor {
            version: rebon_session::SESSION_OWNER_VERSION,
            pid: std::process::id(),
            pid_identity: None,
            surface: rebon_session::SessionOwnerSurface::Worker,
            job_id: None,
            ipc_port: Some(dead_port),
            ipc_token: Some("nobody-home".into()),
            started_at_ms: rebon_session_host::now_ms(),
        },
    )
    .unwrap();

    let (to_server_tx, mut to_server) = tokio::sync::mpsc::unbounded_channel();
    let mux = AcpMux::new(to_server_tx);
    let init = to_server.try_recv().unwrap();
    mux.on_server_message(
        json!({ "jsonrpc": "2.0", "id": init["id"], "result": { "protocolVersion": 1 } }),
    );
    let hosted = HostedSessions::with_store(
        projects_root,
        cwd.clone(),
        BackgroundStore::new(dir.path().join("jobs")),
        runtime_fields(),
        Arc::new(Mutex::new(config_options())),
        None,
        Arc::new(rebon_acp::ServerState::new()),
        tokio::runtime::Handle::current(),
    );
    hosted.attach_mux(mux.clone());
    mux.attach_hosted(hosted.clone());

    let (a, mut a_rx) = mux.connect();
    mux.on_client_message(
        a,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/load",
            "params": { "sessionId": SESSION_ID, "cwd": cwd, "mcpServers": [] }
        })
        .to_string(),
    );

    let mut seen = Vec::new();
    wait_for(&mut a_rx, &mut seen, "the refusal", |seen| {
        seen.iter().any(|value| value["id"] == 1)
    })
    .await;
    let refusal = seen.iter().find(|value| value["id"] == 1).unwrap();
    assert_eq!(refusal["error"]["code"], -32000);
    assert_eq!(refusal["error"]["data"]["owner"]["surface"], "worker");
    assert_eq!(refusal["error"]["data"]["owner"]["reachable"], false);
    assert!(
        refusal["error"]["data"]["owner"].get("ipcToken").is_none(),
        "a refusal names the host, never its token"
    );
    assert!(!hosted.is_open(SESSION_ID), "nothing was opened");
    assert!(
        to_server.try_recv().is_err(),
        "and the server behind the mux was never asked to load it"
    );
}

/// The router claims exactly the methods that belong to a session's owner,
/// and leaves the rest to the ACP server behind the mux.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_router_claims_the_owner_s_methods_and_no_others() {
    use crate::serve::mux::HostedRouter;

    let mut owned = Owned::start();
    let (a, mut a_rx) = owned.mux.connect();
    let params = json!({ "sessionId": SESSION_ID });

    for method in ["session/list", "session/set_mode", "initialize"] {
        assert!(
            !owned.hosted.take_request(a, &json!(1), method, &params),
            "{method} is not the owner's"
        );
    }
    for method in ["session/new", "session/load"] {
        assert!(
            owned
                .hosted
                .take_request(a, &json!(1), method, &json!({ "cwd": owned.cwd })),
            "{method} starts or finds a host"
        );
    }
    // A session no tab has open here is refused rather than run in-process.
    assert!(owned.hosted.take_request(
        a,
        &json!(5),
        "session/prompt",
        &json!({ "sessionId": "sess-other" })
    ));
    let mut seen = Vec::new();
    wait_for(&mut a_rx, &mut seen, "the refusal", |seen| {
        seen.iter().any(|value| value["id"] == 5)
    })
    .await;
    assert!(
        seen.iter().find(|value| value["id"] == 5).unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Session not found")
    );

    assert!(!owned.hosted.take_notification("session/update", &params));
    assert!(!owned
        .hosted
        .take_notification("session/cancel", &json!({ "sessionId": "sess-other" })));
    // Drain whatever the `session/new` above set in motion.
    let _ = owned.to_server.try_recv();
}

/// Two tabs, one lease, and the lease outlives whichever one leaves first.
///
/// RFC-0004 §16.9: one lease per session, given up when the *last* tab leaves.
/// A measurement on a real `rebon serve` had it released when the first one
/// did, with the second still watching.
///
/// `the_last_tab_of_a_session_releases_its_lease` covers the sequential case
/// and always passed: a tab that opens after the session is on the map is
/// registered on the way in. The window is the other one. `session/load`
/// checks whether the session is open *before* asking the ACP server for its
/// metadata and registers the client *after* the answer, so a tab that arrives
/// while another is still opening sees nothing and used to be added to
/// nothing.
///
/// Answering in the other order is not a contrivance -- it is the ordinary
/// case with a tab that *creates* the session, whose registration waits on a
/// worker starting, which takes far longer than a second tab's metadata round
/// trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lease_outlives_the_first_tab_to_leave() {
    let mut owned = Owned::start();
    let (a, _a_rx) = owned.mux.connect();
    let (b, _b_rx) = owned.mux.connect();

    // Both loads in flight before either is answered.
    owned.send_load(a, 1);
    let first = owned.next_server_request().await;
    owned.send_load(b, 2);
    let second = owned.next_server_request().await;

    // Answered in the other order, which is what puts one of them in the gap.
    owned.answer_load(&second);
    owned.answer_load(&first);

    owned.wait_for_owner().await;
    owned.wait_for_lease().await;

    // The tab answered second goes away. The other is still watching, so the
    // session and its lease must survive.
    owned.mux.disconnect(b);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        owned.hosted.is_open(SESSION_ID),
        "the session was closed while a tab was still watching it"
    );
    assert_eq!(
        owned.serve_leases(),
        1,
        "the lease was given up while a tab was still watching"
    );

    // And when the last one leaves, it is released.
    owned.mux.disconnect(a);
    let deadline = Instant::now() + SETUP_TIMEOUT;
    while owned.serve_leases() != 0 {
        assert!(
            Instant::now() < deadline,
            "the last tab left and the lease stayed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!owned.hosted.is_open(SESSION_ID));
}

/// A setting one tab changed reaches the others, including the ones the owner
/// has no say in.
///
/// An owner-held value gets there on the owner's next status, which is the I4
/// path and already had a test. A local one -- a `config.json` setting the
/// next worker reads -- has no such echo: it is written here and nothing else
/// ever mentions it, so a second tab went on showing the old value until it
/// was reloaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_config_option_one_tab_changed_reaches_the_others() {
    let mut owned = Owned::start();
    let (a, mut a_rx) = owned.mux.connect();
    let (_b, mut b_rx) = owned.mux.connect();
    owned.open(a, 1).await;
    owned.wait_for_owner().await;

    // Opening produced updates of its own; this test is about what comes
    // after, so what has already arrived is read and set aside.
    let mut b_seen = drained(&mut b_rx);
    b_seen.clear();

    owned.mux.on_client_message(
        a,
        &json!({
            "jsonrpc": "2.0", "id": 7, "method": "session/set_config_option",
            // Not one of the owner's: `owner_config_option` returns `None` for
            // it, so this is a `config.json` setting applied here.
            "params": { "sessionId": SESSION_ID, "configId": "sub_agents", "value": "off" }
        })
        .to_string(),
    );

    let mut a_seen = Vec::new();
    wait_for(
        &mut a_rx,
        &mut a_seen,
        "the answer to the tab that asked",
        |seen| seen.iter().any(|value| value["id"] == json!(7)),
    )
    .await;

    wait_for(
        &mut b_rx,
        &mut b_seen,
        "the other tab being told what the controls read now",
        |seen| {
            seen.iter().any(|value| {
                value["method"] == "session/update"
                    && value["params"]["update"]["sessionUpdate"] == "config_option_update"
                    && value["params"]["update"]["configOptions"]
                        .as_array()
                        .is_some_and(|options| {
                            options.iter().any(|option| {
                                option["id"] == "sub_agents" && option["currentValue"] == "off"
                            })
                        })
            })
        },
    )
    .await;
}

/// A steer is answered with the outcome the owner reported, not with the
/// likelier of the two.
///
/// `_session/steering`'s declared result says whether the message landed in
/// the running turn or started a fresh one, and a tab needs the difference:
/// only one of them has a turn already streaming. serve used to build this
/// request by hand and read the field out of an untyped payload with
/// `"injected"` as the default, which reported a new turn as an injection
/// whenever the answer was not the shape it expected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_is_answered_with_the_outcome_the_owner_reported() {
    let mut owned = Owned::start();
    let (a, mut a_rx) = owned.mux.connect();
    owned.open(a, 1).await;
    owned.wait_for_owner().await;
    let _ = drained(&mut a_rx);

    owned.mux.on_client_message(
        a,
        &json!({
            "jsonrpc": "2.0", "id": 9, "method": "_session/steering",
            "params": {
                "sessionId": SESSION_ID,
                "prompt": [{ "type": "text", "text": "also fix the tests" }],
            }
        })
        .to_string(),
    );

    let mut seen = Vec::new();
    wait_for(&mut a_rx, &mut seen, "the steer response", |seen| {
        seen.iter().any(|value| value["id"] == json!(9))
    })
    .await;
    let answer = seen.iter().find(|value| value["id"] == json!(9)).unwrap();
    assert!(
        answer.get("error").is_none(),
        "the steer was refused: {answer:?}"
    );
    // This owner is not running a turn, so the message cannot have been
    // injected into one. That is the outcome the owner computes and the only
    // one this session can produce; the default the old reader used was the
    // other one.
    assert_eq!(
        answer["result"]["outcome"], "startedNewTurn",
        "the answer carries what the owner said"
    );
}
