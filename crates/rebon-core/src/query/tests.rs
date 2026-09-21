use super::executor::{stable_session_prompt_cache_key, sync_ultraplan_run_handle_from_disk};
use super::prompt::history_has_anchored_minimal_anchor;
use super::*;
use async_trait::async_trait;
use rebon_api::{
    auto_compact_truncate, ContentBlockDelta, ContentBlockStart, MessageDeltaFields,
    MockModelClient, ModelError, PruneLevel, RetryConfig, RetryMiddleware, RetryNotifier,
    StreamEvent, ToolChoice,
};
use rebon_tool::{AgentRegistry, McpToolDefinition, Tool, ToolContext};
use rebon_tools_core::{
    PermissionDecision, ToolError, ToolErrorPresentation, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};
use rebon_types::{
    ExecutionPolicy, PolicyMode, RequirementSource, ReviewerVerdictRecord, RunPhase,
    UltraplanContext, UltraplanProfile, UltraplanRunState, VerdictSource,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

struct TestTurnEndHook<F>(F);

impl<F> crate::turn_hook::TurnHook for TestTurnEndHook<F>
where
    F: for<'a> Fn(&crate::turn_hook::TurnEndHookEvent<'a>, &mut crate::turn_hook::TurnHookContext)
        + Send
        + Sync,
{
    fn on_event(&self, _event: &QueryEvent, _context: &mut crate::turn_hook::TurnHookContext) {}

    fn on_turn_end(
        &self,
        event: &crate::turn_hook::TurnEndHookEvent<'_>,
        context: &mut crate::turn_hook::TurnHookContext,
        _state: &mut crate::turn_hook::TurnHookState,
    ) {
        (self.0)(event, context);
    }
}

fn test_turn_end_hook<F>(f: F) -> Arc<dyn crate::turn_hook::TurnHook>
where
    F: for<'a> Fn(&crate::turn_hook::TurnEndHookEvent<'a>, &mut crate::turn_hook::TurnHookContext)
        + Send
        + Sync
        + 'static,
{
    Arc::new(TestTurnEndHook(f))
}

fn continue_terminal_turn(
    event: &crate::turn_hook::TurnEndHookEvent<'_>,
    context: &mut crate::turn_hook::TurnHookContext,
    coercion: ApiMessage,
    forced_tool_choice: Option<ToolChoice>,
) {
    context.append_history(ApiMessage {
        role: Role::Assistant,
        content: event.message.content.clone(),
    });
    context.append_history(coercion);
    if let Some(tool_choice) = forced_tool_choice {
        context.update_params(move |params| params.next_tool_choice = Some(tool_choice));
    }
    context.request_continue_reserving(2);
}

struct AllowAllBroker;

#[async_trait]
impl crate::PermissionBroker for AllowAllBroker {
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

struct EnvVarGuard {
    name: &'static str,
    prior: Option<String>,
}

impl EnvVarGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let prior = std::env::var(name).ok();
        std::env::set_var(name, value);
        Self { name, prior }
    }

    fn set_absent(name: &'static str) -> Self {
        let prior = std::env::var(name).ok();
        std::env::remove_var(name);
        Self { name, prior }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

struct SubAgentsEnabledGuard {
    prior: bool,
}

impl SubAgentsEnabledGuard {
    fn set(enabled: bool) -> Self {
        let prior = rebon_tool::sub_agents_enabled();
        rebon_tool::set_sub_agents_enabled(enabled);
        Self { prior }
    }
}

impl Drop for SubAgentsEnabledGuard {
    fn drop(&mut self) {
        rebon_tool::set_sub_agents_enabled(self.prior);
    }
}

struct GatewayDeferredTool;

#[async_trait]
impl Tool for GatewayDeferredTool {
    fn id(&self) -> ToolId {
        ToolId::new("GatewayDeferred")
    }

    fn description(&self) -> &str {
        "Deferred gateway test tool"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" }
            },
            "required": ["message"],
            "additionalProperties": false
        })
    }

    fn should_defer(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        Ok(json!({ "called_with": input }))
    }
}

/// The `max_tokens` a one-turn session sent on `model`, with `configure`
/// applied to the executor and `per_turn` as the request's own cap.
async fn sent_max_tokens(
    tag: &str,
    model: &str,
    configure: impl FnOnce(EngineQueryExecutor) -> EngineQueryExecutor,
    per_turn: Option<u32>,
) -> u32 {
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "answer"));
    let captured = mock.clone();
    let client: Arc<dyn ModelClient> = Arc::new(mock);
    let projects_root_dir = temp_projects_root(tag);
    let projects_root = projects_root_dir.path();
    let cwd = projects_root.to_string_lossy().to_string();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let session = state.create_session(cwd.clone(), Vec::new());
    let engine = Arc::new(Engine::with_builtin_tools());
    let executor = configure(
        EngineQueryExecutor::new(engine, client, projects_root, model).with_server_state(state),
    );

    executor
        .execute(test_prompt_request(&session.id, &cwd, "go", per_turn))
        .await
        .unwrap();

    let requests = captured.captured_requests();
    assert_eq!(requests.len(), 1, "one turn, no title model: one request");
    requests[0].max_tokens
}

/// Limits as the harness resolves them for a provider entry: the startup
/// model's output limit as the default, and one other model with its own.
fn output_limits_handle() -> PruneLevelHandle {
    PruneLevelHandle::with_model_context_limits(
        PruneLevel::Conservative,
        1_000_000,
        128_000,
        [("other-model", 200_000)],
        [("other-model", 32_000)],
    )
}

fn test_system_prompt_config() -> SystemPromptConfig {
    SystemPromptConfig {
        model: "claude-test".to_string(),
        model_marketing_name: Some("Claude Test".to_string()),
        knowledge_cutoff: Some("May 2025".to_string()),
        tool_names: vec!["Read".to_string(), "AskUserQuestion".to_string()],
        deferred_tool_names: vec!["Write".to_string()],
        platform: "linux".to_string(),
        shell: "bash".to_string(),
        os_version: "test-os".to_string(),
        language: None,
        normal_system_prompt_override: None,
        minimal_system_prompt_override: None,
        chat_system_prompt_override: None,
        auto_continue_background_agents: true,
    }
}

fn test_prompt_request(
    session_id: &str,
    cwd: &str,
    text: &str,
    max_tokens: Option<u32>,
) -> rebon_agent_core::PromptRequest {
    rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: text.to_string(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: None,
        permission_publisher: None,
        cancel: CancelToken::new(),
        thinking_budget: None,
        max_tokens,
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

fn captured_runtime_context_text(request: &rebon_api::CreateMessageRequest) -> &str {
    request
        .messages
        .iter()
        .rev()
        .find_map(|message| {
            message.content.iter().find_map(|block| match block {
                ApiContentBlock::Text(text) if text.text.contains("<runtime_context>") => {
                    Some(text.text.as_str())
                }
                _ => None,
            })
        })
        .expect("captured virtual runtime context message")
}

/// Tool that records invocations and returns a canned JSON result.
#[derive(Debug, Default)]
struct RecordingTool {
    name: String,
    calls: Mutex<Vec<Value>>,
    coordinator_report_paths: Mutex<Vec<Vec<PathBuf>>>,
    response: Value,
    fail: bool,
    defer: bool,
}

impl RecordingTool {
    fn new(name: &str, response: Value) -> Self {
        Self {
            name: name.into(),
            calls: Mutex::new(Vec::new()),
            coordinator_report_paths: Mutex::new(Vec::new()),
            response,
            fail: false,
            defer: false,
        }
    }

    fn deferred(name: &str, response: Value) -> Self {
        Self {
            name: name.into(),
            calls: Mutex::new(Vec::new()),
            coordinator_report_paths: Mutex::new(Vec::new()),
            response,
            fail: false,
            defer: true,
        }
    }

    fn failing(name: &str) -> Self {
        Self {
            name: name.into(),
            calls: Mutex::new(Vec::new()),
            coordinator_report_paths: Mutex::new(Vec::new()),
            response: Value::Null,
            fail: true,
            defer: false,
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn coordinator_report_paths_by_call(&self) -> Vec<Vec<PathBuf>> {
        self.coordinator_report_paths.lock().unwrap().clone()
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

    fn should_defer(&self) -> bool {
        self.defer
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

    async fn call(&self, input: Value, context: &ToolContext) -> Result<Value, ToolError> {
        self.calls.lock().unwrap().push(input);
        self.coordinator_report_paths
            .lock()
            .unwrap()
            .push(context.coordinator_report_paths().to_vec());
        if self.fail {
            Err(ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!("tool blew up"),
            })
        } else {
            Ok(self.response.clone())
        }
    }
}

struct HangingTool {
    started: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

struct PendingToolGuard(Arc<AtomicBool>);

impl Drop for PendingToolGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[async_trait]
impl Tool for HangingTool {
    fn id(&self) -> ToolId {
        ToolId::new("Hanging")
    }

    fn description(&self) -> &str {
        "never finishes"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({ "type": "object" })
    }

    async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
        let _guard = PendingToolGuard(self.dropped.clone());
        self.started.store(true, Ordering::Release);
        std::future::pending::<()>().await;
        unreachable!()
    }
}

struct PresentedErrorTool;

#[async_trait]
impl Tool for PresentedErrorTool {
    fn id(&self) -> ToolId {
        ToolId::new("SendMessage")
    }

    fn description(&self) -> &str {
        "returns a structured presentation error"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({ "type": "object" })
    }

    async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
        Err(ToolError::Presented {
            tool: self.id(),
            presentation: ToolErrorPresentation::new(
                "agent_closed",
                "Agent is no longer available.",
                "Agent expired; spawn a fresh worker with the follow-up instructions.",
            ),
        })
    }
}

fn build_engine_with(tool: Arc<dyn Tool>) -> Arc<Engine> {
    struct ApproveBroker;
    #[async_trait::async_trait]
    impl crate::PermissionBroker for ApproveBroker {
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

fn build_engine_with_tools(tools: Vec<Arc<dyn Tool>>) -> Arc<Engine> {
    struct ApproveBroker;
    #[async_trait::async_trait]
    impl crate::PermissionBroker for ApproveBroker {
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
    for tool in tools {
        engine.register_tool(tool);
    }
    Arc::new(engine)
}

#[derive(Debug)]
struct UltraplanDiskUpdateTool {
    projects_root: PathBuf,
    cwd: String,
    run_id: String,
}

#[async_trait]
impl Tool for UltraplanDiskUpdateTool {
    fn id(&self) -> ToolId {
        ToolId::new("Read")
    }

    fn description(&self) -> &str {
        "simulates a runner-side ultraplan run-state update while the engine turn is active"
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
        input: &Value,
        _context: &ToolContext,
    ) -> Result<PermissionDecision, ToolError> {
        Ok(PermissionDecision::allow(input.clone()))
    }

    async fn call(&self, _input: Value, _context: &ToolContext) -> Result<Value, ToolError> {
        let mut state =
            rebon_session::load_ultraplan_run(&self.projects_root, &self.cwd, &self.run_id)
                .expect("run state exists");
        state.phase = RunPhase::Executing;
        state.round = 2;
        state.last_plan_draft = Some("runner approved draft".into());
        state.reviewer_verdicts.push(ReviewerVerdictRecord {
            round: 2,
            verdict: "USER_REJECTED".into(),
            blocking_gaps: 1,
            source: VerdictSource::UserRejection,
        });
        state.updated_at_ms = 20;
        rebon_session::save_ultraplan_run(&self.projects_root, &self.cwd, &state).map_err(
            |err| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            },
        )?;
        Ok(json!({"updated": true}))
    }
}

#[derive(Clone)]
struct AnchoredMockClient {
    inner: MockModelClient,
}

impl AnchoredMockClient {
    fn new(inner: MockModelClient) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ModelClient for AnchoredMockClient {
    fn provider_name(&self) -> &'static str {
        "mock-anchored"
    }

    fn supports_anchored_minimal(&self) -> bool {
        true
    }

    fn reset_session_state(&self) {
        self.inner.reset_session_state();
    }

    fn end_turn(&self) {
        self.inner.end_turn();
    }

    fn invalidate_previous_response_id(&self) {
        self.inner.invalidate_previous_response_id();
    }

    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> rebon_api::ModelResult<rebon_api::StreamEventStream> {
        self.inner.create_message_stream(request).await
    }
}

#[derive(Clone)]
struct NoTransientMockClient {
    inner: MockModelClient,
}

impl NoTransientMockClient {
    fn new(inner: MockModelClient) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ModelClient for NoTransientMockClient {
    fn provider_name(&self) -> &'static str {
        "mock-no-transient"
    }

    fn supports_request_scoped_transient_context(&self) -> bool {
        false
    }

    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> rebon_api::ModelResult<rebon_api::StreamEventStream> {
        self.inner.create_message_stream(request).await
    }
}

fn text_message_texts(messages: &[ApiMessage]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|message| message.content.iter().filter_map(ApiContentBlock::as_text))
        .collect()
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

fn text_turn_with_stop(id: &str, text: &str, stop_reason: StopReason) -> Vec<StreamEvent> {
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
                stop_reason: Some(stop_reason),
                usage: Usage {
                    output_tokens: 4,
                    ..Default::default()
                },
            },
        },
        StreamEvent::MessageStop,
    ]
}

fn text_turn(id: &str, text: &str) -> Vec<StreamEvent> {
    text_turn_with_stop(id, text, StopReason::EndTurn)
}

fn generated_image_turn(
    id: &str,
    images: &[(&str, &str, &str)],
    stop_reason: StopReason,
) -> Vec<StreamEvent> {
    let mut events = vec![message_start(id)];
    for (index, (image_id, data, media_type)) in images.iter().enumerate() {
        events.push(StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::ImageGeneration {
                id: (*image_id).into(),
                status: Some("completed".into()),
            },
        });
        events.push(StreamEvent::ContentBlockDelta {
            index,
            delta: ContentBlockDelta::ImageDataDelta {
                b64_json: (*data).into(),
                partial_index: None,
                revised_prompt: None,
                media_type: Some((*media_type).into()),
            },
        });
        events.push(StreamEvent::ContentBlockStop { index });
    }
    events.extend([
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(stop_reason),
                usage: Usage {
                    output_tokens: 4,
                    ..Default::default()
                },
            },
        },
        StreamEvent::MessageStop,
    ]);
    events
}

fn thinking_turn_with_stop(
    id: &str,
    thinking: &str,
    data: Option<&str>,
    signature: Option<&str>,
    stop_reason: StopReason,
) -> Vec<StreamEvent> {
    let mut events = vec![
        message_start(id),
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Thinking {
                thinking: String::new(),
                data: data.map(str::to_owned),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ThinkingDelta {
                thinking: thinking.into(),
            },
        },
    ];
    if let Some(signature) = signature {
        events.push(StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::SignatureDelta {
                signature: signature.into(),
            },
        });
    }
    events.extend([
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(stop_reason),
                usage: Usage {
                    output_tokens: 4,
                    ..Default::default()
                },
            },
        },
        StreamEvent::MessageStop,
    ]);
    events
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

fn two_tool_turn(
    id: &str,
    first: (&str, &str, &str),
    second: (&str, &str, &str),
) -> Vec<StreamEvent> {
    let mut events = vec![message_start(id)];
    for (index, (tool_name, tool_id, input_json)) in [first, second].into_iter().enumerate() {
        events.push(StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::ToolUse {
                id: tool_id.into(),
                name: tool_name.into(),
            },
        });
        events.push(StreamEvent::ContentBlockDelta {
            index,
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: input_json.into(),
            },
        });
        events.push(StreamEvent::ContentBlockStop { index });
    }
    events.extend([
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage {
                    output_tokens: 12,
                    ..Default::default()
                },
            },
        },
        StreamEvent::MessageStop,
    ]);
    events
}

async fn drain(rx: &mut mpsc::UnboundedReceiver<QueryEvent>) -> Vec<QueryEvent> {
    let mut out = Vec::new();
    while let Some(event) = rx.recv().await {
        let terminal = event.is_terminal();
        out.push(event);
        if terminal {
            break;
        }
    }
    out
}

fn query_event_kind(event: &QueryEvent) -> &'static str {
    match event {
        QueryEvent::Stream(_) => "stream",
        QueryEvent::IterationComplete { .. } => "iteration",
        QueryEvent::ToolDispatchStart { .. } => "tool-start",
        QueryEvent::ToolAutoModeAllowed { .. } => "auto-allowed",
        QueryEvent::ToolDispatchProgress { .. } => "tool-progress",
        QueryEvent::ToolDispatchResult { .. } => "tool-result",
        QueryEvent::IterationLimitReached { .. } => "iteration-limit",
        QueryEvent::AttachmentInjected { .. } => "attachment",
        QueryEvent::ContextReset { .. } => "context-reset",
        QueryEvent::CompactingStarted { .. } => "compact-start",
        QueryEvent::CompactingFinished { .. } => "compact-finish",
        QueryEvent::Cancelled => "cancelled",
        QueryEvent::PermissionQuery(_) => "permission",
        QueryEvent::Error(_) => "error",
        QueryEvent::Done { .. } => "done",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CacheTraceObservation {
    SessionBasePrompt {
        stable_base_system: bool,
        cache_hits: Vec<bool>,
    },
    Request {
        model: String,
        message_count: usize,
        runtime_context_message: Option<String>,
        transient_context_message: Option<String>,
        cache_miss_reason: CacheMissReason,
        cache_trace_context: Option<CacheTraceContext>,
    },
    Usage {
        model: String,
        input_tokens: u32,
        output_tokens: u32,
        cache_trace_context: Option<CacheTraceContext>,
    },
}

struct RecordingCacheTraceHook {
    observations: Arc<Mutex<Vec<CacheTraceObservation>>>,
}

impl crate::turn_hook::TurnHook for RecordingCacheTraceHook {
    fn on_event(&self, _event: &QueryEvent, _context: &mut crate::turn_hook::TurnHookContext) {}

    fn on_cache_trace(&self, event: &crate::turn_hook::CacheTraceEvent<'_>) {
        let observation = match event {
            crate::turn_hook::CacheTraceEvent::SessionBasePromptCache {
                stable_base_system,
                cache_hits,
            } => CacheTraceObservation::SessionBasePrompt {
                stable_base_system: *stable_base_system,
                cache_hits: cache_hits.to_vec(),
            },
            crate::turn_hook::CacheTraceEvent::RequestBuilt {
                request,
                runtime_context_message,
                transient_context_message,
                cache_miss_reason,
            } => CacheTraceObservation::Request {
                model: request.model.clone(),
                message_count: request.messages.len(),
                runtime_context_message: runtime_context_message.map(str::to_string),
                transient_context_message: transient_context_message.map(str::to_string),
                cache_miss_reason: *cache_miss_reason,
                cache_trace_context: request.cache_trace_context.clone(),
            },
            crate::turn_hook::CacheTraceEvent::RequestUsage {
                model,
                usage,
                cache_trace_context,
            } => CacheTraceObservation::Usage {
                model: (*model).to_string(),
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_trace_context: (*cache_trace_context).cloned(),
            },
        };
        self.observations
            .lock()
            .expect("cache trace observations poisoned")
            .push(observation);
    }
}

fn cache_trace_recording_seat() -> (
    Arc<crate::turn_hook::TurnHookSeat>,
    Arc<Mutex<Vec<CacheTraceObservation>>>,
) {
    let seat = crate::turn_hook::TurnHookSeat::new();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let registration = seat
        .subscribe(
            "tests/cache-trace-recorder",
            crate::turn_hook::Order::LAST,
            Arc::new(RecordingCacheTraceHook {
                observations: observations.clone(),
            }),
        )
        .unwrap();
    drop(registration);
    (seat, observations)
}

/// Mark a streamed turn as cut off by `max_tokens`.
fn cut_off_by_max_tokens(mut events: Vec<StreamEvent>) -> Vec<StreamEvent> {
    for event in &mut events {
        if let StreamEvent::MessageDelta { delta } = event {
            delta.stop_reason = Some(StopReason::MaxTokens);
        }
    }
    events
}

/// A tool call whose input JSON never closed before `max_tokens`.
fn truncated_tool_turn(id: &str, tool_id: &str) -> Vec<StreamEvent> {
    cut_off_by_max_tokens(tool_turn(id, "Read", tool_id, "{\"path\":"))
}

/// The tool_use the model replayed for `tool_id`, and the result block
/// that answers it in the very next message.
fn replayed_tool_call<'a>(
    messages: &'a [ApiMessage],
    tool_id: &str,
) -> (&'a ToolUseBlock, &'a rebon_api::ToolResultBlock) {
    let (index, tool_use) = messages
        .iter()
        .enumerate()
        .find_map(|(index, message)| {
            message.content.iter().find_map(|block| match block {
                ApiContentBlock::ToolUse(tool_use) if tool_use.id == tool_id => {
                    Some((index, tool_use))
                }
                _ => None,
            })
        })
        .expect("the cut-off call is replayed so its result has a partner");
    let result = messages
        .get(index + 1)
        .expect("a replayed tool_use is followed by its results")
        .content
        .iter()
        .find_map(|block| match block {
            ApiContentBlock::ToolResult(result) if result.tool_use_id == tool_id => Some(result),
            _ => None,
        })
        .expect("the truncation reaches the model as that call's result");
    (tool_use, result)
}

fn turn_budget_warning_text(max_iterations: usize) -> String {
    format!(
        "[SYSTEM: You are approaching the iteration limit. \
         You have 10 iterations remaining out of {}. \
         Please wrap up your current work and provide a final response.]",
        max_iterations
    )
}

fn is_turn_budget_warning(message: &ApiMessage) -> bool {
    message.role == Role::User
        && message.content.len() == 1
        && message.content[0].as_text().is_some_and(|text| {
            text.starts_with("[SYSTEM: You are approaching the iteration limit.")
        })
}

fn turn_budget_warning_count(messages: &[ApiMessage]) -> usize {
    messages
        .iter()
        .filter(|message| is_turn_budget_warning(message))
        .count()
}

struct RecordingTurnBudgetHook(Arc<AtomicUsize>);

impl crate::turn_hook::TurnHook for RecordingTurnBudgetHook {
    fn on_event(&self, _event: &QueryEvent, _context: &mut crate::turn_hook::TurnHookContext) {}

    fn on_turn_budget(
        &self,
        _event: &crate::turn_hook::TurnBudgetEvent,
        _context: &mut crate::turn_hook::TurnHookContext,
    ) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn turn_budget_recording_seat() -> (Arc<crate::turn_hook::TurnHookSeat>, Arc<AtomicUsize>) {
    let seat = crate::turn_hook::TurnHookSeat::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let registration = seat
        .subscribe(
            "tests/turn-budget-recorder",
            crate::turn_hook::Order::LAST,
            Arc::new(RecordingTurnBudgetHook(calls.clone())),
        )
        .unwrap();
    drop(registration);
    (seat, calls)
}
// ── Attachment-poller integration ────────────────────────────

/// Scripted poller that returns a fixed sequence of messages on
/// each call. Used to prove that [`TurnControlPlugin`] forwards the
/// poller output into the next iteration's history.
#[derive(Default)]
struct ScriptedPoller {
    calls: std::sync::Mutex<Vec<(String, String, u64, AttachmentPollPhase)>>,
    messages_per_call: Vec<Vec<ApiMessage>>,
    report_paths_per_call: std::sync::Mutex<std::collections::VecDeque<Vec<PathBuf>>>,
    ready_report_paths: std::sync::Mutex<Vec<PathBuf>>,
}

impl ScriptedPoller {
    fn with_messages(messages_per_call: Vec<Vec<ApiMessage>>) -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            messages_per_call,
            report_paths_per_call: std::sync::Mutex::new(std::collections::VecDeque::new()),
            ready_report_paths: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn with_messages_and_report_paths(
        messages_per_call: Vec<Vec<ApiMessage>>,
        report_paths_per_call: Vec<Vec<PathBuf>>,
    ) -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            messages_per_call,
            report_paths_per_call: std::sync::Mutex::new(report_paths_per_call.into()),
            ready_report_paths: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl crate::query::AttachmentPoller for ScriptedPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Eager {
            return Vec::new();
        }
        let mut calls = self.calls.lock().expect("scripted poller mutex");
        let idx = calls.len();
        calls.push((
            request.session_id.to_string(),
            request.turn_id.to_string(),
            request.next_iteration,
            request.phase,
        ));
        let report_paths = self
            .report_paths_per_call
            .lock()
            .expect("scripted report paths mutex")
            .pop_front()
            .unwrap_or_default();
        *self
            .ready_report_paths
            .lock()
            .expect("ready report paths mutex") = report_paths;
        self.messages_per_call.get(idx).cloned().unwrap_or_default()
    }

    fn take_coordinator_report_paths(&self) -> Vec<PathBuf> {
        std::mem::take(
            &mut *self
                .ready_report_paths
                .lock()
                .expect("ready report paths mutex"),
        )
    }
}

struct EagerPoller {
    calls: AtomicUsize,
    messages: Mutex<std::collections::VecDeque<Vec<ApiMessage>>>,
}

impl EagerPoller {
    fn new(messages: Vec<Vec<ApiMessage>>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            messages: Mutex::new(messages.into()),
        }
    }
}

impl crate::query::AttachmentPoller for EagerPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Regular {
            return Vec::new();
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.messages
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default()
    }
}

/// A single-drive hook runtime carrying the built-in terminal waterfall.
fn task_reconciliation_runtime() -> crate::turn_hook::TurnHookRuntime {
    crate::turn_hook::TurnHookRuntime::from_snapshots(
        crate::turn_hook::TurnHookSeat::new().snapshot(),
    )
}

fn record_task_results(runtime: &crate::turn_hook::TurnHookRuntime, results: &[(&str, Value)]) {
    let params = QueryParams::new("test", Vec::new());
    for (tool_name, output) in results {
        runtime.dispatch_tool_result(&crate::turn_hook::ToolResultHookEvent::completed(
            tool_name,
            Some(output),
            &params,
        ));
    }
}

/// Run the terminal phase once and return what it wrote to history.
fn drive_task_reconciliation(
    runtime: &crate::turn_hook::TurnHookRuntime,
    task_list_id: &str,
) -> Vec<ApiMessage> {
    let mut params = QueryParams::new("test", Vec::new());
    let message = AssistantMessage {
        id: "msg_terminal".into(),
        model: "mock".into(),
        content: vec![ApiContentBlock::Text(TextBlock {
            text: "done".to_string(),
        })],
        stop_reason: Some(StopReason::EndTurn),
        usage: Usage::default(),
    };
    runtime.dispatch_turn_end(&crate::turn_hook::TurnEndHookEvent::terminal_candidate(
        &message,
        0,
        &[],
        task_list_id,
        &params,
    ));
    runtime
        .take_writeback()
        .apply_to_params(&mut params)
        .history
}
// ── End-to-end: EngineQueryExecutor ──────────────────────────────

use rebon_agent_core::{MemorySessionUpdatePublisher, PromptRequest};
use rebon_types::{SessionUpdate, TextContent};

fn temp_projects_root(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("rebon-core-executor-{tag}-"))
        .tempdir()
        .unwrap()
}

/// Serializes process-env mutation across tests. Poison is recovered on
/// purpose: `EnvVarGuard` restores the variable on unwind, so a panicked
/// holder leaves no state worth cascading into unrelated tests.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::test_env_lock()
}

async fn wait_for_session_title_update(publisher: &MemorySessionUpdatePublisher) -> Option<String> {
    for _ in 0..50 {
        if let Some(title) =
            publisher
                .snapshot()
                .into_iter()
                .find_map(|update| match update.update {
                    SessionUpdate::SessionInfoUpdate { title, .. } => title,
                    _ => None,
                })
        {
            return Some(title);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    None
}

struct ForkingModelClient {
    main: MockModelClient,
    title: MockModelClient,
}

#[async_trait]
impl ModelClient for ForkingModelClient {
    fn provider_name(&self) -> &'static str {
        "mock"
    }

    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
        Some(Arc::new(self.title.clone()))
    }

    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> rebon_api::ModelResult<rebon_api::StreamEventStream> {
        self.main.create_message_stream(request).await
    }
}

fn basic_prompt_request(session_id: &str, cwd: &str) -> PromptRequest {
    PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session_id.into(),
        cwd: cwd.into(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: "ping".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: None,
        permission_publisher: None,
        cancel: rebon_agent_core::PromptCancel::new(),
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

fn read_assistant_transcript_entries(
    projects_root: &std::path::Path,
    cwd: &str,
    session_id: &str,
) -> Vec<Value> {
    let path = rebon_session::transcript_file_path(projects_root, cwd, session_id);
    std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] == "assistant")
        .collect()
}

fn make_user_entry(uuid: &str, text: &str) -> rebon_session::TranscriptEntry {
    rebon_session::TranscriptEntry {
        entry_type: "user".into(),
        uuid: uuid.into(),
        parent_uuid: None,
        timestamp: Some(format!("2026-04-09T00:00:{:02}.000Z", uuid.len())),
        raw: json!({
            "type": "user",
            "uuid": uuid,
            "message": { "role": "user", "content": text },
            "timestamp": "2026-04-09T00:00:00.000Z",
        }),
    }
}

fn make_assistant_entry(uuid: &str, parent: &str, text: &str) -> rebon_session::TranscriptEntry {
    rebon_session::TranscriptEntry {
        entry_type: "assistant".into(),
        uuid: uuid.into(),
        parent_uuid: Some(parent.into()),
        timestamp: Some(format!("2026-04-09T00:01:{:02}.000Z", uuid.len())),
        raw: json!({
            "type": "assistant",
            "uuid": uuid,
            "parentUuid": parent,
            "message": { "role": "assistant", "content": text },
            "timestamp": "2026-04-09T00:01:00.000Z",
        }),
    }
}

fn write_transcript_fixture(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    entries: Vec<rebon_session::TranscriptEntry>,
) {
    let writable = entries
        .into_iter()
        .map(|entry| {
            let mut writable =
                rebon_session::TranscriptWriteEntry::new(entry.entry_type, entry.raw)
                    .with_uuid(entry.uuid);
            if let Some(parent) = entry.parent_uuid {
                writable = writable.with_parent(parent);
            }
            if let Some(timestamp) = entry.timestamp {
                writable = writable.with_timestamp(timestamp);
            }
            writable
        })
        .collect();
    rebon_session::write_transcript_entries(projects_root, cwd, session_id, writable).unwrap();
}

fn mobile_prompt_request(
    session_id: &str,
    cwd: &str,
    text: &str,
    user_message_uuid: &str,
) -> rebon_agent_core::PromptRequest {
    rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: text.to_string(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: None,
        permission_publisher: None,
        cancel: CancelToken::new(),
        thinking_budget: None,
        max_tokens: None,
        reasoning_effort_ordinal: None,
        additional_working_directories: Vec::new(),
        coordinator_mode: None,
        coordinator_report_paths: Vec::new(),
        user_message_uuid: Some(user_message_uuid.to_string()),
        background_agent_system: None,
        background_agent_tool_filter: None,
        execution_policy: None,
        replay_requests: Vec::new(),
        skill_invocations: Vec::new(),
    }
}

/// Inject a session record with a pre-loaded transcript
/// straight into `ServerState` for tests. Uses the public
/// `create_session` + `get_session` APIs plus a write via the
/// session_storage write path so the record's
/// `loaded_transcript` field gets populated through a real
/// `load_session` call.
fn insert_session_with_transcript(
    state: &Arc<rebon_session_state::ServerState>,
    sid: &str,
    cwd: &str,
    entries: Vec<rebon_session::TranscriptEntry>,
) {
    use rebon_session::{transcript_file_path, write_transcript_entries, TranscriptWriteEntry};

    let projects_root_dir = temp_projects_root("replay-seed");
    let projects_root = projects_root_dir.path();
    let writable: Vec<TranscriptWriteEntry> = entries
        .into_iter()
        .map(|e| {
            let mut entry = TranscriptWriteEntry::new(e.entry_type.clone(), e.raw.clone())
                .with_uuid(e.uuid.clone());
            if let Some(ts) = e.timestamp {
                entry = entry.with_timestamp(ts);
            }
            if let Some(parent) = e.parent_uuid {
                entry = entry.with_parent(parent);
            }
            entry
        })
        .collect();
    write_transcript_entries(projects_root, cwd, sid, writable).unwrap();

    // Re-use load_session to populate the state so
    // `loaded_transcript` is filled by the same code path
    // `session/load` uses in production.
    state
        .load_session(projects_root, sid, cwd, None, Vec::new())
        .expect("load_session failed");
    // Sanity check the path exists on disk.
    let _ = transcript_file_path(projects_root, cwd, sid);
}

/// Build-and-run helper for the `toolUseResults` persistence tests.
/// Runs one tool-call turn followed by a final text turn, returns the
/// `user` tool_result entry that comes out of the transcript so tests
/// can inspect its `toolUseResults` sibling.
async fn run_single_tool_turn(
    tag: &str,
    tool_name: &str,
    tool_use_id: &str,
    tool_response: Value,
    failing: bool,
    presented: bool,
) -> rebon_session::TranscriptEntry {
    let tool_ref: Arc<dyn Tool> = if presented {
        Arc::new(PresentedErrorTool)
    } else if failing {
        Arc::new(RecordingTool::failing(tool_name))
    } else {
        Arc::new(RecordingTool::new(tool_name, tool_response))
    };
    let engine = build_engine_with(tool_ref);

    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool",
        tool_name,
        tool_use_id,
        "{\"filePath\":\"/tmp/a.rs\"}",
    ));
    client.push_turn(text_turn("msg_done", "all done"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root(tag);
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());

    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state.clone());

    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "trigger tool".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
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
    };
    executor.execute(request).await.unwrap();

    let record = state.get_session(&session.id).unwrap();
    record
        .loaded_transcript
        .iter()
        .find(|e| {
            e.entry_type == "user"
                && e.raw
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    })
                    .unwrap_or(false)
        })
        .expect("tool_result user entry should be persisted")
        .clone()
}

/// Refuses every `PreToolUse`, naming itself so the refusal is traceable to
/// the handle it came from rather than to some other gate.
struct RefusesEveryTool;

impl crate::policy_seat::PolicySubscriber for RefusesEveryTool {
    fn interest(&self, kind: crate::policy_seat::PolicyEventKind) -> bool {
        kind == crate::policy_seat::PolicyEventKind::PreToolUse
    }

    fn decide<'a>(
        &'a self,
        _request: &'a crate::policy_seat::PolicyRequest,
    ) -> crate::policy_seat::PolicyFuture<'a> {
        Box::pin(async move {
            crate::policy_seat::Verdict::Deny {
                reason: "refused by this session's own subscriber".to_string(),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Test submodules
// ---------------------------------------------------------------------------
//
// These are the query-executor tests, grouped by the area they cover. The
// shared imports, helpers and fixtures they all use live in this module;
// every submodule pulls them in with `use super::*;`.
//
// ---------------------------------------------------------------------------

mod anchored_minimal;
mod attachments_and_transcripts;
mod code_mode;
mod context_reset_and_cache_trace;
mod executor_integration;
mod model_routing;
mod request_building;
mod run_query_loop;
mod session_prompt_cache;
mod session_tool_exposure;
mod stream_events_and_cancel;
mod system_prompt_config;
mod task_reconciliation;
mod tool_projection;
mod transcript_persistence;
mod transient_context;
mod ultraplan_and_tool_results;
