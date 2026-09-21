//! The eight real MCP dispatch tests formerly owned by `rebon-core`'s query module.
//! Metadata comes from the plugin provider; execution goes through the typed seat.
use super::*;
use async_trait::async_trait;
use rebon_agent_core::{PromptExecutor, PromptRequest};
use rebon_api::{
    ContentBlockDelta, ContentBlockStart, CreateMessageRequest, MessageDeltaFields,
    MockModelClient, StopReason, StreamEvent, Usage,
};
use rebon_core::query::{CancelToken, EngineQueryExecutor};
use rebon_core::tool_seat::ToolSeat;
use rebon_core::{Engine, PermissionBroker};
use rebon_kernel::Kernel;
use rebon_tool::{McpClient, McpToolDefinition, Tool, ToolContext, ToolFilter, ToolSearchIndex};
use rebon_tools_core::{PermissionDecision, ToolError};
use serde_json::{json, Value};

struct AllowAllBroker;
#[async_trait]
impl PermissionBroker for AllowAllBroker {
    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        _decision: PermissionDecision,
    ) -> Result<Value, ToolError> {
        tool.call(input, context).await
    }
}

struct TestHost {
    _kernel: Arc<Kernel>,
    owner: Arc<lifecycle::RuntimeOwner>,
}

fn test_engine() -> (TestHost, Context, Arc<Engine>) {
    let kernel = Kernel::new();
    kernel
        .context()
        .provide::<ToolSeatService>(ToolSeat::new())
        .unwrap();
    let plugin = kernel.context().fork("mcp-test");
    let owner = Arc::new(lifecycle::RuntimeOwner::default());
    McpPlugin.apply_with_owner(&plugin, &owner).unwrap();
    let mut engine = Engine::with_builtin_tools().with_permission_broker(Arc::new(AllowAllBroker));
    engine.register_tool(Arc::new(rebon_tool::InvokeDeferredTool));
    engine.register_tool(Arc::new(rebon_tool::ToolSearchTool));
    assert!(engine.attach_upstream_tool_context(kernel.context().clone()));
    (
        TestHost {
            _kernel: kernel,
            owner,
        },
        plugin,
        Arc::new(engine),
    )
}

async fn collect_mcp_tool_definitions(client: &dyn McpClient) -> Vec<(String, McpToolDefinition)> {
    let mut definitions = Vec::new();
    for server in client.server_names() {
        if let Some(mut tools) = client.list_tool_definitions(&server).await {
            tools.sort_by(|a, b| a.name.cmp(&b.name));
            definitions.extend(tools.into_iter().map(|tool| (server.clone(), tool)));
        }
    }
    definitions
}

/// Exercise the public executor, not a copy of its tool projection. This calls
/// the plugin's session factory, installs the typed turn seat, and captures the
/// exact model requests after production policy/deferred projection.
async fn execute_prompt(
    host: &TestHost,
    engine: Arc<Engine>,
    client: Arc<dyn McpClient>,
    mock: &MockModelClient,
    filter: Option<ToolFilter>,
    mcp_servers: Vec<rebon_proto::McpServerConfig>,
) -> Vec<CreateMessageRequest> {
    let root = tempfile::tempdir().unwrap();
    let mut executor =
        EngineQueryExecutor::new(engine, Arc::new(mock.clone()), root.path(), "mock")
            .with_mcp_client(client)
            .with_max_iterations(6);
    if let Some(filter) = filter {
        executor = executor.with_tool_filter(filter);
    }
    let outcome = executor
        .execute(PromptRequest {
            user_prompt: None,
            effort_is_session_default: false,
            session_id: "mcp-production-execution".into(),
            cwd: root.path().to_string_lossy().into_owned(),
            prompt: vec![
                serde_json::from_value(json!({"type":"text","text":"inspect MCP"})).unwrap(),
            ],
            mcp_servers,
            update_publisher: None,
            permission_publisher: None,
            cancel: CancelToken::new(),
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
        .unwrap();
    host.owner.shutdown().await;
    assert_eq!(outcome.stop_reason, rebon_proto::StopReason::EndTurn);
    mock.captured_requests()
}

fn tool_result(requests: &[CreateMessageRequest], id: &str) -> Value {
    let messages = serde_json::to_value(&requests.last().unwrap().messages).unwrap();
    let result = messages
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == id)
        .expect("real dispatch must return a tool result to the model");
    assert_ne!(result["is_error"], true, "{result}");
    let text = if let Some(text) = result["content"].as_str() {
        text.to_string()
    } else {
        result["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<String>()
    };
    serde_json::from_str(&text).unwrap()
}

fn assert_tool_result(requests: &[CreateMessageRequest], id: &str, key: &str, expected: Value) {
    let value = tool_result(requests, id);
    assert_eq!(value["content"][key], expected, "{value}");
}

fn deferred_index(
    engine: &Engine,
    definitions: &[(String, McpToolDefinition)],
) -> Arc<ToolSearchIndex> {
    Arc::new(
        engine
            .build_tool_search_index_for_policy(None)
            .with_entries(
                definitions
                    .iter()
                    .filter(|(_, definition)| definition.should_defer())
                    .map(|(server, definition)| {
                        ToolSearchIndex::entry_from_parts(
                            rebon_tool::build_mcp_tool_name(server, &definition.name),
                            definition.description.clone(),
                            definition.input_schema.clone(),
                            definition.search_hint.clone(),
                        )
                    }),
            ),
    )
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_proxy_tools_are_announced_and_dispatch_directly() {
    let (host, _plugin, engine) = test_engine();
    let client = Arc::new(InMemoryMcpClient::new());
    client.register_tool_definition(
        "patent-search",
        McpToolDefinition {
            name: "search_patents".into(),
            description: "Search patents".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
            read_only: true,
            destructive: false,
            open_world: true,
            search_hint: Some("patent search".into()),
            always_load: true,
        },
        json!({"ok": true}),
    );
    let mock = MockModelClient::new();
    mock.push_turn(tool_turn("msg_1", "Mcp", "toolu_1",
        r#"{"server":"patent-search","name":"search_patents","arguments":{"query":"wireless charging"}}"#));
    mock.push_turn(text_turn("msg_2", "done"));
    let requests = execute_prompt(&host, engine, client.clone(), &mock, None, Vec::new()).await;
    let tools = requests[0]
        .tools
        .iter()
        .filter(|tool| tool.name.starts_with("mcp__"))
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "mcp__patent-search__search_patents");
    assert_eq!(tools[0].input_schema["required"][0], "query");
    assert_tool_result(&requests, "toolu_1", "ok", json!(true));
    assert_eq!(client.calls()[0].arguments["query"], "wireless charging");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_accepts_announced_mcp_proxy_tool_names() {
    let (host, _plugin, engine) = test_engine();
    let client = Arc::new(InMemoryMcpClient::new());
    client.register_tool_definition(
        "patent-search",
        McpToolDefinition {
            name: "search_patents".into(),
            description: "Search patents".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
            read_only: true,
            destructive: false,
            open_world: true,
            search_hint: None,
            always_load: true,
        },
        json!({"ok": true}),
    );
    let mock = MockModelClient::new();
    mock.push_turn(tool_turn(
        "msg_1",
        "mcp__patent-search__search_patents",
        "toolu_1",
        r#"{"query":"wireless charging"}"#,
    ));
    mock.push_turn(text_turn("msg_2", "done"));
    let requests = execute_prompt(&host, engine, client.clone(), &mock, None, Vec::new()).await;
    assert_eq!(requests.len(), 2);
    assert!(requests[0]
        .tools
        .iter()
        .any(|tool| tool.name == "mcp__patent-search__search_patents"));
    assert_tool_result(&requests, "toolu_1", "ok", json!(true));
    assert_eq!(client.calls()[0].arguments["query"], "wireless charging");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoke_deferred_tool_accepts_discovered_mcp_proxy_names() {
    let (host, _plugin, engine) = test_engine();
    let client = Arc::new(InMemoryMcpClient::new());
    client.register_tool_definition(
        "patent-search",
        McpToolDefinition {
            name: "search_patents".into(),
            description: "Search patents".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
            read_only: true,
            destructive: false,
            open_world: true,
            search_hint: None,
            always_load: false,
        },
        json!({"ok": true}),
    );
    let tool_name = "mcp__patent-search__search_patents";
    let mock = MockModelClient::new();
    mock.push_turn(tool_turn(
        "msg_1",
        "ToolSearch",
        "search_1",
        r#"{"query":"select:mcp__patent-search__search_patents"}"#,
    ));
    mock.push_turn(tool_turn("msg_2", rebon_tool::INVOKE_DEFERRED_TOOL_NAME, "toolu_1",
        r#"{"tool_name":"mcp__patent-search__search_patents","arguments":{"query":"wireless charging"}}"#));
    mock.push_turn(text_turn("msg_3", "done"));
    let requests = execute_prompt(&host, engine, client.clone(), &mock, None, Vec::new()).await;
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].tools.iter().any(|tool| tool.name == tool_name));
    assert!(requests[0]
        .tools
        .iter()
        .any(|tool| tool.name == "ToolSearch"));
    assert_eq!(
        tool_result(&requests, "search_1")["matched_tools"],
        json!([tool_name])
    );
    assert_tool_result(&requests, "toolu_1", "ok", json!(true));
    assert_eq!(client.calls().len(), 1);
    assert_eq!(client.calls()[0].arguments["query"], "wireless charging");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_lsp_mcp_proxy_tools_are_visible_and_dispatch() {
    let (host, _plugin, engine) = test_engine();
    let client = Arc::new(InMemoryMcpClient::new());
    client.register_tool_definition(
        "rust_lsp",
        McpToolDefinition {
            name: "diagnostics".into(),
            description: "Rust diagnostics".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "file_path": { "type": "string" } },
                "required": ["file_path"],
                "additionalProperties": false
            }),
            read_only: true,
            destructive: false,
            open_world: false,
            search_hint: Some("rust diagnostics".into()),
            always_load: true,
        },
        json!({"diagnostics": [{"message": "expected `;`"}]}),
    );
    client.register_tool_definition(
        "rust_lsp",
        McpToolDefinition {
            name: "hover".into(),
            description: "Rust hover".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string" },
                    "line": { "type": "integer" },
                    "character": { "type": "integer" }
                },
                "required": ["file_path", "line", "character"],
                "additionalProperties": false
            }),
            read_only: true,
            destructive: false,
            open_world: false,
            search_hint: Some("rust hover".into()),
            always_load: true,
        },
        json!({"hover": {"contents": {"value": "fn main()"}}}),
    );

    let mock = MockModelClient::new();
    mock.push_turn(tool_turn(
        "msg_1",
        "mcp__rust_lsp__diagnostics",
        "toolu_1",
        r#"{"file_path":"src/lib.rs"}"#,
    ));
    mock.push_turn(tool_turn(
        "msg_2",
        "mcp__rust_lsp__hover",
        "toolu_2",
        r#"{"file_path":"src/lib.rs","line":0,"character":3}"#,
    ));
    mock.push_turn(text_turn("msg_3", "done"));
    let requests = execute_prompt(&host, engine, client.clone(), &mock, None, Vec::new()).await;
    let mut names = requests[0]
        .tools
        .iter()
        .filter(|tool| tool.name.starts_with("mcp__"))
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["mcp__rust_lsp__diagnostics", "mcp__rust_lsp__hover"]
    );
    assert_eq!(requests.len(), 3);
    assert_tool_result(
        &requests,
        "toolu_1",
        "diagnostics",
        json!([{"message":"expected `;`"}]),
    );
    assert_tool_result(
        &requests,
        "toolu_2",
        "hover",
        json!({"contents":{"value":"fn main()"}}),
    );
    let calls = client.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].server, "rust_lsp");
    assert_eq!(calls[0].name, "diagnostics");
    assert_eq!(calls[0].arguments["file_path"], "src/lib.rs");
    assert_eq!(calls[1].server, "rust_lsp");
    assert_eq!(calls[1].name, "hover");
    assert_eq!(calls[1].arguments["line"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_dispatch_respects_index_and_cannot_survive_plugin_disposal() {
    let (host, plugin, engine) = test_engine();
    let client = Arc::new(InMemoryMcpClient::new());
    let mut definition = McpToolDefinition::new("ping");
    definition.always_load = false;
    client.register_tool_definition("server", definition, json!({"ok":true}));
    let definitions = collect_mcp_tool_definitions(client.as_ref()).await;
    let context = ToolContext::new()
        .with_mcp_client(client.clone())
        .with_tool_search_index(deferred_index(&engine, &definitions))
        .with_mcp_tool_definitions(Arc::new(definitions));
    let invoke = json!({"tool_name":"mcp__server__ping","arguments":{}});
    let unindexed = context
        .clone()
        .with_tool_search_index(Arc::new(ToolSearchIndex::default()));
    assert!(engine
        .invoke_tool(
            rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
            invoke.clone(),
            &unindexed
        )
        .await
        .is_err());
    assert!(client.calls().is_empty());
    context.record_discovered_deferred_tool("mcp__server__ping");
    assert_eq!(
        engine
            .invoke_tool(
                rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
                invoke.clone(),
                &context
            )
            .await
            .unwrap()["content"]["ok"],
        true
    );
    plugin.dispose();
    assert!(engine
        .invoke_tool(rebon_tool::INVOKE_DEFERRED_TOOL_NAME, invoke, &context)
        .await
        .is_err());
    assert!(engine
        .invoke_tool("mcp__server__ping", json!({}), &context)
        .await
        .is_err());
    assert_eq!(client.calls().len(), 1);
    host.owner.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_rejects_nonobject_missing_client_and_transport_failure() {
    let (host, _plugin, engine) = test_engine();
    let client = Arc::new(InMemoryMcpClient::new());
    let mut definition = McpToolDefinition::new("ping");
    definition.always_load = true;
    client.register_tool_definition("server", definition, json!({"ok":true}));
    let definitions = Arc::new(collect_mcp_tool_definitions(client.as_ref()).await);
    let missing = ToolContext::new().with_mcp_tool_definitions(definitions.clone());
    assert!(engine
        .invoke_tool("mcp__server__ping", json!({}), &missing)
        .await
        .is_err());
    let context = missing.with_mcp_client(client.clone());
    assert!(engine
        .invoke_tool("mcp__server__ping", json!([]), &context)
        .await
        .is_err());
    assert!(client.calls().is_empty());
    client.inject_error(
        "server",
        "ping",
        rebon_tool::McpClientError::Transport("connection lost".into()),
    );
    let error = engine
        .invoke_tool("mcp__server__ping", json!({}), &context)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("connection lost"));
    assert_eq!(
        engine
            .invoke_tool("mcp__server__ping", json!({}), &context)
            .await
            .unwrap()["content"]["ok"],
        true
    );
    host.owner.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_session_http_client_discovers_and_executes_both_routes() {
    let (host, _plugin, engine) = test_engine();
    let server = session::tests::Server::start(false, None).await;
    let mock = MockModelClient::new();
    mock.push_turn(tool_turn(
        "msg_1",
        "ToolSearch",
        "search_1",
        r#"{"query":"select:mcp__session-http__ping"}"#,
    ));
    mock.push_turn(tool_turn(
        "msg_2",
        "mcp__session-http__ping",
        "direct_1",
        r#"{"route":"direct"}"#,
    ));
    mock.push_turn(tool_turn(
        "msg_3",
        rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
        "deferred_1",
        r#"{"tool_name":"mcp__session-http__ping","arguments":{"route":"deferred"}}"#,
    ));
    mock.push_turn(text_turn("msg_4", "done"));
    let requests = execute_prompt(
        &host,
        engine,
        Arc::new(InMemoryMcpClient::new()),
        &mock,
        None,
        vec![server.config("session-http")],
    )
    .await;
    assert_eq!(requests.len(), 4);
    assert!(!requests[0]
        .tools
        .iter()
        .any(|tool| tool.name == "mcp__session-http__ping"));
    let search = tool_result(&requests, "search_1");
    assert_eq!(search["matched_tools"], json!(["mcp__session-http__ping"]));
    assert!(search["result"].as_str().unwrap().contains("<function>"));
    assert_tool_result(&requests, "direct_1", "source", json!("session"));
    assert_tool_result(&requests, "deferred_1", "source", json!("session"));
    assert_eq!(server.initializes.load(Ordering::SeqCst), 1);
    let calls = server.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0],
        json!({"name":"ping","arguments":{"route":"direct"}})
    );
    assert_eq!(
        calls[1],
        json!({"name":"ping","arguments":{"route":"deferred"}})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_filter_blocks_mcp_announcement_discovery_and_execution() {
    for always_load in [true, false] {
        let (host, _plugin, engine) = test_engine();
        let client = Arc::new(InMemoryMcpClient::new());
        let mut definition = McpToolDefinition::new("ping");
        definition.always_load = always_load;
        client.register_tool_definition("server", definition, json!({"ok":true}));
        let mock = MockModelClient::new();
        mock.push_turn(tool_turn(
            "msg_1",
            "ToolSearch",
            "search_1",
            r#"{"query":"select:mcp__server__ping"}"#,
        ));
        mock.push_turn(tool_turn("msg_2", "mcp__server__ping", "direct_1", "{}"));
        mock.push_turn(tool_turn(
            "msg_3",
            rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
            "deferred_1",
            r#"{"tool_name":"mcp__server__ping","arguments":{}}"#,
        ));
        mock.push_turn(text_turn("msg_4", "done"));
        let requests = execute_prompt(
            &host,
            engine,
            client.clone(),
            &mock,
            Some(ToolFilter::unrestricted().with_deny(["mcp__server__ping"])),
            Vec::new(),
        )
        .await;
        assert_eq!(requests.len(), 4);
        assert!(requests.iter().all(|request| !request
            .tools
            .iter()
            .any(|tool| tool.name == "mcp__server__ping")));
        assert!(
            client.calls().is_empty(),
            "denied MCP tools must not execute by either route"
        );
        // ToolSearch may mention the requested name in its error, but must not
        // return a callable schema through the production search index.
        let search = tool_result(&requests, "search_1");
        assert_eq!(search["matched_tools"], json!([]));
        assert_eq!(search["missing_tools"], json!(["mcp__server__ping"]));
        assert!(!search["result"].as_str().unwrap().contains("<function>"));
    }
}
