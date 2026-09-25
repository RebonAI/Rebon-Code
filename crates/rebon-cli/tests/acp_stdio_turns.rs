//! `rebon --acp` over real stdio finishes a turn, and then another.
//!
//! The built binary, driven the way an editor drives it: JSON-RPC on its
//! stdin and stdout, a provider behind a local HTTP server. Nothing short of
//! the real process reaches the defect this pins. The in-process ACP tests
//! hand the server an in-memory pipe, and the one that hung was the process's
//! own stdin: on Windows a child that inherits that pipe while the server's
//! read is parked on it — `git`, which the system prompt runs every turn —
//! waits for the read to finish, so the first prompt waited for a client that
//! was itself waiting for the answer. Every wait below has a deadline, so that
//! regression fails here instead of hanging the run.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{ChildStdin, ChildStdout, Command};

/// Generous: a debug binary's first turn boots the kernel and the prompt.
const STEP_DEADLINE: Duration = Duration::from_secs(120);

/// One request the provider saw: how many tools it offered.
#[derive(Debug, Clone)]
struct SeenRequest {
    tools: usize,
}

/// An OpenAI-compatible provider that answers every request with "done":
/// streamed when asked to stream, one JSON body otherwise.
async fn serve_provider(listener: TcpListener, seen: Arc<Mutex<Vec<SeenRequest>>>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let seen = seen.clone();
        tokio::spawn(async move {
            let _ = answer(stream, seen).await;
        });
    }
}

async fn answer(mut stream: TcpStream, seen: Arc<Mutex<Vec<SeenRequest>>>) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body: Value = serde_json::from_slice(&buffer[header_end..]).unwrap_or(Value::Null);
    seen.lock()
        .expect("seen requests poisoned")
        .push(SeenRequest {
            tools: body["tools"].as_array().map_or(0, Vec::len),
        });
    let model = body["model"].as_str().unwrap_or("mock-model").to_string();
    let response = if body["stream"].as_bool() == Some(true) {
        let events = [
            json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                   "choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"}, "finish_reason": null}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
                   "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "model": model, "choices": [],
                   "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}}),
        ];
        let mut sse = String::new();
        for event in events {
            sse.push_str(&format!("data: {event}\n\n"));
        }
        sse.push_str("data: [DONE]\n\n");
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{sse}"
        )
    } else {
        let json = json!({"id": "c1", "object": "chat.completion", "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}})
        .to_string();
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
            json.len()
        )
    };
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// A config home with one provider pointing at `port`, and `cwd` trusted.
fn write_config_home(home: &Path, port: u16, cwd: &Path) {
    let trust_key = rebon_session::cwd_identity(&cwd.to_string_lossy());
    std::fs::write(
        home.join("config.json"),
        json!({
            "activeCustomProvider": "mock",
            "customProviders": [{
                "name": "mock",
                "format": "openai",
                "baseUrl": format!("http://127.0.0.1:{port}/v1"),
                "apiKey": "not-a-real-key",
                "model": "mock-model",
                "models": ["mock-model"]
            }],
            "projects": { trust_key: { "hasTrustDialogAccepted": true } }
        })
        .to_string(),
    )
    .expect("config written");
    std::fs::write(home.join("settings.json"), "{}").expect("settings written");
}

/// The binary cargo built for this test, by its resolved path: a target
/// directory reached through a junction is a different path to a scanner that
/// quarantines freshly built executables, and resolving it costs nothing.
fn rebon_binary() -> PathBuf {
    let built = PathBuf::from(env!("CARGO_BIN_EXE_rebon"));
    std::fs::canonicalize(&built).unwrap_or(built)
}

struct Client {
    stdin: ChildStdin,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
}

impl Client {
    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .expect("write to the server");
        self.stdin.flush().await.expect("flush to the server");
    }

    /// The response to request `id`. Notifications are skipped, and a
    /// request from the server is refused so a turn can never wait on us.
    async fn response(&mut self, id: u64, what: &str) -> Value {
        let wait = async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .expect("read from the server")
                    .unwrap_or_else(|| panic!("the server closed stdout before answering {what}"));
                let message: Value = serde_json::from_str(&line).expect("a JSON-RPC line");
                if message.get("method").is_some() {
                    if let Some(request_id) = message.get("id") {
                        self.send(json!({"jsonrpc": "2.0", "id": request_id,
                            "result": {"outcome": {"outcome": "cancelled"}}}))
                            .await;
                    }
                    continue;
                }
                if message["id"] == json!(id) {
                    return message;
                }
            }
        };
        tokio::time::timeout(STEP_DEADLINE, wait)
            .await
            .unwrap_or_else(|_| panic!("no answer to {what} within {STEP_DEADLINE:?}"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_acp_session_over_stdio_finishes_two_turns() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = tokio::spawn(serve_provider(listener, seen.clone()));

    let home = tempfile::tempdir().expect("config home");
    let cwd = tempfile::tempdir().expect("cwd");
    write_config_home(home.path(), port, cwd.path());

    let mut child = Command::new(rebon_binary())
        .arg("--acp")
        .current_dir(cwd.path())
        .env("REBON_CONFIG_DIR", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn rebon --acp");
    let mut client = Client {
        stdin: child.stdin.take().expect("stdin"),
        lines: BufReader::new(child.stdout.take().expect("stdout")).lines(),
    };

    client
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}}))
        .await;
    client.response(1, "initialize").await;
    client
        .send(json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": cwd.path().to_string_lossy(), "mcpServers": []}}))
        .await;
    let created = client.response(2, "session/new").await;
    let session_id = created["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new failed: {created}"))
        .to_string();

    for (id, text) in [(3, "say hi"), (4, "and again")] {
        client
            .send(
                json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt",
                "params": {"sessionId": session_id,
                           "prompt": [{"type": "text", "text": text}]}}),
            )
            .await;
        let answer = client.response(id, text).await;
        assert_eq!(
            answer["result"]["stopReason"], "end_turn",
            "prompt {id} did not end its turn: {answer}"
        );
    }

    // The turns reached the provider; a request offering no tools is the
    // session title, asked on the side.
    let turns = seen
        .lock()
        .expect("seen requests poisoned")
        .iter()
        .filter(|request| request.tools > 0)
        .count();
    assert_eq!(
        turns,
        2,
        "{:?}",
        seen.lock().expect("seen requests poisoned")
    );

    // Closing stdin is how a client says it is done; the server leaves.
    drop(client);
    let status = tokio::time::timeout(STEP_DEADLINE, child.wait())
        .await
        .expect("the server exits once stdin closes")
        .expect("wait for the server");
    assert!(status.success(), "rebon --acp exited with {status}");
    provider.abort();
}
