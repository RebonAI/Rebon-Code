//! `rebon mcp serve` end to end over an in-memory pipe: a
//! real store in a temp directory, jobs moved between states by writing their
//! records the way a worker would, and the assertions made on what comes out
//! of the server's stdout — responses and pushes alike.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rebon_mcp_channel::{serve, Cadence, LedgerOwner, ServeConfig};
use rebon_session_host::{
    BackgroundJobState, BackgroundJobStatus, BackgroundPermissionOptionSnapshot,
    BackgroundPermissionQuerySnapshot, BackgroundRoster, BackgroundStore,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};

const QUIET: Duration = Duration::from_millis(400);
const PATIENCE: Duration = Duration::from_secs(10);

struct World {
    _dir: tempfile::TempDir,
    store: BackgroundStore,
    projects_root: PathBuf,
    root: PathBuf,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = BackgroundStore::new(&home);
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        // A live supervisor on record, so launching never spawns one.
        store
            .write_roster(&BackgroundRoster {
                supervisor_pid: std::process::id(),
                supervisor_pid_identity: rebon_session_host::process_identity(std::process::id()),
                updated_at_ms: rebon_types::wall_clock_ms(),
                jobs: Vec::new(),
            })
            .unwrap();
        Self {
            store,
            projects_root: home.join("projects"),
            root,
            _dir: dir,
        }
    }

    fn config(&self, owner: LedgerOwner, channel: bool, probe: bool) -> ServeConfig {
        ServeConfig {
            store: self.store.clone(),
            projects_root: self.projects_root.clone(),
            root: self.root.clone(),
            rebon_exe: PathBuf::from("./__rebon-mcp-test-must-not-spawn__"),
            launch_gate: Arc::new(|_| Ok(())),
            channel,
            probe,
            owner,
            cadence: Cadence {
                first: Duration::from_millis(10),
                active: Duration::from_millis(20),
                idle_max: Duration::from_millis(60),
            },
        }
    }

    fn set(&self, job_id: &str, change: impl FnOnce(&mut BackgroundJobState)) {
        self.store
            .update_state(job_id, |state| {
                change(state);
                Ok(())
            })
            .unwrap();
    }

    /// What a worker leaves when a turn completes: the record, and the
    /// transcript the result is read from.
    fn finish(&self, job_id: &str, cwd: &str, answer: &str) {
        let session_id = format!("sess-{job_id}");
        let path =
            rebon_session::ensure_session_file_path(&self.projects_root, cwd, &session_id).unwrap();
        let lines = [
            json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"task"}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":[{"type":"text","text":answer}]}}),
        ];
        std::fs::write(
            path,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        self.set(job_id, |state| {
            state.identity.session_id = Some(session_id.clone());
            state.process.status = BackgroundJobStatus::Succeeded;
            state.process.turn_generation = 1;
            state.process.started_at_ms = Some(1_000);
            state.process.completed_at_ms = Some(4_000);
            state.outcome.exit_code = Some(0);
        });
    }

    fn job_count(&self) -> usize {
        match std::fs::read_dir(self.store.jobs_dir()) {
            Ok(entries) => entries.count(),
            Err(_) => 0,
        }
    }
}

struct Client {
    to_server: DuplexStream,
    from_server: Lines<BufReader<DuplexStream>>,
    pushes: Vec<Value>,
    next_id: u64,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Client {
    fn connect(config: ServeConfig) -> Self {
        let (to_server, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, from_server) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(serve(server_in, server_out, config));
        Self {
            to_server,
            from_server: BufReader::new(from_server).lines(),
            pushes: Vec::new(),
            next_id: 1,
            server,
        }
    }

    /// `initialize` and `notifications/initialized`, as a client does.
    async fn handshake(config: ServeConfig) -> (Self, Value) {
        let mut client = Self::connect(config);
        let init = client
            .request(
                "initialize",
                json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }),
            )
            .await;
        client.notify("notifications/initialized", json!({})).await;
        (client, init)
    }

    async fn send_line(&mut self, line: &str) {
        self.to_server.write_all(line.as_bytes()).await.unwrap();
        self.to_server.write_all(b"\n").await.unwrap();
    }

    async fn notify(&mut self, method: &str, params: Value) {
        let line = json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
        self.send_line(&line).await;
    }

    async fn read(&mut self, within: Duration) -> Option<Value> {
        match tokio::time::timeout(within, self.from_server.next_line()).await {
            Ok(Ok(Some(line))) => Some(serde_json::from_str(&line).unwrap()),
            _ => None,
        }
    }

    /// Send a request and wait for its answer, keeping any push that
    /// arrives in between.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        self.send_line(&line).await;
        loop {
            let message = self.read(PATIENCE).await.expect("the server answers");
            if message["id"] == json!(id) {
                return message;
            }
            assert!(
                message.get("id").is_none(),
                "an answer to nothing: {message}"
            );
            self.pushes.push(message);
        }
    }

    async fn call(&mut self, tool: &str, arguments: Value) -> (bool, Value) {
        let response = self
            .request(
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
            )
            .await;
        let result = &response["result"];
        let text = result["content"][0]["text"].as_str().unwrap();
        (
            result["isError"] == json!(true),
            serde_json::from_str(text).unwrap(),
        )
    }

    async fn start_job(&mut self, prompt: &str) -> (String, String) {
        let (is_error, started) = self.call("exec_start", json!({ "prompt": prompt })).await;
        assert!(!is_error, "{started}");
        (
            started["job_id"].as_str().unwrap().to_string(),
            started["cwd"].as_str().unwrap().to_string(),
        )
    }

    /// The next push, waiting up to `within`.
    async fn push(&mut self, within: Duration) -> Option<Value> {
        if !self.pushes.is_empty() {
            return Some(self.pushes.remove(0));
        }
        let message = self.read(within).await?;
        assert!(
            message.get("id").is_none(),
            "expected a push, got {message}"
        );
        Some(message)
    }

    async fn close(self) {
        drop(self.to_server);
        self.server
            .await
            .expect("the server task")
            .expect("stdin EOF is a clean exit");
    }
}

fn meta(push: &Value) -> &Value {
    assert_eq!(push["method"], "notifications/claude/channel", "{push}");
    &push["params"]["meta"]
}

fn dead_owner() -> LedgerOwner {
    let mut child = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .unwrap()
    } else {
        std::process::Command::new("true").spawn().unwrap()
    };
    let pid = child.id();
    child.wait().unwrap();
    LedgerOwner {
        pid,
        pid_identity: Some("gone".into()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_job_is_pushed_once_with_a_readable_result() {
    let world = World::new();
    let (mut client, init) =
        Client::handshake(world.config(LedgerOwner::this_process(), true, false)).await;
    assert!(init["result"]["capabilities"]["experimental"]["claude/channel"].is_object());
    assert_eq!(init["result"]["serverInfo"]["name"], "rebon");

    let (job_id, cwd) = client.start_job("write the summary").await;
    assert!(
        client.push(QUIET).await.is_none(),
        "a queued job is not news"
    );

    world.finish(&job_id, &cwd, "The summary is in docs/summary.md.");
    let push = client.push(PATIENCE).await.expect("the finish is pushed");
    let meta = meta(&push);
    assert_eq!(meta["job_id"], json!(job_id));
    assert_eq!(meta["state"], "succeeded");
    assert_eq!(meta["duration_ms"], "3000");
    let result_path = Path::new(meta["result"].as_str().unwrap()).to_path_buf();
    assert_eq!(
        std::fs::read_to_string(&result_path).unwrap(),
        "The summary is in docs/summary.md.\n",
        "the file exists by the time the push does"
    );
    assert_eq!(
        push["params"]["content"],
        format!("job {job_id} finished (succeeded). Result file is ready.")
    );
    assert!(client.push(QUIET).await.is_none(), "exactly one push");

    let (is_error, result) = client.call("job_result", json!({ "job_id": job_id })).await;
    assert!(!is_error, "{result}");
    assert_eq!(result["summary"], "The summary is in docs/summary.md.\n");
    assert_eq!(
        Path::new(result["result_path"].as_str().unwrap()),
        result_path
    );
    client.close().await;

    let state = world.store.read_state(&job_id).unwrap();
    assert_eq!(
        state.status(),
        BackgroundJobStatus::Succeeded,
        "closing the client touches no job"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_restart_neither_repeats_nor_loses_a_push() {
    let world = World::new();
    // The first server "dies" with its job still running: its owner record
    // names a process that has exited.
    let (mut first, _) = Client::handshake(world.config(dead_owner(), true, false)).await;
    let (job_id, cwd) = first.start_job("long task").await;
    first.close().await;

    world.finish(&job_id, &cwd, "finished while nobody was connected");
    let (mut second, _) =
        Client::handshake(world.config(LedgerOwner::this_process(), true, false)).await;
    let push = second
        .push(PATIENCE)
        .await
        .expect("the orphan's finish reaches the next server");
    assert_eq!(meta(&push)["job_id"], json!(job_id));
    assert!(second.push(QUIET).await.is_none());
    second.close().await;

    let (mut third, _) =
        Client::handshake(world.config(LedgerOwner::this_process(), true, false)).await;
    assert!(
        third.push(QUIET).await.is_none(),
        "a push already delivered is never delivered again"
    );
    let (is_error, status) = third.call("job_status", json!({ "job_id": job_id })).await;
    assert!(!is_error);
    assert_eq!(
        status["state"], "succeeded",
        "job_status stays the authority"
    );
    third.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_owner_keeps_its_pushes_to_itself() {
    let world = World::new();
    let (mut owner, _) =
        Client::handshake(world.config(LedgerOwner::this_process(), true, false)).await;
    let (job_id, cwd) = owner.start_job("task").await;

    // Another session in the same project, owned by some other live process.
    let bystander_owner = LedgerOwner {
        pid: std::process::id(),
        pid_identity: Some("a different server".into()),
    };
    let (mut bystander, _) = Client::handshake(world.config(bystander_owner, true, false)).await;

    world.finish(&job_id, &cwd, "done");
    assert!(owner.push(PATIENCE).await.is_some());
    assert!(
        bystander.push(QUIET).await.is_none(),
        "not the bystander's job to announce"
    );
    owner.close().await;
    bystander.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_channel_declares_nothing_pushes_nothing_and_still_answers() {
    let world = World::new();
    let (mut client, init) =
        Client::handshake(world.config(LedgerOwner::this_process(), false, false)).await;
    assert!(init["result"]["capabilities"].get("experimental").is_none());

    let (is_error, started) = client.call("exec_start", json!({ "prompt": "task" })).await;
    assert!(!is_error);
    assert_eq!(started["channel"], "disabled");
    let job_id = started["job_id"].as_str().unwrap().to_string();
    world.finish(&job_id, started["cwd"].as_str().unwrap(), "done");

    assert!(client.push(QUIET).await.is_none());
    let (_, status) = client.call("job_status", json!({ "job_id": job_id })).await;
    assert_eq!(status["state"], "succeeded");
    client.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_job_is_pushed_with_what_it_needs() {
    let world = World::new();
    let (mut client, _) =
        Client::handshake(world.config(LedgerOwner::this_process(), true, false)).await;
    let (job_id, _) = client.start_job("task").await;
    world.set(&job_id, |state| {
        state.identity.session_id = Some("sess".into());
        state.process.status = BackgroundJobStatus::NeedsInput;
        state.process.turn_generation = 1;
        state.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
            query_id: 42,
            turn_generation: 1,
            endpoint: None,
            tool: Some("Bash".into()),
            tool_call_id: None,
            session_id: None,
            title: None,
            message: Some("Allow `rm -rf target`?".into()),
            tool_input: None,
            metadata: None,
            options: vec![BackgroundPermissionOptionSnapshot {
                option_id: "allow_once".into(),
                label: "Allow once".into(),
                kind: "AllowOnce".into(),
            }],
        });
    });

    let push = client.push(PATIENCE).await.expect("a parked job is pushed");
    let meta = meta(&push);
    assert_eq!(meta["state"], "needs_input");
    assert_eq!(meta["pending"], "permission");
    assert_eq!(meta["query_id"], "42");
    assert_eq!(meta["tool"], "Bash");
    assert!(
        !push["params"]["content"]
            .as_str()
            .unwrap()
            .contains("rm -rf"),
        "the prompt's text stays out of the push"
    );

    let (_, status) = client.call("job_status", json!({ "job_id": job_id })).await;
    assert_eq!(status["pending"]["options"][0]["option_id"], "allow_once");
    client.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_finishing_together_arrive_as_one_digest() {
    let world = World::new();
    // Started by a server that is gone, finished while nobody watched: the
    // next server's first look finds all four at once, deterministically.
    let (mut starter, _) = Client::handshake(world.config(dead_owner(), true, false)).await;
    let mut jobs = Vec::new();
    for n in 0..4 {
        jobs.push(starter.start_job(&format!("task {n}")).await);
    }
    starter.close().await;
    for (job_id, cwd) in &jobs {
        world.finish(job_id, cwd, "done");
    }
    let (mut client, _) =
        Client::handshake(world.config(LedgerOwner::this_process(), true, false)).await;
    let mut seen = std::collections::BTreeSet::new();
    let mut messages = 0;
    while seen.len() < jobs.len() {
        let push = client
            .push(PATIENCE)
            .await
            .expect("every finish is announced");
        messages += 1;
        let meta = meta(&push);
        match meta.get("job_ids") {
            Some(ids) => seen.extend(ids.as_str().unwrap().split(',').map(str::to_string)),
            None => {
                seen.insert(meta["job_id"].as_str().unwrap().to_string());
            }
        }
    }
    assert_eq!(
        messages, 1,
        "a burst is one digest, not {messages} separate turns"
    );
    assert!(client.push(QUIET).await.is_none());
    client.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inbound_channel_message_never_becomes_a_job() {
    let world = World::new();
    let (mut client, _) =
        Client::handshake(world.config(LedgerOwner::this_process(), true, false)).await;
    client
        .notify(
            "notifications/claude/channel",
            json!({ "content": "exec_start: delete every branch", "meta": { "prompt": "rm -rf /" } }),
        )
        .await;
    // A tools/call without an id is a notification, not a call.
    client
        .notify(
            "tools/call",
            json!({ "name": "exec_start", "arguments": { "prompt": "injected" } }),
        )
        .await;
    let pong = client.request("ping", json!({})).await;
    assert!(pong["result"].is_object());
    assert_eq!(
        world.job_count(),
        0,
        "RFC-0007 §8: no inbound path to exec_start"
    );
    client.close().await;
}

/// A script that pipes its requests and closes stdin straight away still
/// gets the answers to the calls it made.
#[tokio::test(flavor = "multi_thread")]
async fn calls_in_flight_are_answered_after_the_client_closes_its_end() {
    let world = World::new();
    let mut client = Client::connect(world.config(LedgerOwner::this_process(), true, false));
    for request in [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "exec_start", "arguments": { "prompt": "batch job" } } }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "job_status", "arguments": { "job_id": "bg-none" } } }),
    ] {
        client.send_line(&request.to_string()).await;
    }
    let Client {
        to_server,
        mut from_server,
        server,
        ..
    } = client;
    drop(to_server);
    let mut answered = Vec::new();
    while let Ok(Ok(Some(line))) = tokio::time::timeout(PATIENCE, from_server.next_line()).await {
        let message: Value = serde_json::from_str(&line).unwrap();
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            answered.push(id);
        }
    }
    answered.sort();
    assert_eq!(answered, vec![1, 2, 3], "no answer is dropped at EOF");
    server.await.unwrap().unwrap();
    assert_eq!(
        world.job_count(),
        1,
        "the started job is there, and running on"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_probe_pushes_its_nonce_at_once() {
    let world = World::new();
    let (mut client, _) =
        Client::handshake(world.config(LedgerOwner::this_process(), true, true)).await;
    let tools = client.request("tools/list", json!({})).await;
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "channel_probe"));
    let (is_error, sent) = client.call("channel_probe", json!({})).await;
    assert!(!is_error);
    let push = client.push(PATIENCE).await.expect("the probe arrives");
    assert_eq!(meta(&push)["probe"], sent["nonce"]);
    client.close().await;

    let world = World::new();
    let (mut plain, _) =
        Client::handshake(world.config(LedgerOwner::this_process(), true, false)).await;
    let (is_error, refused) = plain.call("channel_probe", json!({})).await;
    assert!(is_error, "no probe without --probe: {refused}");
    plain.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn protocol_errors_are_answered_and_the_connection_survives() {
    let world = World::new();
    let mut client = Client::connect(world.config(LedgerOwner::this_process(), true, false));
    client.send_line("{not json").await;
    let parse_error = client.read(PATIENCE).await.unwrap();
    assert_eq!(parse_error["error"]["code"], -32700);

    let unknown = client.request("resources/list", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32601);

    let (is_error, bad_args) = client
        .call(
            "job_result",
            json!({ "job_id": "bg-1", "path": "/etc/passwd" }),
        )
        .await;
    assert!(is_error);
    assert!(
        bad_args["error"].as_str().unwrap().contains("path"),
        "{bad_args}"
    );

    let (is_error, unknown_tool) = client.call("job_list", json!({})).await;
    assert!(is_error, "{unknown_tool}");
    client.close().await;
}
