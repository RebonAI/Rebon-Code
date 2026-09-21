//! End-to-end tests that drive `rebon-core` through the `rebon-acp`
//! server.
//!
//! These used to live inside `rebon-core`'s own unit tests, which forced
//! the engine to depend on the ACP server crate. The engine no longer does,
//! so the tests moved up to `rebon-harness` — the lowest crate that already
//! depends on both sides. The assertions are unchanged; only the test-local
//! helpers were copied over from the engine's test module.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{duplex, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};

use rebon_acp::{serve_with_publishers, DefaultHandler, RequestHandler};
use rebon_agent_core::prompt_executor::PromptExecutor;
use rebon_agent_core::{
    ChannelPermissionRequestPublisher, ChannelSessionUpdatePublisher, MemorySessionUpdatePublisher,
    SessionUpdatePublisher,
};
use rebon_api::events::{ContentBlockDelta, ContentBlockStart, MessageDeltaFields, StreamEvent};
use rebon_api::mock::MockModelClient;
use rebon_api::{ModelClient, StopReason, Usage};
use rebon_core::query::EngineQueryExecutor;
use rebon_core::{AcpPermissionBroker, Engine, PermissionBroker};
use rebon_permissions::types::PermissionRuleSource;
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    PermissionDecision, PermissionRequest, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};
use rebon_types::SessionUpdate;

// ── Test-local helpers (copied verbatim from the engine test module) ──

fn temp_projects_root(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("rebon-core-executor-{tag}-"))
        .tempdir()
        .unwrap()
}

fn build_engine_with(tool: Arc<dyn Tool>) -> Arc<Engine> {
    struct ApproveBroker;
    #[async_trait::async_trait]
    impl PermissionBroker for ApproveBroker {
        async fn resolve(
            &self,
            tool: &dyn Tool,
            input: Value,
            context: &ToolContext,
            decision: PermissionDecision,
        ) -> Result<Value, ToolError> {
            let _ = decision;
            tool.call(input, context).await
        }
    }
    let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
    engine.register_tool(tool);
    Arc::new(engine)
}

fn message_start(id: &str) -> StreamEvent {
    StreamEvent::MessageStart {
        message_id: id.into(),
        model: "mock".into(),
        usage: Usage {
            input_tokens: 4,
            ..Default::default()
        },
    }
}

fn text_turn(id: &str, text: &str) -> Vec<StreamEvent> {
    vec![
        message_start(id),
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::TextDelta { text: text.into() },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage {
                    output_tokens: 4,
                    ..Default::default()
                },
            },
        },
        StreamEvent::MessageStop,
    ]
}

fn tool_turn(id: &str, tool_name: &str, tool_id: &str, input_json: &str) -> Vec<StreamEvent> {
    vec![
        message_start(id),
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: tool_id.into(),
                name: tool_name.into(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: input_json.into(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage {
                    output_tokens: 6,
                    ..Default::default()
                },
            },
        },
        StreamEvent::MessageStop,
    ]
}

/// Records the inputs it was called with and returns a canned response.
struct RecordingTool {
    name: String,
    calls: Mutex<Vec<Value>>,
    response: Value,
}

impl RecordingTool {
    fn new(name: &str, response: Value) -> Self {
        Self {
            name: name.into(),
            calls: Mutex::new(Vec::new()),
            response,
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn id(&self) -> ToolId {
        ToolId::new(self.name.clone())
    }

    fn description(&self) -> &str {
        "recording tool"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "additionalProperties": true
        })
    }

    async fn validate_input(
        &self,
        _input: &Value,
        _context: &ToolContext,
    ) -> Result<ValidationOutcome, ToolError> {
        Ok(ValidationOutcome::valid())
    }

    async fn check_permissions(
        &self,
        _input: &Value,
        _context: &ToolContext,
    ) -> Result<PermissionDecision, ToolError> {
        Ok(PermissionDecision::allow(Value::Null))
    }

    async fn call(&self, input: Value, _context: &ToolContext) -> Result<Value, ToolError> {
        self.calls.lock().unwrap().push(input);
        Ok(self.response.clone())
    }
}

/// Always asks for permission before running.
struct AskingTool {
    name: String,
    calls: Mutex<usize>,
}

impl AskingTool {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            calls: Mutex::new(0),
        }
    }
    fn call_count(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl Tool for AskingTool {
    fn id(&self) -> ToolId {
        ToolId::new(self.name.clone())
    }
    fn description(&self) -> &str {
        "permission-asking tool"
    }
    fn input_schema(&self) -> ToolInputSchema {
        json!({ "type": "object", "additionalProperties": true })
    }
    async fn validate_input(
        &self,
        _input: &Value,
        _context: &ToolContext,
    ) -> Result<ValidationOutcome, ToolError> {
        Ok(ValidationOutcome::valid())
    }
    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> Result<PermissionDecision, ToolError> {
        Ok(PermissionDecision::ask(
            PermissionRequest::new("Run asker", "Asking for approval").with_options([
                "allow_once",
                "allow_always",
                "reject_once",
            ]),
            Some(input.clone()),
        ))
    }
    async fn call(&self, _input: Value, _context: &ToolContext) -> Result<Value, ToolError> {
        *self.calls.lock().unwrap() += 1;
        Ok(json!({ "ran": true }))
    }
}

/// A tool whose permission check always asks, used by the reverse-RPC
/// round-trip test below.
struct AskPermissionTool;

#[async_trait]
impl Tool for AskPermissionTool {
    fn id(&self) -> ToolId {
        ToolId::new("AskPermission")
    }

    fn description(&self) -> &str {
        "test tool"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "additionalProperties": true
        })
    }

    async fn validate_input(
        &self,
        _input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        Ok(ValidationOutcome::valid())
    }

    async fn check_permissions(
        &self,
        _input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        Ok(PermissionDecision::ask(
            PermissionRequest::new("Need approval", "Ask first"),
            None,
        ))
    }

    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        Ok(input)
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn acp_permission_broker_round_trips_selected_response() {
    let (mut client_to_server, server_in, server_out, mut client_from_server) = {
        let (client_to_server, server_reader) = duplex(4096);
        let (server_writer, client_from_server) = duplex(4096);
        (
            client_to_server,
            server_reader,
            server_writer,
            client_from_server,
        )
    };
    let (publisher, rx) = ChannelPermissionRequestPublisher::new();

    let serve_task = tokio::spawn(async move {
        serve_with_publishers(
            server_in,
            server_out,
            DefaultHandler::default(),
            None,
            Some(rx),
        )
        .await
        .unwrap();
    });

    let mut engine = Engine::new()
        .with_permission_broker(Arc::new(AcpPermissionBroker::new(publisher, "sess-1")));
    engine.register_tool(Arc::new(AskPermissionTool));

    let invoke_task = tokio::spawn(async move {
        engine
            .invoke_tool(
                "AskPermission",
                json!({ "a": 1 }),
                &ToolContext::for_tool_use("toolu_01"),
            )
            .await
            .unwrap()
    });

    let mut buf = vec![0u8; 2048];
    let n = client_from_server.read(&mut buf).await.unwrap();
    let line = std::str::from_utf8(&buf[..n]).unwrap().trim();
    let request: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(request["method"], "session/request_permission");
    assert_eq!(request["params"]["sessionId"], "sess-1");
    assert_eq!(request["params"]["toolCall"]["toolCallId"], "toolu_01");
    assert_eq!(request["params"]["title"], "Need approval");
    assert_eq!(request["params"]["message"], "Ask first");

    let id = request["id"].as_i64().unwrap();
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "outcome": {
                "outcome": "selected",
                "optionId": "allow_once"
            }
        }
    });
    client_to_server
        .write_all(serde_json::to_string(&response).unwrap().as_bytes())
        .await
        .unwrap();
    client_to_server.write_all(b"\n").await.unwrap();
    drop(client_to_server);

    let out = invoke_task.await.unwrap();
    serve_task.await.unwrap();
    assert_eq!(out, json!({ "a": 1 }));
}

#[tokio::test]
async fn acp_session_prompt_invokes_engine_query_executor_end_to_end() {
    let tool = Arc::new(RecordingTool::new(
        "Read",
        json!({"contents": "end-to-end"}),
    ));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_1",
        "{\"path\":\"a.rs\"}",
    ));
    client.push_turn(text_turn("msg_done", "all set"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("acp_e2e");
    let projects_root = projects_root_dir.path();
    let executor = Arc::new(
        EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
            .with_max_iterations(5),
    );
    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let handler = DefaultHandler::default()
        .with_prompt_executor(executor)
        .with_update_publisher(publisher.clone() as Arc<dyn SessionUpdatePublisher>);

    // Drive the handler directly (no stdio).
    // 1. initialize
    let init_resp = handler
        .handle_request(
            "initialize",
            Some(json!({
                "protocolVersion": 1,
                "clientCapabilities": {}
            })),
        )
        .await
        .unwrap();
    assert_eq!(init_resp["protocolVersion"], 1);

    // 2. session/new
    let new_resp = handler
        .handle_request(
            "session/new",
            Some(json!({
                "cwd": "/tmp/acp-e2e",
                "mcpServers": []
            })),
        )
        .await
        .unwrap();
    let session_id = new_resp["sessionId"].as_str().unwrap().to_string();

    // 3. session/prompt — this flows through the EngineQueryExecutor.
    let prompt_resp = handler
        .handle_request(
            "session/prompt",
            Some(json!({
                "sessionId": session_id,
                "prompt": [
                    { "type": "text", "text": "read a.rs" }
                ]
            })),
        )
        .await
        .unwrap();

    // Response should carry a real stop reason (end_turn).
    assert_eq!(prompt_resp["stopReason"], "end_turn");

    // The real tool was invoked, and the publisher saw streaming
    // text deltas from the final assistant turn.
    assert_eq!(tool.call_count(), 1);
    let snap = publisher.snapshot();
    assert!(snap
        .iter()
        .any(|u| matches!(&u.update, SessionUpdate::AgentMessageChunk { .. })));

    // The transcript file was written to the per-session cwd.
    let transcript_path =
        rebon_session::transcript_file_path(projects_root, "/tmp/acp-e2e", &session_id);
    assert!(transcript_path.exists(), "transcript should be persisted");
}

// Hold TestConfigHome's environment lock for this guard's entire lifetime.
struct PolicyEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl PolicyEnv {
    fn clear() -> Self {
        Self(
            ["REBON_ALLOW_RULES", "REBON_DENY_RULES"]
                .into_iter()
                .map(|key| {
                    let old = std::env::var_os(key);
                    std::env::remove_var(key);
                    (key, old)
                })
                .collect(),
        )
    }
}
impl Drop for PolicyEnv {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

async fn wire_policy_prompt(
    writer: &mut DuplexStream,
    reader: &mut BufReader<DuplexStream>,
    sid: &str,
    choice: &str,
) -> usize {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let request = json!({"jsonrpc":"2.0","id":"prompt","method":"session/prompt",
            "params":{"sessionId":sid,"prompt":[{"type":"text","text":"run ls"}]}});
        writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut asks = 0;
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == "prompt" {
                assert!(message.get("error").is_none(), "{message}");
                return asks;
            }
            assert_eq!(message["method"], "session/request_permission", "{message}");
            assert_eq!(message["params"]["sessionId"], sid);
            assert!(
                message["params"]["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|option| option["optionId"] == choice),
                "client must choose an offered option: choice={choice}, request={message}"
            );
            asks += 1;
            let response = json!({"jsonrpc":"2.0","id":message["id"],
                "result":{"outcome":{"outcome":"selected","optionId":choice}}});
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
    })
    .await
    .expect("ACP prompt must finish")
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_allow_always_same_turn_and_persistence_failure_are_session_local() {
    let _home = rebon_tool::tasks::test_support::TestConfigHome::new("acp-live-engine");
    let _env = PolicyEnv::clear();
    for persistence_fails in [false, true] {
        let project = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let asking = Arc::new(AskingTool::new("Bash"));
        let client = Arc::new(MockModelClient::new());
        let (publisher, permission_rx) = ChannelPermissionRequestPublisher::new();
        let (handler, resolve) =
            rebon_harness::with_acp_session_policies(DefaultHandler::default());
        let executor = EngineQueryExecutor::new(
            build_engine_with(asking.clone()),
            client.clone(),
            projects.path(),
            "mock",
        )
        .with_max_iterations(4)
        .with_policy_store_resolver(resolve);
        let handler = handler
            .with_prompt_executor(Arc::new(executor))
            .with_permission_publisher(publisher);
        handler
            .handle_request(
                "initialize",
                Some(json!({"protocolVersion":1,"clientCapabilities":{}})),
            )
            .await
            .unwrap();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let response = handler
                .handle_request(
                    "session/new",
                    Some(json!({"cwd":project.path(),"mcpServers":[]})),
                )
                .await
                .unwrap();
            ids.push(response["sessionId"].as_str().unwrap().to_owned());
        }
        // Both real sessions exist before A accepts its grant; B is still idle.
        let settings = project.path().join(".rebon/settings.json");
        if persistence_fails {
            std::fs::create_dir_all(&settings).unwrap();
        }
        let (mut writer, server_in) = duplex(8192);
        let (server_out, reader) = duplex(8192);
        let mut reader = BufReader::new(reader);
        let server = tokio::spawn(serve_with_publishers(
            server_in,
            server_out,
            handler,
            None,
            Some(permission_rx),
        ));
        for (index, sid, tool_rounds, choice, expected_asks) in [
            (0, &ids[0], 2, "allow_always", 1),
            (1, &ids[0], 1, "allow_always", 0),
            (2, &ids[1], 1, "allow_once", 1),
            (3, &ids[1], 1, "allow_once", 1),
        ] {
            for round in 0..tool_rounds {
                let id = format!("tool-{index}-{round}");
                client.push_turn(tool_turn(&id, "Bash", &id, r#"{"command":"ls -la"}"#));
            }
            client.push_turn(text_turn(&format!("done-{index}"), "done"));
            assert_eq!(
                wire_policy_prompt(&mut writer, &mut reader, sid, choice).await,
                expected_asks,
                "persistence_fails={persistence_fails}, prompt={index}"
            );
        }
        assert_eq!(asking.call_count(), 5);
        if persistence_fails {
            assert!(settings.is_dir());
        } else {
            let settings: Value =
                serde_json::from_slice(&std::fs::read(settings).unwrap()).unwrap();
            assert_eq!(settings["permissions"]["allow"], json!(["Bash(ls -la)"]));
        }
        drop(writer);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fixed_policy_store_retains_same_turn_live_grants() {
    let _home = rebon_tool::tasks::test_support::TestConfigHome::new("fixed-live-policy");
    let _env = PolicyEnv::clear();
    let project = tempfile::tempdir().unwrap();
    let projects = tempfile::tempdir().unwrap();
    let asking = Arc::new(AskingTool::new("Bash"));
    let client = Arc::new(MockModelClient::new());
    for id in ["first", "second"] {
        client.push_turn(tool_turn(id, "Bash", id, r#"{"command":"ls -la"}"#));
    }
    client.push_turn(text_turn("done", "done"));
    let policy = rebon_core::policy::PolicyStore::new();
    let executor = EngineQueryExecutor::new(
        build_engine_with(asking.clone()),
        client,
        projects.path(),
        "mock",
    )
    .with_max_iterations(4)
    .with_policy_store(policy.clone());
    let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
    let handler = DefaultHandler::default()
        .with_prompt_executor(Arc::new(executor))
        .with_permission_publisher(publisher);
    handler
        .handle_request(
            "initialize",
            Some(json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let response = handler
        .handle_request(
            "session/new",
            Some(json!({"cwd":project.path(),"mcpServers":[]})),
        )
        .await
        .unwrap();
    let sid = response["sessionId"].as_str().unwrap().to_owned();
    let prompt = tokio::spawn(async move {
        handler
            .handle_request(
                "session/prompt",
                Some(json!({"sessionId":sid,"prompt":[{"type":"text","text":"run"}]})),
            )
            .await
    });
    let outbound = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    // The TUI still holds this fixed store and updates it while a turn is waiting.
    policy.allow("Bash(ls -la)", PermissionRuleSource::ProjectSettings);
    outbound
        .response_tx
        .send(
            serde_json::from_value(json!({"jsonrpc":"2.0","id":outbound.request_id,
        "result":{"outcome":{"outcome":"selected","optionId":"allow_always"}}}))
            .unwrap(),
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), prompt)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(rx.try_recv().is_err());
    assert_eq!(asking.call_count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_two_sessions_load_only_their_own_workspace_policy() {
    let _home = rebon_tool::tasks::test_support::TestConfigHome::new("acp-engine-scopes");
    let _env = PolicyEnv::clear();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    std::fs::create_dir(a.path().join(".rebon")).unwrap();
    std::fs::write(
        a.path().join(".rebon/settings.json"),
        r#"{"permissions":{"allow":["Bash(ls:*)"]}}"#,
    )
    .unwrap();
    let asking = Arc::new(AskingTool::new("Bash"));
    let client = Arc::new(MockModelClient::new());
    for id in ["a", "b"] {
        client.push_turn(tool_turn(id, "Bash", id, r#"{"command":"ls -la"}"#));
        client.push_turn(text_turn("done", "done"));
    }
    let projects = tempfile::tempdir().unwrap();
    let (publisher, mut permission_rx) = ChannelPermissionRequestPublisher::new();
    let (handler, resolve) = rebon_harness::with_acp_session_policies(DefaultHandler::default());
    let executor = EngineQueryExecutor::new(
        build_engine_with(asking.clone()),
        client,
        projects.path(),
        "mock",
    )
    .with_max_iterations(3)
    .with_policy_store_resolver(resolve);
    let handler = handler
        .with_prompt_executor(Arc::new(executor))
        .with_permission_publisher(publisher);
    handler
        .handle_request(
            "initialize",
            Some(json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    for (index, cwd) in [a.path(), b.path()].into_iter().enumerate() {
        let response = handler
            .handle_request("session/new", Some(json!({"cwd":cwd,"mcpServers":[]})))
            .await
            .unwrap();
        let sid = response["sessionId"].as_str().unwrap().to_owned();
        let h = handler.clone();
        let prompt_sid = sid.clone();
        let task = tokio::spawn(async move {
            h.handle_request(
                "session/prompt",
                Some(json!({"sessionId":prompt_sid,"prompt":[{"type":"text","text":"run ls"}]})),
            )
            .await
        });
        if index == 1 {
            let outbound =
                tokio::time::timeout(std::time::Duration::from_secs(5), permission_rx.recv())
                    .await
                    .expect("session B must ask, not inherit session A's project allow")
                    .unwrap();
            assert_eq!(outbound.params.session_id, sid);
            outbound
                .response_tx
                .send(
                    serde_json::from_value(json!({"jsonrpc":"2.0","id":outbound.request_id,
                "result":{"outcome":{"outcome":"selected","optionId":"reject_once"}}}))
                    .unwrap(),
                )
                .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(permission_rx.try_recv().is_err());
    }
    assert_eq!(asking.call_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_policy_resolver_error_never_falls_back_to_fixed_store() {
    let _home = rebon_tool::tasks::test_support::TestConfigHome::new("acp-resolver-error");
    let _env = PolicyEnv::clear();
    let project = tempfile::tempdir().unwrap();
    let projects = tempfile::tempdir().unwrap();
    let asking = Arc::new(AskingTool::new("Read"));
    let client = Arc::new(MockModelClient::new());
    client.push_turn(tool_turn("tool", "Read", "tool", "{}"));
    client.push_turn(text_turn("done", "done"));
    let policy = rebon_core::policy::PolicyStore::new();
    policy.allow("Read", PermissionRuleSource::ProjectSettings);
    let expected_cwd = project.path().to_string_lossy().into_owned();
    let executor = EngineQueryExecutor::new(
        build_engine_with(asking.clone()),
        client,
        projects.path(),
        "mock",
    )
    .with_policy_store(policy)
    .with_policy_store_resolver(Arc::new(move |sid, cwd| {
        assert!(!sid.is_empty());
        assert_eq!(cwd, expected_cwd);
        Err("test authoritative policy unavailable".into())
    }));
    let handler = DefaultHandler::default().with_prompt_executor(Arc::new(executor));
    handler
        .handle_request(
            "initialize",
            Some(json!({"protocolVersion":1,"clientCapabilities":{}})),
        )
        .await
        .unwrap();
    let response = handler
        .handle_request(
            "session/new",
            Some(json!({"cwd":project.path(),"mcpServers":[]})),
        )
        .await
        .unwrap();
    let error = handler
        .handle_request(
            "session/prompt",
            Some(json!({"sessionId":response["sessionId"],
        "prompt":[{"type":"text","text":"run"}]})),
        )
        .await
        .unwrap_err();
    assert!(error
        .message
        .contains("test authoritative policy unavailable"));
    assert_eq!(asking.call_count(), 0);
}

#[tokio::test]
async fn acp_session_prompt_auto_allows_when_policy_rule_matches() {
    // Engine + ask-returning Bash-like tool.
    let asking = Arc::new(AskingTool::new("Bash"));
    let engine = build_engine_with(asking.clone() as Arc<dyn Tool>);

    // Mock client: turn 1 = tool call, turn 2 = final text.
    let client = MockModelClient::new();
    client.push_turn(vec![
        StreamEvent::MessageStart {
            message_id: "m1".into(),
            model: "mock".into(),
            usage: Usage::default(),
        },
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: "toolu_1".into(),
                name: "Bash".into(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: "{\"command\":\"ls -la\"}".into(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage::default(),
            },
        },
        StreamEvent::MessageStop,
    ]);
    client.push_turn(text_turn("msg_done", "auto-allowed by rule"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    // Pre-seed a matching allow rule.
    let policy = rebon_core::policy::PolicyStore::new();
    policy.allow("Bash(ls:*)", PermissionRuleSource::UserSettings);

    let projects_root_dir = temp_projects_root("policy_auto_allow");
    let projects_root = projects_root_dir.path();
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_max_iterations(3)
        .with_policy_store(policy);

    let (update_publisher, _update_rx) = ChannelSessionUpdatePublisher::new();
    let (permission_publisher, mut permission_rx) = ChannelPermissionRequestPublisher::new();

    let handler = DefaultHandler::default()
        .with_prompt_executor(Arc::new(executor) as Arc<dyn PromptExecutor>)
        .with_update_publisher(Arc::new(update_publisher) as Arc<dyn SessionUpdatePublisher>)
        .with_permission_publisher(permission_publisher);

    handler
        .handle_request(
            "initialize",
            Some(json!({ "protocolVersion": 1, "clientCapabilities": {} })),
        )
        .await
        .unwrap();
    let new_resp = handler
        .handle_request(
            "session/new",
            Some(json!({ "cwd": "/tmp/policy-e2e", "mcpServers": [] })),
        )
        .await
        .unwrap();
    let session_id = new_resp["sessionId"].as_str().unwrap().to_string();

    let prompt_resp = handler
        .handle_request(
            "session/prompt",
            Some(json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "please run ls" }]
            })),
        )
        .await
        .unwrap();

    // Tool ran exactly once without ANY permission RPC delivery
    // on our receiver — proving the rules layer short-circuited
    // the ACP ask path.
    assert_eq!(prompt_resp["stopReason"], "end_turn");
    assert_eq!(asking.call_count(), 1);
    assert!(
        permission_rx.try_recv().is_err(),
        "no permission RPC should have been sent when a matching allow rule exists"
    );
}

#[tokio::test]
async fn acp_session_prompt_routes_tool_permission_through_reverse_rpc_publisher() {
    // Build engine + ask-returning tool.
    let asking = Arc::new(AskingTool::new("Asker"));
    let engine = build_engine_with(asking.clone() as Arc<dyn Tool>);

    // Mock client: turn 1 = tool call, turn 2 = final text.
    let client = MockModelClient::new();
    client.push_turn(vec![
        StreamEvent::MessageStart {
            message_id: "m1".into(),
            model: "mock".into(),
            usage: Usage::default(),
        },
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: "toolu_1".into(),
                name: "Asker".into(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: "{}".into(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage::default(),
            },
        },
        StreamEvent::MessageStop,
    ]);
    client.push_turn(text_turn("msg_done", "permission granted"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("permission_reverse_rpc");
    let projects_root = projects_root_dir.path();
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_max_iterations(3);

    // Real reverse-RPC publisher + channel.
    let (update_publisher, _update_rx) = ChannelSessionUpdatePublisher::new();
    let (permission_publisher, mut permission_rx) = ChannelPermissionRequestPublisher::new();

    let handler = DefaultHandler::default()
        .with_prompt_executor(Arc::new(executor) as Arc<dyn PromptExecutor>)
        .with_update_publisher(Arc::new(update_publisher) as Arc<dyn SessionUpdatePublisher>)
        .with_permission_publisher(permission_publisher);

    // Initialize + session/new + kick off session/prompt.
    handler
        .handle_request(
            "initialize",
            Some(json!({ "protocolVersion": 1, "clientCapabilities": {} })),
        )
        .await
        .unwrap();
    let new_resp = handler
        .handle_request(
            "session/new",
            Some(json!({ "cwd": "/tmp/perm-e2e", "mcpServers": [] })),
        )
        .await
        .unwrap();
    let session_id = new_resp["sessionId"].as_str().unwrap().to_string();

    // Spawn session/prompt concurrently — it will block on the
    // reverse-RPC waiting for the client's approval.
    let handler_clone = handler.clone();
    let session_id_clone = session_id.clone();
    let prompt_task = tokio::spawn(async move {
        handler_clone
            .handle_request(
                "session/prompt",
                Some(json!({
                    "sessionId": session_id_clone,
                    "prompt": [{ "type": "text", "text": "please run Asker" }]
                })),
            )
            .await
    });

    // Wait for the permission request to land on our receiver,
    // then "approve" it by sending back a selected outcome.
    let outbound = tokio::time::timeout(std::time::Duration::from_secs(5), permission_rx.recv())
        .await
        .expect("timed out waiting for permission request")
        .expect("permission channel closed prematurely");
    assert_eq!(outbound.params.session_id, session_id);
    assert_eq!(outbound.params.tool_call.tool_call_id, "toolu_1");

    // Respond via the pending-response oneshot the server
    // registered on our behalf.
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": outbound.request_id,
        "result": {
            "outcome": {
                "outcome": "selected",
                "optionId": "allow_once"
            }
        }
    });
    let wire_response: rebon_proto::JsonRpcResponse = serde_json::from_value(response).unwrap();
    outbound.response_tx.send(wire_response).unwrap();

    // The prompt task should now unblock and return.
    let prompt_resp = prompt_task
        .await
        .expect("prompt task panicked")
        .expect("prompt handler errored");
    assert_eq!(prompt_resp["stopReason"], "end_turn");

    // The tool actually ran once — proving the permission broker
    // approved it and dispatched the call.
    assert_eq!(asking.call_count(), 1);
}
