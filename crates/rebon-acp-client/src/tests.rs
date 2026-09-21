//! End-to-end tests against a scripted agent on an in-memory pipe.
//!
//! [`FakeAgent`] is a real ACP peer — it reads NDJSON frames, answers
//! `initialize`/`session/new`/`session/load`/`session/prompt`, and can
//! push `session/update` notifications and reverse requests back. That
//! makes the interesting paths (id rewriting, three-tier resume,
//! reconnect, a dead agent mid-turn) reachable without launching a
//! binary.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rebon_agent_core::backend::{
    AgentBackend, AgentBackendKind, AgentSessionSpec, SessionResumeMode,
};
use rebon_agent_core::prompt_executor::{PromptCancel, PromptRequest};
use rebon_agent_core::publisher::{
    ChannelPermissionRequestPublisher, MemorySessionUpdatePublisher, SessionUpdatePublisher,
};
use rebon_proto::types::{
    AgentCapabilities as ProtoAgentCapabilities, JsonRpcMessage, JsonRpcResponse, JsonRpcVersion,
    RequestPermissionParams, RequestPermissionResult, SessionUpdate, SessionUpdateParams,
    TextContent,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

use crate::backend::AcpAgentBackend;
use crate::client::{default_client_capabilities, AcpClient, ClientError};
use crate::connection::ClientDelegate;
use crate::connector::AgentConnector;
use crate::host_fs::DirectHostFs;
use crate::journal::{NoopTurnJournal, TurnJournal};

// ── the scripted agent ────────────────────────────────────────────

/// What the fake agent should do when asked.
#[derive(Clone)]
pub(crate) struct AgentScript {
    /// Reported in the `initialize` result.
    pub load_session: bool,
    /// Agent-side session ids handed out by `session/new`, in order.
    pub new_session_ids: Vec<String>,
    /// Whether `session/load` succeeds.
    pub load_succeeds: bool,
    /// Reject `session/new` — the agent is up but refuses the cwd.
    pub new_session_fails: bool,
    /// Text pushed as a `session/update` before answering a prompt.
    pub prompt_update_text: Option<String>,
    /// Ask for permission before answering a prompt.
    pub ask_permission: bool,
    /// Advertise `_session/steering` in the initialize `_meta`.
    pub steering_supported: bool,
    /// What a steering request is answered with.
    pub steering_outcome: &'static str,
    /// Hold the prompt response until a steering request arrives, so a
    /// test has a turn that is genuinely still running when it steers.
    pub hold_prompt_for_steer: bool,
    /// Die (close the pipe) instead of answering the Nth prompt.
    pub die_on_prompt: Option<usize>,
}

impl Default for AgentScript {
    fn default() -> Self {
        Self {
            load_session: false,
            new_session_ids: vec!["agent-sess-1".into()],
            load_succeeds: false,
            new_session_fails: false,
            prompt_update_text: None,
            ask_permission: false,
            steering_supported: false,
            steering_outcome: "injected",
            hold_prompt_for_steer: false,
            die_on_prompt: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct AgentLog {
    pub methods: Mutex<Vec<String>>,
    /// Every request's params, so tests can assert what the agent was
    /// actually sent — the mcpServers list in particular.
    pub params: Mutex<Vec<(String, serde_json::Value)>>,
}

impl AgentLog {
    fn record(&self, method: &str, params: &serde_json::Value) {
        self.methods
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(method.to_string());
        self.params
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((method.to_string(), params.clone()));
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.methods
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn count(&self, method: &str) -> usize {
        self.calls().iter().filter(|m| m.as_str() == method).count()
    }

    /// The params of every `method` call, in order.
    pub(crate) fn params_of(&self, method: &str) -> Vec<serde_json::Value> {
        self.params
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, params)| params.clone())
            .collect()
    }
}

fn ok(id: serde_json::Value, result: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn err(id: serde_json::Value, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": -32000, "message": message}
    })
    .to_string()
}

/// Serve one connection until the client hangs up.
async fn serve(mut stream: DuplexStream, script: AgentScript, log: Arc<AgentLog>) {
    let (read_half, mut write_half) = tokio::io::split(&mut stream);
    let mut lines = BufReader::new(read_half).lines();
    let mut prompts = 0usize;
    let mut new_sessions = 0usize;
    let mut current_session = String::new();
    // The request id of a prompt held open for a steering request.
    let mut held_prompt: Option<serde_json::Value> = None;

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = JsonRpcMessage::from_bytes(line.as_bytes()) else {
            continue;
        };
        let request = match message {
            JsonRpcMessage::Request(request) => request,
            // Notifications get no reply but tests still need to see
            // them — `session/cancel` in particular.
            JsonRpcMessage::Notification(notification) => {
                let params = notification
                    .params
                    .clone()
                    .unwrap_or(serde_json::Value::Null);
                log.record(&notification.method, &params);
                continue;
            }
            // The client answering one of our reverse requests.
            JsonRpcMessage::Response(_) => continue,
        };
        let id = serde_json::to_value(&request.id).unwrap();
        let params = request.params.clone().unwrap_or(serde_json::Value::Null);
        log.record(&request.method, &params);

        let reply = match request.method.as_str() {
            "initialize" => {
                let mut result = serde_json::json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {"loadSession": script.load_session},
                    "agentInfo": {"name": "fake-agent", "version": "0.0.1"}
                });
                if script.steering_supported {
                    // The real adapters put this at the TOP level of
                    // the result, sibling of agentCapabilities.
                    result["_meta"] = serde_json::json!({"steering": {"supported": true}});
                }
                ok(id, result)
            }
            "session/new" if script.new_session_fails => err(id, "cwd is not trusted"),
            "session/new" => {
                let session_id = script
                    .new_session_ids
                    .get(new_sessions)
                    .cloned()
                    .unwrap_or_else(|| format!("agent-sess-{}", new_sessions + 1));
                new_sessions += 1;
                current_session = session_id.clone();
                ok(id, serde_json::json!({"sessionId": session_id}))
            }
            "session/load" => {
                if script.load_succeeds {
                    let session_id = params
                        .get("sessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("agent-sess-loaded")
                        .to_string();
                    current_session = session_id.clone();
                    ok(id, serde_json::json!({"sessionId": session_id}))
                } else {
                    err(id, "no such session")
                }
            }
            "session/prompt" => {
                prompts += 1;
                if script.die_on_prompt == Some(prompts) {
                    // Hang up mid-turn without answering.
                    return;
                }
                let session_id = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or(current_session.as_str())
                    .to_string();

                if let Some(text) = &script.prompt_update_text {
                    let update = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": text}
                            }
                        }
                    })
                    .to_string();
                    let _ = write_half.write_all(update.as_bytes()).await;
                    let _ = write_half.write_all(b"\n").await;
                    let _ = write_half.flush().await;
                }

                if script.ask_permission {
                    let ask = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 9001,
                        "method": "session/request_permission",
                        "params": {
                            "sessionId": session_id,
                            "toolCall": {"toolCallId": "call-1", "title": "Write file"},
                            "options": [
                                {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                                {"optionId": "deny", "name": "Deny", "kind": "reject_once"}
                            ]
                        }
                    })
                    .to_string();
                    let _ = write_half.write_all(ask.as_bytes()).await;
                    let _ = write_half.write_all(b"\n").await;
                    let _ = write_half.flush().await;
                    // Wait for the client's answer before finishing.
                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Ok(JsonRpcMessage::Response(_)) =
                            JsonRpcMessage::from_bytes(line.as_bytes())
                        {
                            break;
                        }
                    }
                }

                if script.hold_prompt_for_steer && held_prompt.is_none() {
                    // Keep the turn genuinely running until a steering
                    // request lands, then answer both.
                    held_prompt = Some(id);
                    continue;
                }
                ok(id, serde_json::json!({"stopReason": "end_turn"}))
            }
            "_session/steering" => {
                let steer_reply = ok(id, serde_json::json!({"outcome": script.steering_outcome}));
                let _ = write_half.write_all(steer_reply.as_bytes()).await;
                let _ = write_half.write_all(b"\n").await;
                let _ = write_half.flush().await;
                if let Some(held_id) = held_prompt.take() {
                    // A steered message is accepted *into* a turn that
                    // keeps running — so keep working briefly before
                    // answering the prompt, the way a real agent does.
                    // Answering instantly would close the turn before
                    // the client had processed the steer response,
                    // which is a race no real steering hits.
                    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                    let prompt_reply = ok(held_id, serde_json::json!({"stopReason": "end_turn"}));
                    let _ = write_half.write_all(prompt_reply.as_bytes()).await;
                    let _ = write_half.write_all(b"\n").await;
                    let _ = write_half.flush().await;
                }
                continue;
            }
            _ => err(id, "method not found"),
        };

        let _ = write_half.write_all(reply.as_bytes()).await;
        let _ = write_half.write_all(b"\n").await;
        let _ = write_half.flush().await;
    }
}

/// Connects to a freshly spawned [`serve`] task each time.
pub(crate) struct FakeConnector {
    script: AgentScript,
    pub log: Arc<AgentLog>,
    pub connects: AtomicUsize,
}

impl FakeConnector {
    fn new(script: AgentScript) -> Self {
        Self {
            script,
            log: Arc::new(AgentLog::default()),
            connects: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl AgentConnector for FakeConnector {
    async fn connect(&self, delegate: Arc<dyn ClientDelegate>) -> Result<AcpClient, ClientError> {
        self.connects.fetch_add(1, Ordering::Relaxed);
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let script = self.script.clone();
        let log = self.log.clone();
        tokio::spawn(serve(theirs, script, log));
        let (reader, writer) = tokio::io::split(ours);
        AcpClient::connect_over_pipe(
            reader,
            writer,
            None,
            "fake-agent",
            default_client_capabilities(),
            delegate,
        )
        .await
    }

    fn label(&self) -> String {
        "fake-agent".to_string()
    }
}

fn backend(script: AgentScript) -> (Arc<AcpAgentBackend>, Arc<FakeConnector>) {
    backend_with_journal(script, Arc::new(NoopTurnJournal))
}

fn backend_with_journal(
    script: AgentScript,
    journal: Arc<dyn TurnJournal>,
) -> (Arc<AcpAgentBackend>, Arc<FakeConnector>) {
    let connector = Arc::new(FakeConnector::new(script));
    let backend = Arc::new(AcpAgentBackend::with_connector(
        "fake",
        connector.clone(),
        Arc::new(DirectHostFs),
        journal,
        Vec::new(),
    ));
    (backend, connector)
}

fn prompt_request(session_id: &str) -> PromptRequest {
    PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session_id.into(),
        cwd: "/tmp/work".into(),
        prompt: vec![rebon_types::ContentBlock::Text(TextContent {
            text: "hello".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: None,
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
    }
}

// ── tests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_first_session_is_created() {
    let (backend, connector) = backend(AgentScript::default());
    let start = backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();

    assert_eq!(start.mode, SessionResumeMode::Created);
    assert_eq!(start.session_id, "agent-sess-1");
    assert_eq!(connector.log.count("session/new"), 1);
    assert_eq!(backend.kind(), AgentBackendKind::Acp);
}

#[tokio::test]
async fn a_live_session_is_reused_without_touching_the_wire() {
    let (backend, connector) = backend(AgentScript::default());
    backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();
    let again = backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();

    assert_eq!(again.mode, SessionResumeMode::Reused);
    assert_eq!(again.session_id, "agent-sess-1");
    // Reuse means exactly that: no second session/new, no session/load.
    assert_eq!(connector.log.count("session/new"), 1);
    assert_eq!(connector.log.count("session/load"), 0);
}

#[tokio::test]
async fn a_host_supplied_id_is_loaded_when_the_agent_can() {
    let (backend, connector) = backend(AgentScript {
        load_session: true,
        load_succeeds: true,
        ..AgentScript::default()
    });

    let start = backend
        .start_session(
            AgentSessionSpec::new("host-1", "/tmp/work").with_resume_session_id("agent-sess-old"),
        )
        .await
        .unwrap();

    assert_eq!(start.mode, SessionResumeMode::Loaded);
    assert_eq!(start.session_id, "agent-sess-old");
    assert_eq!(connector.log.count("session/load"), 1);
    assert_eq!(connector.log.count("session/new"), 0);
}

#[tokio::test]
async fn an_agent_without_load_session_skips_straight_to_create() {
    let (backend, connector) = backend(AgentScript {
        load_session: false,
        ..AgentScript::default()
    });

    let start = backend
        .start_session(
            AgentSessionSpec::new("host-1", "/tmp/work").with_resume_session_id("agent-sess-old"),
        )
        .await
        .unwrap();

    assert_eq!(start.mode, SessionResumeMode::Created);
    // No point sending a request the handshake said would be refused.
    assert_eq!(connector.log.count("session/load"), 0);
    assert_eq!(connector.log.count("session/new"), 1);
}

#[tokio::test]
async fn a_failed_load_falls_back_to_a_new_session() {
    let (backend, connector) = backend(AgentScript {
        load_session: true,
        load_succeeds: false,
        ..AgentScript::default()
    });

    let start = backend
        .start_session(
            AgentSessionSpec::new("host-1", "/tmp/work").with_resume_session_id("agent-sess-gone"),
        )
        .await
        .unwrap();

    // A session the agent no longer has is a cold start, not an error
    // the user has to do something about.
    assert_eq!(start.mode, SessionResumeMode::Created);
    assert_eq!(connector.log.count("session/load"), 1);
    assert_eq!(connector.log.count("session/new"), 1);
}

#[tokio::test]
async fn updates_are_rewritten_to_the_host_session_id() {
    let (backend, _connector) = backend(AgentScript {
        prompt_update_text: Some("streaming".into()),
        ..AgentScript::default()
    });

    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let mut request = prompt_request("host-1");
    request.update_publisher = Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>);

    let outcome = backend.prompt(request).await.unwrap();
    assert_eq!(outcome.stop_reason, rebon_types::StopReason::EndTurn);

    let updates = publisher.snapshot();
    assert_eq!(updates.len(), 1, "the streamed chunk must reach the host");
    // The agent said "agent-sess-1"; the host only knows "host-1".
    assert_eq!(updates[0].session_id, "host-1");
    match &updates[0].update {
        SessionUpdate::AgentMessageChunk {
            content: rebon_types::ContentBlock::Text(text),
        } => {
            assert_eq!(text.text, "streaming");
        }
        other => panic!("unexpected update: {other:?}"),
    }
}

#[tokio::test]
async fn updates_outside_a_turn_are_dropped_not_misrouted() {
    let (backend, _connector) = backend(AgentScript::default());
    backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();

    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    // Nothing registered this publisher for a turn, so an update
    // arriving now has nowhere legitimate to go.
    let router_update = SessionUpdateParams {
        session_id: "agent-sess-1".into(),
        update: SessionUpdate::AgentMessageChunk {
            content: rebon_types::ContentBlock::Text(TextContent {
                text: "late".into(),
                annotations: None,
            }),
        },
    };
    backend.router().session_update(router_update).await;
    assert!(publisher.snapshot().is_empty());
}

#[tokio::test]
async fn a_permission_request_reaches_the_host_and_the_answer_goes_back() {
    let (backend, _connector) = backend(AgentScript {
        ask_permission: true,
        ..AgentScript::default()
    });

    let (permission_publisher, mut permission_rx) = ChannelPermissionRequestPublisher::new();
    let mut request = prompt_request("host-1");
    request.permission_publisher = Some(permission_publisher);

    // Answer the prompt from the host side as soon as it arrives.
    let answered = tokio::spawn(async move {
        let outbound = permission_rx.recv().await.expect("permission must arrive");
        let host_session = outbound.params.session_id.clone();
        let response = rebon_agent_core::publisher::make_permission_result_response(
            outbound.request_id,
            RequestPermissionResult {
                outcome: rebon_proto::types::PermissionOutcome::Selected,
                option_id: Some("allow".into()),
                updated_input: None,
            },
        );
        let _ = outbound.response_tx.send(response);
        host_session
    });

    let outcome = backend.prompt(request).await.unwrap();
    assert_eq!(outcome.stop_reason, rebon_types::StopReason::EndTurn);
    // The host saw its own session id, not the agent's.
    assert_eq!(answered.await.unwrap(), "host-1");
}

#[tokio::test]
async fn an_agent_that_dies_mid_turn_fails_the_turn_instead_of_hanging() {
    let (backend, _connector) = backend(AgentScript {
        die_on_prompt: Some(1),
        ..AgentScript::default()
    });

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        backend.prompt(prompt_request("host-1")),
    )
    .await
    .expect("a dead agent must not hang the turn");

    let err = result.expect_err("the turn cannot have succeeded");
    assert!(
        err.to_string().contains("closed") || err.to_string().contains("connection"),
        "error should name the lost connection: {err}"
    );
}

#[tokio::test]
async fn a_dead_agent_reconnects_and_resumes_by_id() {
    let (backend, connector) = backend(AgentScript {
        load_session: true,
        load_succeeds: true,
        die_on_prompt: Some(1),
        ..AgentScript::default()
    });

    // First turn: session created, then the agent hangs up.
    let _ = backend.prompt(prompt_request("host-1")).await;

    // Second turn: the connection is dead, so the backend reconnects
    // and asks to load the id it remembers rather than starting cold.
    let start = backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();

    assert_eq!(start.mode, SessionResumeMode::Loaded);
    assert_eq!(start.session_id, "agent-sess-1");
    assert_eq!(connector.connects.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn closing_a_session_forgets_its_mapping() {
    let (backend, connector) = backend(AgentScript {
        new_session_ids: vec!["agent-sess-1".into(), "agent-sess-2".into()],
        ..AgentScript::default()
    });

    backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();
    assert_eq!(
        backend.agent_session_id("host-1").as_deref(),
        Some("agent-sess-1")
    );

    backend.close_session("host-1").await.unwrap();
    assert!(backend.agent_session_id("host-1").is_none());

    // A closed session starts over rather than reusing the old id.
    let start = backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();
    assert_eq!(start.mode, SessionResumeMode::Created);
    assert_eq!(start.session_id, "agent-sess-2");
    assert_eq!(connector.log.count("session/new"), 2);
}

#[tokio::test]
async fn cancel_before_any_session_is_a_no_op() {
    let (backend, connector) = backend(AgentScript::default());
    backend.cancel("host-never-started").await.unwrap();
    // Nothing was started, so nothing was connected to cancel.
    assert_eq!(connector.connects.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn capabilities_admit_what_this_leg_cannot_do() {
    let (backend, _connector) = backend(AgentScript::default());
    let caps = backend.capabilities();

    // DirectHostFs takes no snapshots, so rewind must not be offered.
    assert!(!caps.writes_through_host_fs);
    assert!(!caps.supports_rewind());
    // ACP's prompt result has no usage field.
    assert!(!caps.reports_usage);
    // We do serve session/request_permission.
    assert!(caps.host_permission_prompts);
}

#[tokio::test]
async fn a_backend_reports_the_agent_capabilities_it_negotiated() {
    let connector = Arc::new(FakeConnector::new(AgentScript {
        load_session: true,
        ..AgentScript::default()
    }));
    let delegate: Arc<dyn ClientDelegate> = Arc::new(NullDelegate);
    let client = connector.connect(delegate).await.unwrap();
    assert!(client.supports_load_session());
    assert_eq!(
        client.agent_info().map(|info| info.name.as_str()),
        Some("fake-agent")
    );
    assert!(client.is_connected());
    assert_eq!(
        client.agent_capabilities().load_session,
        Some(true),
        "handshake result must be retained verbatim"
    );
    let _ = ProtoAgentCapabilities::default();
}

struct NullDelegate;

#[async_trait::async_trait]
impl ClientDelegate for NullDelegate {
    async fn session_update(&self, _params: SessionUpdateParams) {}

    async fn request_permission(
        &self,
        _params: RequestPermissionParams,
    ) -> anyhow::Result<RequestPermissionResult> {
        Err(anyhow::anyhow!("no permissions here"))
    }

    async fn read_text_file(
        &self,
        _params: rebon_proto::types::ReadTextFileParams,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("no fs here"))
    }

    async fn write_text_file(
        &self,
        _params: rebon_proto::types::WriteTextFileParams,
    ) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("no fs here"))
    }
}

#[tokio::test]
async fn an_unknown_reverse_method_is_refused_not_ignored() {
    // A response must come back for every request, or the agent waits
    // forever for a method we never implemented.
    let (ours, mut theirs) = tokio::io::duplex(8 * 1024);
    let (reader, writer) = tokio::io::split(ours);
    let _connection = crate::connection::Connection::spawn(
        reader,
        writer,
        rebon_proto::FramingMode::Ndjson,
        Arc::new(NullDelegate),
    );

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "terminal/create",
        "params": {}
    })
    .to_string();
    theirs.write_all(request.as_bytes()).await.unwrap();
    theirs.write_all(b"\n").await.unwrap();
    theirs.flush().await.unwrap();

    let mut lines = BufReader::new(theirs).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .expect("a reply must come back")
        .unwrap()
        .expect("a reply must come back");
    let response: JsonRpcResponse = serde_json::from_str(&line).unwrap();
    let error = response.error.expect("unsupported methods must error");
    assert!(error.message.contains("terminal/create"));
    let _ = JsonRpcVersion;
}

#[tokio::test]
async fn an_id_from_a_previous_run_is_loaded_without_the_host_repeating_it() {
    // Across a restart the host has the agent's id in the session
    // sidecar. Handing it back once is what turns a cold start into a
    // `session/load` — including for the prompt path, which builds its
    // own spec and offers nothing.
    let (backend, connector) = backend(AgentScript {
        load_session: true,
        load_succeeds: true,
        ..AgentScript::default()
    });
    backend.remember_agent_session("host-1", "agent-sess-old");
    assert_eq!(
        backend.remembered_agent_session_id("host-1").as_deref(),
        Some("agent-sess-old")
    );

    let start = backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();

    assert_eq!(start.mode, SessionResumeMode::Loaded);
    assert_eq!(start.session_id, "agent-sess-old");
    assert_eq!(connector.log.count("session/load"), 1);
    assert_eq!(
        connector.log.count("session/new"),
        0,
        "a resumable session must not be thrown away"
    );
}

#[tokio::test]
async fn a_remembered_id_is_not_mistaken_for_a_live_session() {
    // The agent has never been asked about this id, so reporting a
    // reuse would skip the `session/load` that actually restores it.
    let (backend, connector) = backend(AgentScript {
        load_session: true,
        load_succeeds: false,
        ..AgentScript::default()
    });
    backend.remember_agent_session("host-1", "agent-sess-gone");

    let start = backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();

    // The agent no longer has it: that is a cold start, not an error.
    assert_eq!(start.mode, SessionResumeMode::Created);
    assert_eq!(connector.log.count("session/load"), 1);
    assert_eq!(connector.log.count("session/new"), 1);
}

// ── the turn, as Rebon remembers it ───────────────────────────────

fn journal_root(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("rebon-acp-backend-journal-{name}-"))
        .tempdir()
        .expect("projects root")
}

fn journal_rows(root: &std::path::Path, session: &str) -> Vec<(String, serde_json::Value)> {
    let path = rebon_session::transcript_file_path(root, "/tmp/work", session);
    rebon_session::load_raw_transcript_from_file(&path)
        .expect("read transcript")
        .map(|raw| {
            raw.entries
                .into_iter()
                .map(|entry| (entry.entry_type, entry.raw))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_turn_run_on_an_agent_lands_in_rebons_transcript() {
    // The whole point of the journal: after the agent answers, the
    // conversation is in Rebon's history, not only on the screen.
    let root = journal_root("happy");
    let journal = Arc::new(crate::journal::TranscriptJournal::new(
        root.path(),
        "/tmp/work",
        "host-1",
        "fake",
    ));
    let (backend, _connector) = backend_with_journal(
        AgentScript {
            prompt_update_text: Some("all done".into()),
            ..AgentScript::default()
        },
        journal,
    );

    let outcome = backend.prompt(prompt_request("host-1")).await.unwrap();
    assert_eq!(outcome.stop_reason, rebon_types::StopReason::EndTurn);

    let rows = journal_rows(root.path(), "host-1");
    assert_eq!(rows.len(), 2, "the prompt and the answer");
    assert_eq!(rows[0].0, "user");
    assert_eq!(rows[0].1["message"]["content"][0]["text"], "hello");
    assert_eq!(rows[1].0, "assistant");
    assert_eq!(rows[1].1["message"]["content"][0]["text"], "all done");

    // And the agent's own session id is stored, so a later run can ask
    // for it back instead of starting cold.
    assert_eq!(
        rebon_session::load_agent_session_id(root.path(), "/tmp/work", "host-1").as_deref(),
        Some("agent-sess-1")
    );
    assert_eq!(
        rebon_session::load_session_agent(root.path(), "/tmp/work", "host-1").as_deref(),
        Some("fake")
    );
}

#[tokio::test]
async fn a_turn_whose_agent_dies_is_still_closed_out() {
    // The turn failed, so nothing more will arrive — but the prompt is
    // already history, and the file-history boundary must be released
    // or the next turn would snapshot into a turn that already ended.
    #[derive(Default)]
    struct Spy(Mutex<Vec<String>>);
    impl crate::host_fs::HostFileHistory for Spy {
        fn begin_turn(&self, _session: &str, turn: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("begin {turn}"));
            Ok(())
        }
        fn snapshot_before_write(
            &self,
            _session: &str,
            _path: &std::path::Path,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn end_turn(&self, _session: &str, turn: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("end {turn}"));
            Ok(())
        }
    }

    let root = journal_root("agent-dies");
    let spy = Arc::new(Spy::default());
    let journal = Arc::new(
        crate::journal::TranscriptJournal::new(root.path(), "/tmp/work", "host-1", "fake")
            .with_file_history(spy.clone()),
    );
    let (backend, _connector) = backend_with_journal(
        AgentScript {
            die_on_prompt: Some(1),
            ..AgentScript::default()
        },
        journal,
    );

    let result = backend.prompt(prompt_request("host-1")).await;
    assert!(result.is_err(), "a dead agent is a failed turn");

    let rows = journal_rows(root.path(), "host-1");
    assert_eq!(rows.len(), 1, "the prompt survives the failed turn");
    assert_eq!(rows[0].0, "user");
    assert!(
        rebon_session::load_agent_session_id(root.path(), "/tmp/work", "host-1").is_some(),
        "the session existed before the turn failed, so its id is still worth keeping"
    );

    let calls = spy.0.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "armed and released: {calls:?}");
    assert!(calls[1].starts_with("end "), "{calls:?}");
}

#[tokio::test]
async fn a_session_the_agent_rejects_leaves_no_orphan_prompt() {
    // The turn is journalled only once the session exists, so a
    // session that never started does not litter the transcript with a
    // prompt that was never asked.
    let root = journal_root("no-session");
    let journal = Arc::new(crate::journal::TranscriptJournal::new(
        root.path(),
        "/tmp/work",
        "host-1",
        "fake",
    ));
    let (backend, _connector) = backend_with_journal(
        AgentScript {
            new_session_fails: true,
            ..AgentScript::default()
        },
        journal,
    );

    assert!(backend.prompt(prompt_request("host-1")).await.is_err());
    assert!(journal_rows(root.path(), "host-1").is_empty());
}

// ── injected MCP servers ──────────────────────────────────────────

fn stdio_server(name: &str, command: &str) -> rebon_proto::types::McpServerConfig {
    rebon_proto::types::McpServerConfig::Stdio {
        name: name.to_string(),
        command: command.to_string(),
        args: Vec::new(),
        env: Default::default(),
        cwd: None,
    }
}

fn server_names(params: &serde_json::Value) -> Vec<(String, String)> {
    params["mcpServers"]
        .as_array()
        .expect("mcpServers is required by the spec")
        .iter()
        .map(|server| {
            (
                server["name"].as_str().unwrap_or_default().to_string(),
                server["command"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn injected_servers_ride_into_session_new() {
    let (backend, connector) = backend(AgentScript::default());
    let backend =
        backend_owned(backend).with_injected_mcp_servers(vec![stdio_server("rebon-fs", "rebon")]);

    backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();

    let news = connector.log.params_of("session/new");
    assert_eq!(news.len(), 1);
    assert_eq!(
        server_names(&news[0]),
        vec![("rebon-fs".to_string(), "rebon".to_string())],
        "the injected server must reach the agent"
    );
}

#[tokio::test]
async fn a_spec_cannot_shadow_an_injected_server() {
    // The spec's list replaces the config's — but the injected tools
    // are the host's own, and a same-named entry must lose to them.
    let (backend, connector) = backend(AgentScript::default());
    let backend =
        backend_owned(backend).with_injected_mcp_servers(vec![stdio_server("rebon-fs", "rebon")]);

    let mut spec = AgentSessionSpec::new("host-1", "/tmp/work");
    spec.mcp_servers = vec![
        stdio_server("rebon-fs", "impostor"),
        stdio_server("other", "other-bin"),
    ];
    backend.start_session(spec).await.unwrap();

    let news = connector.log.params_of("session/new");
    let servers = server_names(&news[0]);
    assert_eq!(
        servers.len(),
        2,
        "deduped by name, not doubled: {servers:?}"
    );
    assert!(
        servers.contains(&("other".to_string(), "other-bin".to_string())),
        "the spec's own servers still ride: {servers:?}"
    );
    assert!(
        servers.contains(&("rebon-fs".to_string(), "rebon".to_string())),
        "the injection must win the name: {servers:?}"
    );
}

#[tokio::test]
async fn session_meta_rides_into_session_new_as_underscore_meta() {
    // `_meta` is how adapters take options the spec has no field for —
    // claude-agent-acp's `claudeCode.options.disallowedTools` in
    // particular, which is what makes the injected fs tools actually
    // get used. Verified against the real adapter on 2026-07-26.
    let (backend, connector) = backend(AgentScript::default());
    let backend =
        backend_owned(backend).with_session_meta(Some(std::collections::HashMap::from([(
            "claudeCode".to_string(),
            serde_json::json!({"options": {"disallowedTools": ["Write", "Edit"]}}),
        )])));

    backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();

    let news = connector.log.params_of("session/new");
    assert_eq!(
        news[0]["_meta"]["claudeCode"]["options"]["disallowedTools"],
        serde_json::json!(["Write", "Edit"]),
        "the meta object must arrive under the spec's `_meta` key"
    );
}

#[tokio::test]
async fn start_session_with_meta_merges_overlay_over_configured_meta() {
    // The overlay rides *on top of* the configured sessionMeta: sibling
    // keys survive, deeper objects merge key-by-key, and only the
    // overlay's leaves win. This is what lets the sub-agent pool ask
    // for a model without clobbering the user's disallowedTools recipe.
    let (backend, connector) = backend(AgentScript {
        new_session_ids: vec!["agent-sess-1".into(), "agent-sess-2".into()],
        ..AgentScript::default()
    });
    let backend =
        backend_owned(backend).with_session_meta(Some(std::collections::HashMap::from([(
            "claudeCode".to_string(),
            serde_json::json!({"options": {"disallowedTools": ["Write", "Edit"]}}),
        )])));

    backend
        .start_session_with_meta(
            AgentSessionSpec::new("host-1", "/tmp/work"),
            Some(std::collections::HashMap::from([
                (
                    "claudeCode".to_string(),
                    serde_json::json!({"options": {"model": "claude-opus-5"}}),
                ),
                ("modelHint".to_string(), serde_json::json!("claude-opus-5")),
            ])),
        )
        .await
        .unwrap();

    let news = connector.log.params_of("session/new");
    let meta = &news[0]["_meta"];
    assert_eq!(
        meta["claudeCode"]["options"]["disallowedTools"],
        serde_json::json!(["Write", "Edit"]),
        "configured leaves must survive the merge"
    );
    assert_eq!(meta["claudeCode"]["options"]["model"], "claude-opus-5");
    assert_eq!(meta["modelHint"], "claude-opus-5");

    // The overlay was for that session only: the plain trait path on
    // another session sends the configured meta untouched.
    backend
        .start_session(AgentSessionSpec::new("host-2", "/tmp/work"))
        .await
        .unwrap();
    let news = connector.log.params_of("session/new");
    assert!(news[1]["_meta"]["modelHint"].is_null());
    assert!(news[1]["_meta"]["claudeCode"]["options"]["model"].is_null());
}

#[tokio::test]
async fn session_start_counts_report_created_loaded_and_reused() {
    // Session reuse is this leg's efficiency signal; these counters
    // are the signal. One cold start, one reuse, then a
    // load-after-remember on a second host session.
    let (backend, _connector) = backend(AgentScript {
        load_session: true,
        load_succeeds: true,
        ..AgentScript::default()
    });

    backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();
    backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();
    assert_eq!(backend.session_start_counts(), (1, 0, 1));

    backend.remember_agent_session("host-2", "agent-sess-old");
    let start = backend
        .start_session(AgentSessionSpec::new("host-2", "/tmp/work"))
        .await
        .unwrap();
    assert_eq!(start.mode, SessionResumeMode::Loaded);
    assert_eq!(backend.session_start_counts(), (1, 1, 1));
}

#[tokio::test]
async fn shutdown_stops_the_agent_and_a_later_start_reconnects() {
    // A long-lived holder (the sub-agent pool) shuts its backends down
    // deliberately; needing the agent again later must reconnect
    // rather than fail on a stale slot.
    let (backend, connector) = backend(AgentScript::default());
    backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();
    assert_eq!(connector.connects.load(Ordering::Relaxed), 1);

    backend.shutdown().await;
    // Idempotent: a second shutdown with no live client is a no-op.
    backend.shutdown().await;

    let start = backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();
    assert_eq!(connector.connects.load(Ordering::Relaxed), 2);
    // The live binding died with the connection; without loadSession
    // the new connection starts cold — honestly reported as Created.
    assert_eq!(start.mode, SessionResumeMode::Created);
}

#[tokio::test]
async fn injected_servers_reach_session_load_too() {
    // Resuming after a restart re-sends the servers; an agent that
    // honours mcpServers on load keeps the tools across restarts.
    let (backend, connector) = backend(AgentScript {
        load_session: true,
        load_succeeds: true,
        ..AgentScript::default()
    });
    let backend =
        backend_owned(backend).with_injected_mcp_servers(vec![stdio_server("rebon-fs", "rebon")]);
    backend.remember_agent_session("host-1", "agent-sess-old");

    let start = backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();
    assert_eq!(start.mode, SessionResumeMode::Loaded);

    let loads = connector.log.params_of("session/load");
    assert_eq!(loads.len(), 1);
    assert_eq!(
        server_names(&loads[0]),
        vec![("rebon-fs".to_string(), "rebon".to_string())]
    );
}

// ── workspace cwd ─────────────────────────────────────────────────

#[tokio::test]
async fn without_a_workspace_cwd_the_host_session_directory_is_used() {
    let (backend, connector) = backend(AgentScript::default());

    backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();

    let news = connector.log.params_of("session/new");
    assert_eq!(news[0]["cwd"], serde_json::json!("/tmp/work"));
}

#[tokio::test]
async fn a_workspace_cwd_replaces_the_host_session_directory() {
    // The case this exists for: an agent reached over ssh, where the
    // host's cwd names a directory on the wrong machine.
    let (backend, connector) = backend(AgentScript::default());
    let backend = backend_owned(backend).with_workspace_cwd("/srv/app");

    backend
        .start_session(AgentSessionSpec::new("host-1", "C:\\dev\\local"))
        .await
        .unwrap();

    let news = connector.log.params_of("session/new");
    assert_eq!(news[0]["cwd"], serde_json::json!("/srv/app"));
}

#[tokio::test]
async fn a_workspace_cwd_applies_to_session_load_as_well_as_new() {
    // Load and create must name the same directory. If only one of
    // them were overridden, a resume would land the host session in a
    // different place on the agent's side than a cold start did.
    let (backend, connector) = backend(AgentScript {
        load_session: true,
        load_succeeds: true,
        ..AgentScript::default()
    });
    let backend = backend_owned(backend).with_workspace_cwd("/srv/app");
    backend.remember_agent_session("host-1", "agent-sess-old");

    let start = backend
        .start_session(AgentSessionSpec::new("host-1", "C:\\dev\\local"))
        .await
        .unwrap();
    assert_eq!(start.mode, SessionResumeMode::Loaded);

    let loads = connector.log.params_of("session/load");
    assert_eq!(loads[0]["cwd"], serde_json::json!("/srv/app"));
}

#[tokio::test]
async fn a_workspace_cwd_survives_a_failed_load_falling_through_to_new() {
    // The fall-through path builds `session/new` after `session/load`
    // has already been rejected; the override must still apply there,
    // or a remote session would cold-start in the local directory.
    let (backend, connector) = backend(AgentScript {
        load_session: true,
        load_succeeds: false,
        ..AgentScript::default()
    });
    let backend = backend_owned(backend).with_workspace_cwd("/srv/app");
    backend.remember_agent_session("host-1", "agent-sess-old");

    let start = backend
        .start_session(AgentSessionSpec::new("host-1", "C:\\dev\\local"))
        .await
        .unwrap();
    assert_eq!(start.mode, SessionResumeMode::Created);

    let news = connector.log.params_of("session/new");
    assert_eq!(news[0]["cwd"], serde_json::json!("/srv/app"));
}

/// Unwrap the `Arc` the `backend()` helper returns so the builder
/// method can move it — no other clone exists at this point.
fn backend_owned(backend: Arc<AcpAgentBackend>) -> AcpAgentBackend {
    Arc::try_unwrap(backend).unwrap_or_else(|_| panic!("the backend arc is not shared yet"))
}

// ── conversation handoff ──────────────────────────────────────────

/// Hands over a fixed digest and counts how often it was asked.
struct CountingHandoff {
    text: String,
    calls: Arc<AtomicUsize>,
}

impl crate::backend::HandoffProvider for CountingHandoff {
    fn handoff_text(&self, _host_session_id: &str) -> Option<String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Some(self.text.clone())
    }
}

fn prompt_texts(params: &serde_json::Value) -> Vec<String> {
    params["prompt"]
        .as_array()
        .expect("prompt blocks")
        .iter()
        .filter_map(|block| block["text"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_cold_agent_session_is_caught_up_once_and_only_on_the_wire() {
    // Switching agents mid-conversation used to drop the new agent
    // into an amnesiac session. It now gets the conversation so far —
    // prepended to the first prompt only, and never written to the
    // transcript, or the next handoff would quote the previous one.
    let root = journal_root("handoff");
    let journal = Arc::new(crate::journal::TranscriptJournal::new(
        root.path(),
        "/tmp/work",
        "host-1",
        "fake",
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let (backend, connector) = backend_with_journal(AgentScript::default(), journal);
    let backend = backend_owned(backend).with_handoff_provider(Arc::new(CountingHandoff {
        text: "<rebon-handoff>earlier: we fixed the uploader</rebon-handoff>".into(),
        calls: calls.clone(),
    }));

    backend.prompt(prompt_request("host-1")).await.unwrap();
    backend.prompt(prompt_request("host-1")).await.unwrap();

    let prompts = connector.log.params_of("session/prompt");
    assert_eq!(prompts.len(), 2);
    let first = prompt_texts(&prompts[0]);
    assert_eq!(
        first.len(),
        2,
        "the handoff rides in front of the user's message: {first:?}"
    );
    assert!(first[0].contains("we fixed the uploader"));
    assert_eq!(first[1], "hello");
    assert_eq!(
        prompt_texts(&prompts[1]),
        vec!["hello".to_string()],
        "the second turn is on the same agent session — no repeat"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    let rows = journal_rows(root.path(), "host-1");
    assert!(
        !rows
            .iter()
            .any(|(_, raw)| raw.to_string().contains("rebon-handoff")),
        "the handoff must stay off disk, or it compounds: {rows:?}"
    );
}

#[tokio::test]
async fn a_resumed_agent_session_is_not_caught_up() {
    // `session/load` means the agent still has the conversation;
    // handing it a summary of what it already knows wastes context
    // and invites it to answer the summary.
    let root = journal_root("handoff-load");
    let journal = Arc::new(crate::journal::TranscriptJournal::new(
        root.path(),
        "/tmp/work",
        "host-1",
        "fake",
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let (backend, connector) = backend_with_journal(
        AgentScript {
            load_session: true,
            load_succeeds: true,
            ..AgentScript::default()
        },
        journal,
    );
    let backend = backend_owned(backend).with_handoff_provider(Arc::new(CountingHandoff {
        text: "<rebon-handoff>not needed</rebon-handoff>".into(),
        calls: calls.clone(),
    }));
    backend.remember_agent_session("host-1", "agent-sess-old");

    backend.prompt(prompt_request("host-1")).await.unwrap();

    assert_eq!(connector.log.count("session/load"), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        prompt_texts(&connector.log.params_of("session/prompt")[0]),
        vec!["hello".to_string()]
    );
}

// ── steering ──────────────────────────────────────────────────────

fn steer_blocks(text: &str) -> Vec<rebon_types::ContentBlock> {
    vec![rebon_types::ContentBlock::Text(TextContent {
        text: text.to_string(),
        annotations: None,
    })]
}

#[tokio::test]
async fn an_agent_that_never_advertised_steering_is_not_asked_to() {
    // The fallback (queue until the turn ends) is always correct, so
    // this is `Unsupported`, not an error worth surfacing.
    let (backend, connector) = backend(AgentScript::default());
    backend
        .start_session(AgentSessionSpec::new("host-1", "/tmp/work"))
        .await
        .unwrap();

    let err = backend
        .steer("host-1", steer_blocks("also fix the tests"), "u-1")
        .await
        .expect_err("no advertisement, no steering");
    assert!(
        matches!(err, rebon_agent_core::AgentBackendError::Unsupported(_)),
        "{err:?}"
    );
    assert_eq!(
        connector.log.count("_session/steering"),
        0,
        "an unsupported agent must not even be asked"
    );
}

#[tokio::test]
async fn a_message_steered_into_a_running_turn_is_recorded_in_order() {
    // The whole point: the message reaches the agent mid-turn *and*
    // lands in history between the prompt and the answer, so a replay
    // reads the conversation the way it happened.
    let root = journal_root("steer-order");
    let journal = Arc::new(crate::journal::TranscriptJournal::new(
        root.path(),
        "/tmp/work",
        "host-1",
        "fake",
    ));
    let (backend, connector) = backend_with_journal(
        AgentScript {
            steering_supported: true,
            hold_prompt_for_steer: true,
            prompt_update_text: Some("working".into()),
            ..AgentScript::default()
        },
        journal,
    );
    let backend = Arc::new(backend_owned(backend));

    let turn = {
        let backend = backend.clone();
        tokio::spawn(async move { backend.prompt(prompt_request("host-1")).await })
    };

    // Wait for the turn to actually be in flight before steering.
    let steered = loop {
        if backend.agent_session_id("host-1").is_some() {
            match backend
                .steer(
                    "host-1",
                    steer_blocks("and also update the docs"),
                    "u-steer-1",
                )
                .await
            {
                Ok(outcome) => break outcome,
                Err(rebon_agent_core::AgentBackendError::UnknownSession(_)) => {}
                Err(err) => panic!("steer failed: {err:?}"),
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert_eq!(steered, rebon_agent_core::SteerOutcome::Injected);
    turn.await.expect("turn task").expect("turn succeeded");

    assert_eq!(connector.log.count("_session/steering"), 1);
    let rows = journal_rows(root.path(), "host-1");
    assert_eq!(rows.len(), 3, "prompt, steered message, answer: {rows:?}");
    assert_eq!(rows[0].0, "user");
    assert_eq!(rows[1].0, "user");
    assert_eq!(rows[1].1["uuid"], "u-steer-1", "the host's id is kept");
    assert_eq!(
        rows[1].1["message"]["content"][0]["text"],
        "and also update the docs"
    );
    assert_eq!(
        rows[1].1["queuedCommand"], true,
        "marked the same way the local engine marks a mid-turn message"
    );
    assert_eq!(rows[1].1["parentUuid"], rows[0].1["uuid"]);
    assert_eq!(rows[2].0, "assistant");
    assert_eq!(
        rows[2].1["parentUuid"], rows[1].1["uuid"],
        "the answer hangs off the steered message, keeping the chain unbroken"
    );
}

#[tokio::test]
async fn a_steer_that_missed_its_turn_is_cancelled_and_handed_back() {
    // `startedNewTurn` means the agent opened a turn nobody on our
    // side is awaiting — its output would stream into a session whose
    // publisher is gone. Cancel it and let the caller re-send.
    let root = journal_root("steer-missed");
    let journal = Arc::new(crate::journal::TranscriptJournal::new(
        root.path(),
        "/tmp/work",
        "host-1",
        "fake",
    ));
    let (backend, connector) = backend_with_journal(
        AgentScript {
            steering_supported: true,
            steering_outcome: "startedNewTurn",
            ..AgentScript::default()
        },
        journal,
    );

    backend.prompt(prompt_request("host-1")).await.unwrap();
    let outcome = backend
        .steer("host-1", steer_blocks("too late"), "u-late")
        .await
        .expect("the agent answered");

    assert_eq!(outcome, rebon_agent_core::SteerOutcome::TurnAlreadyOver);
    // `cancel` is a notification: the write has completed, but the
    // agent may not have read it yet.
    for _ in 0..100 {
        if connector.log.count("session/cancel") == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        connector.log.count("session/cancel"),
        1,
        "the turn the agent started on its own must be cancelled"
    );
    let rows = journal_rows(root.path(), "host-1");
    assert!(
        !rows.iter().any(|(_, raw)| raw["uuid"] == "u-late"),
        "a message handed back must not also be recorded: {rows:?}"
    );
}

#[tokio::test]
async fn steering_a_session_the_agent_never_started_is_an_error() {
    let (backend, connector) = backend(AgentScript {
        steering_supported: true,
        ..AgentScript::default()
    });
    // Force the handshake so the capability is known, but never start
    // a session for `host-1`.
    backend
        .start_session(AgentSessionSpec::new("other", "/tmp/work"))
        .await
        .unwrap();

    let err = backend
        .steer("host-1", steer_blocks("hello"), "u-1")
        .await
        .expect_err("no session, nothing to steer");
    assert!(
        matches!(err, rebon_agent_core::AgentBackendError::UnknownSession(_)),
        "{err:?}"
    );
    assert_eq!(connector.log.count("_session/steering"), 0);
}
