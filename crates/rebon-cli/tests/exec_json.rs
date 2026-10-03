use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

struct ExecFixture {
    root: tempfile::TempDir,
    home: PathBuf,
    cwd: PathBuf,
    _config_home: rebon_tool::tasks::test_support::TestConfigHome,
}

impl ExecFixture {
    fn new() -> Self {
        let config_home = rebon_tool::tasks::test_support::TestConfigHome::new("exec-json");
        let root = tempfile::tempdir().expect("test root");
        let home = root.path().join("home");
        let cwd = root.path().join("cwd");
        std::fs::create_dir(&home).expect("config home");
        std::fs::create_dir(&cwd).expect("cwd");
        std::fs::write(home.join("config.json"), "{}").expect("config");
        std::fs::write(home.join("settings.json"), "{}").expect("settings");
        Self {
            root,
            home,
            cwd,
            _config_home: config_home,
        }
    }

    fn provider(&self, address: std::net::SocketAddr) {
        let trust_key = rebon_session::cwd_identity(&self.cwd.to_string_lossy());
        std::fs::write(
            self.home.join("config.json"),
            json!({
                "activeCustomProvider": "mock",
                "customProviders": [{
                    "name": "mock", "format": "openai",
                    "baseUrl": format!("http://{address}/v1"),
                    "apiKey": "exec-test-not-a-secret", "model": "mock-model",
                    "models": ["mock-model"]
                }],
                "projects": { trust_key: { "hasTrustDialogAccepted": true } }
            })
            .to_string(),
        )
        .expect("mock config");
    }

    async fn run(&self, args: &[&str]) -> Output {
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_rebon"));
        let mut command = Command::new(std::fs::canonicalize(&binary).expect("exec binary path"));
        command.env_clear();
        for name in ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .args(args)
            .current_dir(&self.cwd)
            .env("REBON_CONFIG_DIR", &self.home)
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("NO_PROXY", "127.0.0.1,localhost")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        tokio::time::timeout(Duration::from_secs(120), command.output())
            .await
            .expect("exec must finish and shut down the plugin plane")
            .expect("run rebon exec")
    }
}

fn events(output: &Output) -> Vec<Value> {
    let stdout = std::str::from_utf8(&output.stdout).expect("UTF-8 stdout");
    stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stdout must contain JSONL only: {error}: {stdout}"))
        })
        .collect()
}

fn assert_one_error(output: &Output) -> Vec<Value> {
    assert!(!output.status.success(), "failure must exit nonzero");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("exec-test-not-a-secret"));
    let events = events(output);
    let errors: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "error")
        .collect();
    assert_eq!(errors.len(), 1, "{events:?}");
    let error = errors[0];
    assert_eq!(error.as_object().expect("error object").len(), 2);
    assert!(!error["message"]
        .as_str()
        .expect("error message")
        .trim()
        .is_empty());
    assert!(!error["message"]
        .as_str()
        .unwrap()
        .contains("stack backtrace"));
    events
}
fn event_types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["type"].as_str().expect("event type"))
        .collect()
}

#[derive(Clone, Copy)]
enum Reply {
    Success,
    Failure,
    Pending,
}

struct Provider {
    address: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local provider");
        let address = listener.local_addr().expect("provider address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("provider connection");
                let mut buffer = Vec::new();
                let mut chunk = [0; 4096];
                let header_end = loop {
                    let read = stream.read(&mut chunk).await.expect("request headers");
                    assert_ne!(read, 0);
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        break at + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .expect("request content-length")
                    .trim()
                    .parse::<usize>()
                    .expect("body length");
                while buffer.len() < header_end + length {
                    let read = stream.read(&mut chunk).await.expect("request body");
                    assert_ne!(read, 0);
                    buffer.extend_from_slice(&chunk[..read]);
                }
                let body: Value =
                    serde_json::from_slice(&buffer[header_end..]).expect("request JSON");
                requests
                    .lock()
                    .expect("requests poisoned")
                    .push(body.clone());
                let (status, content_type, response) = match reply {
                    Reply::Pending => std::future::pending().await,
                    Reply::Failure => ("400 Bad Request", "application/json",
                        json!({"error": {"message": "mock turn failed", "type": "invalid_request_error"}}).to_string()),
                    Reply::Success if body["stream"] == true => {
                        let chunks = [
                            json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock-model",
                                "choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"}, "finish_reason": null}]}),
                            json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock-model",
                                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                            json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock-model", "choices": [],
                                "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}}),
                        ];
                        let mut sse: String = chunks.iter().map(|chunk| format!("data: {chunk}\n\n")).collect();
                        sse.push_str("data: [DONE]\n\n");
                        ("200 OK", "text/event-stream", sse)
                    }
                    Reply::Success => ("200 OK", "application/json",
                        json!({"id": "c1", "object": "chat.completion", "model": "mock-model",
                            "choices": [{"index": 0, "message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}}).to_string()),
                };
                let response = format!("HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}", response.len());
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("provider response");
                stream.shutdown().await.expect("provider shutdown");
            }
        });
        Self {
            address,
            seen,
            task,
        }
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_initialization_failure_emits_one_json_error() {
    let fixture = ExecFixture::new();
    let plugin_dir = fixture.home.join("plugins");
    std::fs::create_dir(&plugin_dir).expect("plugins");
    std::fs::write(plugin_dir.join("installed.json"), "not JSON").expect("invalid plugin state");
    let output = fixture.run(&["exec", "--json", "hello"]).await;
    let events = assert_one_error(&output);
    assert_eq!(events.len(), 1);
    assert!(events[0]["message"]
        .as_str()
        .unwrap()
        .contains("failed to parse plugin state"));
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_failure_does_not_repeat_the_observer_error() {
    let fixture = ExecFixture::new();
    let provider = Provider::start(Reply::Failure).await;
    fixture.provider(provider.address);
    let output = fixture.run(&["exec", "--json", "hello"]).await;
    let events = assert_one_error(&output);
    assert_eq!(event_types(&events), ["session", "error"]);
    assert!(events[1]["message"]
        .as_str()
        .unwrap()
        .contains("mock turn failed"));
    assert!(!provider.seen.lock().expect("requests poisoned").is_empty());
    let output = fixture.run(&["exec", "hello"]).await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn successful_turn_and_resume_keep_the_json_contract() {
    let fixture = ExecFixture::new();
    let provider = Provider::start(Reply::Success).await;
    fixture.provider(provider.address);
    let output = fixture.run(&["exec", "--json", "first prompt"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first = events(&output);
    assert_eq!(
        event_types(&first),
        ["session", "message", "turn.completed", "result"]
    );
    assert_eq!(first[1]["text"], "done");
    assert_eq!(first[3]["stopReason"], "end_turn");
    assert_eq!(first[3]["usage"]["input_tokens"], 10);
    assert_eq!(first[3]["usage"]["output_tokens"], 2);
    let session_id = first[0]["sessionId"].as_str().expect("session id");
    let output = fixture
        .run(&["exec", "--json", "--resume", session_id, "second prompt"])
        .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let resumed = events(&output);
    assert_eq!(event_types(&resumed), event_types(&first));
    assert_eq!(resumed[0], first[0]);
    assert_eq!(resumed[3]["sessionId"], session_id);
    let requests = provider.seen.lock().expect("requests poisoned");
    let replay = requests.last().expect("resume request")["messages"].to_string();
    assert!(replay.contains("first prompt"));
    assert!(replay.contains("done"));
    assert!(replay.contains("second prompt"));
    drop(requests);
    let output = fixture.run(&["exec", "text prompt"]).await;
    assert!(output.status.success());
    assert_eq!(std::str::from_utf8(&output.stdout).unwrap().trim(), "done");
}

#[tokio::test(flavor = "multi_thread")]
async fn max_duration_keeps_cancelled_event_and_successful_result() {
    let fixture = ExecFixture::new();
    let provider = Provider::start(Reply::Pending).await;
    fixture.provider(provider.address);
    let output = fixture
        .run(&["exec", "--json", "--max-duration", "1", "hello"])
        .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = events(&output);
    assert_eq!(event_types(&events), ["session", "error", "result"]);
    assert_eq!(events[1], json!({"type": "error", "message": "cancelled"}));
    assert_eq!(
        events[2],
        json!({"type": "result", "sessionId": events[0]["sessionId"],
        "stopReason": "max_duration", "usage": null})
    );
    assert!(!provider.seen.lock().expect("requests poisoned").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn slash_command_answer_keeps_message_and_result_without_a_turn() {
    let fixture = ExecFixture::new();
    let provider = Provider::start(Reply::Success).await;
    fixture.provider(provider.address);
    let output = fixture.run(&["exec", "--json", "/runtime"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = events(&output);
    assert_eq!(event_types(&events), ["session", "message", "result"]);
    assert!(!events[1]["text"].as_str().expect("answer").is_empty());
    assert_eq!(
        events[2],
        json!({"type": "result", "sessionId": events[0]["sessionId"],
        "stopReason": "command", "usage": null})
    );
    assert!(provider.seen.lock().expect("requests poisoned").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn blank_prompt_emits_one_json_error_and_text_mode_stays_text() {
    let fixture = ExecFixture::new();
    for prompt in ["", " \t\n "] {
        let output = fixture.run(&["exec", "--json", prompt]).await;
        let events = assert_one_error(&output);
        assert_eq!(
            events,
            [json!({"type": "error", "message": "rebon exec: empty prompt"})]
        );
    }
    let output = fixture.run(&["exec", " \t "]).await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("empty prompt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_configuration_failure_emits_one_json_error_before_session() {
    let fixture = ExecFixture::new();
    let output = fixture
        .run(&[
            "--provider",
            "missing-test-provider",
            "exec",
            "--json",
            "hello",
        ])
        .await;
    let events = assert_one_error(&output);
    assert_eq!(events.len(), 1);
    assert!(events[0]["message"]
        .as_str()
        .unwrap()
        .contains("failed to build headless session"));
    let output = fixture
        .run(&["--provider", "missing-test-provider", "exec", "hello"])
        .await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}
