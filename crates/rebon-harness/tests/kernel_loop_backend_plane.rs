//! `/agent kernel:dsh` acceptance at the backend seam: a loop on the plugin
//! plane drives one full tool turn through the [`AgentBackend`] contract — the
//! same trait `AgentBackendSwitch` points a session at — with streamed
//! [`SessionUpdate`]s (live publisher + journal double-write), honest usage,
//! and idle-steer / close-session semantics.
//!
//! `plugin_plane_loop_e2e.rs` covers the layer below this one: a spec in, a
//! `LoopHost` out. This one is about the seam a *session* sees, which is a
//! different contract and was the only thing the embedded loop's own test
//! proved. The backend is runtime-agnostic — it holds a spawner — so moving it
//! to the plane is the whole change.
//!
//! Skips without `REBON_TEST_NODE`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rebon_acp_client::journal::TurnJournal;
use rebon_agent_core::backend::{AgentBackend, AgentSessionSpec, SessionResumeMode, SteerOutcome};
use rebon_agent_core::prompt_executor::{PromptCancel, PromptRequest};
use rebon_agent_core::publisher::SessionUpdatePublisher;
use rebon_kernel::Kernel;
use rebon_kernel_seats::kernel_config_seats::ConfigSeatsPlugin;
use rebon_plugin_host::kernel_loop_backend::{KernelLoopBackend, KernelLoopConfig};
use rebon_plugin_host::plugin_manifests::plain_path;
use rebon_provider::kernel_model_router::ModelRouterPlugin;
use rebon_types::{
    ContentBlock, SessionUpdate, SessionUpdateParams, StopReason, TextContent, ToolCallStatus,
};

fn node() -> Option<PathBuf> {
    match std::env::var_os("REBON_TEST_NODE") {
        Some(node) => Some(PathBuf::from(node)),
        None => {
            assert_ne!(
                std::env::var_os("REBON_REQUIRE_TEST_NODE").as_deref(),
                Some(std::ffi::OsStr::new("1")),
                "REBON_REQUIRE_TEST_NODE=1 requires an absolute REBON_TEST_NODE"
            );
            eprintln!("skipping: set REBON_TEST_NODE to an absolute Node executable");
            None
        }
    }
}

/// Points the plane at this checkout, and hands it the Node the test runs with.
/// Process-global, so this test runs alone.
fn point_at_checkout(node: &std::path::Path) {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root resolves");
    std::env::set_var("REBON_PLUGIN_NODE", node);
    std::env::set_var(
        "REBON_PLUGIN_HOST_JS",
        plain_path(&repo.join("runtimes/node/plugin-host/src/cli.mjs")),
    );
    std::env::set_var(
        "REBON_COMPOSE_LOADER_JS",
        plain_path(&repo.join("runtimes/node/compose-runtime/src/index.mjs")),
    );
    std::env::set_var(
        "REBON_KERNEL_JS_DIR",
        plain_path(&repo.join("runtimes/node/compose-runtime/payload")),
    );
}

async fn spawn_scripted_endpoint(responses: Vec<String>) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fake endpoint binds");
    let addr = listener.local_addr().expect("bound address");
    let script = Arc::new(responses);
    let served = Arc::new(Mutex::new(0usize));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let script = script.clone();
            let served = served.clone();
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end = loop {
                    match socket.read(&mut tmp).await {
                        Ok(0) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => return,
                    }
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                while buf.len() < header_end + content_length {
                    match socket.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => return,
                    }
                }
                let index = {
                    let mut served = served.lock().unwrap();
                    let index = *served;
                    *served += 1;
                    index
                };
                let sse = script
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| script.last().cloned().unwrap_or_default());
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    sse.len(),
                    sse
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    addr
}

fn sse_body(events: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// Live sink: every published update, in order.
#[derive(Default)]
struct CollectingPublisher(Mutex<Vec<SessionUpdateParams>>);

#[async_trait]
impl SessionUpdatePublisher for CollectingPublisher {
    async fn publish_owned(&self, params: SessionUpdateParams) {
        self.0.lock().unwrap().push(params);
    }
}

/// Journal double-write probe: labels of everything recorded.
#[derive(Default)]
struct RecordingJournal(Mutex<Vec<String>>);

impl TurnJournal for RecordingJournal {
    fn begin_turn(&self, _session_id: &str, _prompt: &[ContentBlock]) -> Option<String> {
        self.0.lock().unwrap().push("begin".into());
        Some("turn-1".into())
    }

    fn record_update(&self, _session_id: &str, update: &SessionUpdate) {
        let label = match update {
            SessionUpdate::AgentMessageChunk { .. } => "chunk",
            SessionUpdate::ToolCall { .. } => "tool_call",
            SessionUpdate::ToolCallUpdate { .. } => "tool_call_update",
            SessionUpdate::ThinkingDelta { .. } => "thinking",
            SessionUpdate::ThinkingEnd => "thinking_end",
            _ => "other",
        };
        self.0.lock().unwrap().push(label.into());
    }

    fn end_turn(&self, _session_id: &str, turn_id: &str, stop_reason: Option<StopReason>) {
        self.0
            .lock()
            .unwrap()
            .push(format!("end:{turn_id}:{stop_reason:?}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_backend_runs_a_switched_session_turn_end_to_end() {
    let Some(node) = node() else { return };
    point_at_checkout(&node);

    let workspace = tempfile::tempdir().expect("workspace dir");
    let probe_file = workspace.path().join("loop-probe.txt");
    std::fs::write(&probe_file, "后端轮暗号BACKEND-9257\n").expect("probe file written");

    let read_args = serde_json::json!({ "file_path": probe_file.to_string_lossy() }).to_string();
    let step_one = sse_body(&[
        serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content":"查文件"}}]}),
        serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call-backend-1","function":{"name":"Read","arguments": read_args}}
        ]}}]}),
        serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        serde_json::json!({"choices":[],"usage":{"prompt_tokens":30,"completion_tokens":9}}),
    ]);
    let step_two = sse_body(&[
        serde_json::json!({"choices":[{"index":0,"delta":{"content":"暗号已读出。"}}]}),
        serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        serde_json::json!({"choices":[],"usage":{"prompt_tokens":55,"completion_tokens":6}}),
    ]);
    let addr = spawn_scripted_endpoint(vec![step_one, step_two]).await;
    std::env::set_var("REBON_TEST_LOOP_BACKEND_KEY", "sk-backend-fake-key");

    let config_dir = tempfile::tempdir().expect("config dir");
    std::fs::write(
        config_dir.path().join("config.json"),
        serde_json::json!({
            "kernelPlugins": {
                "plugins": [{
                    "id": "llm-deepseek",
                    "name": "@deepseek-ai/dsh-llm-deepseek",
                    "config": {
                        "apiKeyEnv": "REBON_TEST_LOOP_BACKEND_KEY",
                        "baseURL": format!("http://{addr}"),
                        "models": [{ "id": "deepseek-v4-flash", "name": "F",
                                     "contextWindow": 128000 }],
                    },
                }],
                "credentialGrants": ["REBON_TEST_LOOP_BACKEND_KEY"],
                "loopAgent": { "provider": "deepseek-official", "model": "deepseek-v4-flash" },
            },
        })
        .to_string(),
    )
    .expect("config written");

    let kernel = Kernel::new();
    kernel
        .load(vec![
            Box::new(ModelRouterPlugin),
            Box::new(ConfigSeatsPlugin::new(config_dir.path().to_path_buf())),
        ])
        .expect("kernel seats load");

    let loop_config =
        KernelLoopConfig::from_config_dir(config_dir.path()).expect("loopAgent config parses");
    assert_eq!(loop_config.provider, "deepseek-official");
    assert_eq!(loop_config.vendor, "dsh");
    let journal = Arc::new(RecordingJournal::default());
    let backend = KernelLoopBackend::new(
        "kernel:dsh",
        Arc::new(rebon_plugin_host::kernel_loop_plane::PlaneLoopSpawner::new(
            kernel.context().clone(),
        )),
        loop_config,
        journal.clone(),
    );

    // The switch edge: start the session explicitly (fresh = Created).
    let spec = AgentSessionSpec::new("backend-e2e", workspace.path().to_string_lossy());
    let started = backend
        .start_session(spec.clone())
        .await
        .expect("session starts");
    assert_eq!(started.mode, SessionResumeMode::Created);
    assert!(started.session_id.starts_with("backend-e2e-session-"));
    // Idempotent restart reuses the live host.
    let restarted = backend.start_session(spec).await.expect("restart");
    assert_eq!(restarted.mode, SessionResumeMode::Reused);

    // One full turn through the trait.
    let publisher = Arc::new(CollectingPublisher::default());
    let outcome = backend
        .prompt(PromptRequest {
            user_prompt: None,
            effort_is_session_default: false,
            session_id: "backend-e2e".into(),
            cwd: workspace.path().to_string_lossy().into_owned(),
            prompt: vec![ContentBlock::Text(TextContent {
                text: "请读取探针文件并复述暗号。".into(),
                annotations: None,
            })],
            mcp_servers: Vec::new(),
            update_publisher: Some(publisher.clone()),
            permission_publisher: None,
            cancel: PromptCancel::new(),
            thinking_budget: None,
            max_tokens: None,
            reasoning_effort_ordinal: None,
            additional_working_directories: Vec::new(),
            coordinator_mode: None,
            coordinator_report_paths: Vec::new(),
            user_message_uuid: None,
            background_agent_system: None,
            background_agent_tool_filter: None,
            execution_policy: None,
            replay_requests: Vec::new(),
            skill_invocations: Vec::new(),
        })
        .await
        .expect("the switched turn completes");

    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    // Honest usage, summed over both model steps (30+55 / 9+6).
    //
    // This is also where event ordering shows up: a turn's usage is snapshotted
    // when `turn/end` arrives, so the second step's `assistant/message` has to
    // reach the translator before it. It did not, until the supervisor gave
    // upstream events one ordered lane instead of a task each.
    assert_eq!(outcome.usage.input_tokens, 85);
    assert_eq!(outcome.usage.output_tokens, 15);

    // The live stream told the whole story.
    let updates = publisher.0.lock().unwrap().clone();
    let text: String = updates
        .iter()
        .filter_map(|params| match &params.update {
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(text),
            } => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "暗号已读出。");
    let thinking: String = updates
        .iter()
        .filter_map(|params| match &params.update {
            SessionUpdate::ThinkingDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, "查文件");
    let tool_call = updates
        .iter()
        .find_map(|params| match &params.update {
            SessionUpdate::ToolCall {
                tool_call_id,
                title,
                status,
                raw_input,
                ..
            } => Some((
                tool_call_id.clone(),
                title.clone(),
                *status,
                raw_input.clone(),
            )),
            _ => None,
        })
        .expect("tool call announced");
    assert_eq!(
        tool_call.0, "call-backend-1",
        "tool call update malformed; all updates: {updates:#?}"
    );
    assert_eq!(tool_call.1, "Read");
    assert_eq!(tool_call.2, ToolCallStatus::InProgress);
    assert!(
        tool_call
            .3
            .is_some_and(|input| input.contains_key("file_path")),
        "raw input must carry the parsed arguments"
    );
    let tool_update = updates
        .iter()
        .find_map(|params| match &params.update {
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status,
                content,
                ..
            } => Some((tool_call_id.clone(), *status, content.clone())),
            _ => None,
        })
        .expect("tool result streamed");
    assert_eq!(tool_update.0, "call-backend-1");
    assert_eq!(tool_update.1, Some(ToolCallStatus::Completed));
    assert!(
        format!("{:?}", tool_update.2).contains("BACKEND-9257"),
        "tool result must carry the file payload: {:?}",
        tool_update.2
    );

    // Journal saw the same stream, bracketed by begin/end.
    let recorded = journal.0.lock().unwrap().clone();
    assert_eq!(recorded.first().map(String::as_str), Some("begin"));
    assert!(recorded.iter().any(|r| r == "tool_call"), "{recorded:?}");
    assert!(
        recorded.iter().any(|r| r == "tool_call_update"),
        "{recorded:?}"
    );
    assert!(recorded.iter().any(|r| r == "chunk"), "{recorded:?}");
    assert!(
        recorded
            .last()
            .is_some_and(|r| r == "end:turn-1:Some(EndTurn)"),
        "{recorded:?}"
    );

    // Steering an idle loop refuses cleanly (the caller re-prompts).
    let steered = backend
        .steer(
            "backend-e2e",
            vec![ContentBlock::Text(TextContent {
                text: "补充".into(),
                annotations: None,
            })],
            "uuid-1",
        )
        .await
        .expect("steer resolves");
    assert_eq!(steered, SteerOutcome::TurnAlreadyOver);

    // The switch-back edge: close releases the per-loop host.
    backend
        .close_session("backend-e2e")
        .await
        .expect("close succeeds");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        // A restarted session after close would spawn fresh — proving the
        // old host is gone is what matters; poll via a new start_session.
        if backend
            .start_session(AgentSessionSpec::new(
                "backend-e2e",
                workspace.path().to_string_lossy(),
            ))
            .await
            .expect("session restarts after close")
            .mode
            == SessionResumeMode::Created
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "closed session must not be reused"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    backend
        .close_session("backend-e2e")
        .await
        .expect("second close");

    std::env::remove_var("REBON_TEST_LOOP_BACKEND_KEY");
}
