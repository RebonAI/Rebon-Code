use super::*;
use rebon_tools_core::PermissionRequest;

const PRIORITY: &str = "# Code Mode";

struct CodeModeTools {
    available: AtomicBool,
    enabled: AtomicBool,
    calls: Arc<AtomicUsize>,
}

impl CodeModeTools {
    fn new(available: bool) -> Arc<Self> {
        Arc::new(Self {
            available: AtomicBool::new(available),
            enabled: AtomicBool::new(true),
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }
}

struct CodeModeTool {
    enabled: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CodeModeTool {
    fn id(&self) -> ToolId {
        ToolId::new("run_code")
    }
    fn description(&self) -> &str {
        "测试 Code Mode 工具"
    }
    fn input_schema(&self) -> ToolInputSchema {
        json!({"type": "object"})
    }
    fn is_enabled(&self) -> bool {
        self.enabled
    }
    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        Ok(if input["ask"].as_bool().unwrap_or(false) {
            PermissionDecision::ask(PermissionRequest::new("测试权限", "需要用户批准"), None)
        } else {
            PermissionDecision::allow(input.clone())
        })
    }
    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(input)
    }
}

impl rebon_tool::PluginToolProvider for CodeModeTools {
    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        (name == "run_code" && self.available.load(Ordering::SeqCst)).then(|| {
            Arc::new(CodeModeTool {
                enabled: self.enabled.load(Ordering::SeqCst),
                calls: self.calls.clone(),
            }) as Arc<dyn Tool>
        })
    }
    fn tool_names(&self) -> Vec<String> {
        if self.available.load(Ordering::SeqCst) {
            vec!["run_code".into()]
        } else {
            Vec::new()
        }
    }
}

fn code_mode_params(model: &str) -> QueryParams {
    let mut params = QueryParams::new(model, vec![ApiMessage::user_text("继续工作")]);
    params.system = Some("原有 system 前缀".into());
    params.tools = vec![ApiTool {
        name: "run_code".into(),
        description: "测试 Code Mode 工具".into(),
        input_schema: json!({"type": "object"}),
    }];
    params
}

fn code_mode_notifications(events: &[QueryEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            QueryEvent::AttachmentInjected { message, .. } => Some(message),
            _ => None,
        })
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ApiContentBlock::Text(text) if text.text.contains(PRIORITY) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

fn priority_messages(request: &CreateMessageRequest) -> usize {
    request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(
            |block| matches!(block, ApiContentBlock::Text(text) if text.text.contains(PRIORITY)),
        )
        .count()
}

async fn code_mode_query(
    engine: Arc<Engine>,
    session: Arc<SessionHandle>,
    params: QueryParams,
    tools: Arc<CodeModeTools>,
) -> Vec<QueryEvent> {
    let context = ToolContext::new().with_plugin_tools(tools);
    let mut rx = run_query(engine, session, params, context, CancelToken::new());
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert!(
        matches!(events.last(), Some(QueryEvent::Done { .. })),
        "{events:?}"
    );
    events
}

#[tokio::test]
async fn code_mode_resolver_only_matrix() {
    for (available, enabled, filtered) in [
        (true, true, false),
        (true, false, false),
        (false, true, false),
        (true, true, true),
    ] {
        let mock = MockModelClient::new();
        mock.push_turn(text_turn("done", "完成"));
        let tools = CodeModeTools::new(available);
        tools.enabled.store(enabled, Ordering::SeqCst);
        let engine = Arc::new(Engine::new());
        let resolver = engine.scoped_tool_resolver(None, &[], Some(tools));
        let mut params = code_mode_params("mock");
        params.tools.clear();
        if filtered {
            params.effective_tool_filter = Some(ToolFilter::allow_only(["Read"]));
        }
        let mut rx = run_query(
            engine,
            SessionHandle::new(Arc::new(mock.clone())),
            params,
            ToolContext::new().with_tool_resolver(resolver),
            CancelToken::new(),
        );
        while rx.recv().await.is_some() {}
        let requests = mock.captured_requests();
        let expected = available && enabled && !filtered;
        assert_eq!(
            requests[0].system.as_deref().unwrap().contains(PRIORITY),
            expected
        );
        assert_eq!(
            requests[0].tools.iter().any(|tool| tool.name == "run_code"),
            expected
        );
    }
}

#[tokio::test]
async fn code_mode_initial_system_matrix_all_models_and_empty_system() {
    for model in ["gpt-6-astra", "gpt-5.6-sol", "claude-test", "unknown-model"] {
        for system in [None, Some("原有 system 前缀".to_string())] {
            let mock = MockModelClient::new();
            mock.push_turn(text_turn("done", "完成"));
            let tools = CodeModeTools::new(true);
            let mut params = code_mode_params(model);
            params.system = system.clone();
            let expected_tools = params.tools.clone();
            let events = code_mode_query(
                Arc::new(Engine::new()),
                SessionHandle::new(Arc::new(mock.clone())),
                params,
                tools,
            )
            .await;
            let requests = mock.captured_requests();
            let request = &requests[0];
            let prompt = request.system.as_deref().unwrap();
            assert!(prompt.contains(PRIORITY), "{model}: {prompt}");
            assert!(prompt.contains("Prefer `run_code`"));
            assert!(prompt.starts_with(system.as_deref().unwrap_or("")));
            assert_eq!(prompt.matches(PRIORITY).count(), 1);
            assert_eq!(request.tools, expected_tools);
            assert!(request.tool_choice.is_none());
            assert_eq!(priority_messages(request), 0);
            assert!(code_mode_notifications(&events).is_empty());
        }
    }
}

#[tokio::test]
async fn code_mode_unavailable_matrix_never_advertises_priority() {
    for case in [
        "absent",
        "disabled",
        "filtered",
        "policy",
        "invariant",
        "chat",
        "minimal",
    ] {
        let mock = MockModelClient::new();
        mock.push_turn(text_turn("done", "完成"));
        let tools = CodeModeTools::new(case != "absent");
        tools.enabled.store(case != "disabled", Ordering::SeqCst);
        let mut params = code_mode_params("mock");
        match case {
            "filtered" => params.effective_tool_filter = Some(ToolFilter::allow_only(["Read"])),
            "policy" | "invariant" => {
                let policy = ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
                    "code-mode",
                    "plan_mode_active",
                    PolicyMode::Enforce,
                ));
                if case == "policy" {
                    params.execution_policy = Some(policy);
                } else {
                    params.invariant_execution_policy = Some(policy);
                }
            }
            "chat" => params.capability_mode = AgentCapabilityMode::Chat,
            "minimal" => params.capability_mode = AgentCapabilityMode::Minimal,
            _ => {}
        }
        let events = code_mode_query(
            Arc::new(Engine::new()),
            SessionHandle::new(Arc::new(mock.clone())),
            params,
            tools,
        )
        .await;
        let requests = mock.captured_requests();
        assert_eq!(
            requests[0].system.as_deref(),
            Some("原有 system 前缀"),
            "{case}"
        );
        assert!(
            !requests[0].tools.iter().any(|tool| tool.name == "run_code"),
            "{case}"
        );
        assert_eq!(priority_messages(&requests[0]), 0, "{case}");
        assert!(code_mode_notifications(&events).is_empty(), "{case}");
    }
}

#[tokio::test]
async fn code_mode_filtered_tool_cannot_execute_from_stale_announcements() {
    let mock = MockModelClient::new();
    mock.push_turn(tool_turn("code", "run_code", "code_id", "{}"));
    mock.push_turn(text_turn("done", "完成"));
    let tools = CodeModeTools::new(true);
    let mut params = code_mode_params("mock");
    params.effective_tool_filter = Some(ToolFilter::allow_only(["Read"]));
    let events = code_mode_query(
        Arc::new(Engine::new()),
        SessionHandle::new(Arc::new(mock.clone())),
        params,
        tools.clone(),
    )
    .await;
    assert_eq!(tools.calls.load(Ordering::SeqCst), 0);
    assert!(events.iter().any(|event| matches!(event,
        QueryEvent::ToolDispatchResult { name, outcome: Err(_), .. } if name == "run_code"
    )));
    assert!(!mock.captured_requests()[0]
        .tools
        .iter()
        .any(|tool| tool.name == "run_code"));
}

#[tokio::test]
async fn code_mode_preference_does_not_bypass_permission_requests() {
    let mock = MockModelClient::new();
    mock.push_turn(tool_turn("code", "run_code", "code_id", r#"{"ask":true}"#));
    mock.push_turn(text_turn("done", "完成"));
    let tools = CodeModeTools::new(true);
    let events = code_mode_query(
        Arc::new(Engine::new()),
        SessionHandle::new(Arc::new(mock.clone())),
        code_mode_params("mock"),
        tools.clone(),
    )
    .await;
    assert_eq!(tools.calls.load(Ordering::SeqCst), 0);
    assert!(events.iter().any(|event| matches!(event,
        QueryEvent::ToolDispatchResult { name, outcome: Err(_), .. } if name == "run_code"
    )));
    assert!(mock.captured_requests()[0]
        .system
        .as_deref()
        .unwrap()
        .contains(PRIORITY));
}

#[tokio::test]
async fn code_mode_late_activation_survives_turns_and_replaced_history() {
    let mock = MockModelClient::new();
    let session = SessionHandle::new(Arc::new(mock.clone()));
    let engine = Arc::new(Engine::new());
    let tools = CodeModeTools::new(false);
    let mut history = vec![ApiMessage::user_text("开始")];
    for turn in 0..4 {
        mock.push_turn(text_turn("done", "完成"));
        tools.available.store(turn > 0, Ordering::SeqCst);
        let mut params = code_mode_params("mock");
        params.tools.clear();
        if turn == 3 {
            history = vec![ApiMessage::user_text("压缩后的摘要")];
        }
        params.messages = history;
        let events = code_mode_query(engine.clone(), session.clone(), params, tools.clone()).await;
        let requests = mock.captured_requests();
        let request = requests.last().unwrap();
        assert_eq!(request.system.as_deref(), Some("原有 system 前缀"));
        assert_eq!(
            priority_messages(request),
            usize::from(turn > 0),
            "turn {turn}"
        );
        assert_eq!(
            code_mode_notifications(&events).len(),
            usize::from(turn == 1 || turn == 3)
        );
        assert_eq!(
            request.tools.iter().any(|tool| tool.name == "run_code"),
            turn > 0
        );
        history = request.messages.clone();
        history.push(ApiMessage::assistant_text("完成"));
        history.push(ApiMessage::user_text("继续"));
    }
}

#[tokio::test]
async fn code_mode_initial_priority_keeps_system_stable_across_disable_and_reenable() {
    let mock = MockModelClient::new();
    let session = SessionHandle::new(Arc::new(mock.clone()));
    let engine = Arc::new(Engine::new());
    let tools = CodeModeTools::new(true);
    let mut history = vec![ApiMessage::user_text("开始")];
    let mut first_system = None;
    for available in [true, false, true, true] {
        mock.push_turn(text_turn("done", "完成"));
        tools.available.store(available, Ordering::SeqCst);
        let mut params = code_mode_params("mock");
        params.messages = history;
        let events = code_mode_query(engine.clone(), session.clone(), params, tools.clone()).await;
        let requests = mock.captured_requests();
        let request = requests.last().unwrap();
        if first_system.is_none() {
            assert!(request.system.as_deref().unwrap().contains(PRIORITY));
            first_system = request.system.clone();
        }
        assert_eq!(request.system, first_system);
        assert_eq!(
            request.tools.iter().any(|tool| tool.name == "run_code"),
            available
        );
        if !available {
            let notifications = code_mode_notifications(&events);
            assert_eq!(notifications.len(), 1);
            assert!(notifications[0].contains("unavailable"));
        }
        history = request.messages.clone();
        history.push(ApiMessage::assistant_text("完成"));
        history.push(ApiMessage::user_text("继续"));
    }
}

struct SwitchCodeMode(Arc<CodeModeTools>);

#[async_trait]
impl Tool for SwitchCodeMode {
    fn id(&self) -> ToolId {
        ToolId::new("SwitchCodeMode")
    }
    fn description(&self) -> &str {
        "测试运行中工具变更"
    }
    fn input_schema(&self) -> ToolInputSchema {
        json!({"type": "object"})
    }
    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        self.0
            .available
            .store(input["enabled"].as_bool().unwrap(), Ordering::SeqCst);
        Ok(json!({}))
    }
}

#[tokio::test]
async fn code_mode_mid_turn_activation_uses_notification_and_preserves_tool_adjacency() {
    let tools = CodeModeTools::new(false);
    let engine = build_engine_with(Arc::new(SwitchCodeMode(tools.clone())));
    let mock = MockModelClient::new();
    mock.push_turn(tool_turn(
        "switch",
        "SwitchCodeMode",
        "switch_id",
        r#"{"enabled":true}"#,
    ));
    mock.push_turn(tool_turn("code", "run_code", "code_id", "{}"));
    mock.push_turn(text_turn("done", "完成"));
    let mut params = code_mode_params("mock");
    params.tools = tools_from_engine(&engine);
    let events = code_mode_query(
        engine,
        SessionHandle::new(Arc::new(mock.clone())),
        params,
        tools.clone(),
    )
    .await;
    let requests = mock.captured_requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.system.as_deref(), Some("原有 system 前缀"));
        assert!(request.tool_choice.is_none());
    }
    assert_eq!(priority_messages(&requests[0]), 0);
    assert_eq!(priority_messages(&requests[1]), 1);
    assert_eq!(priority_messages(&requests[2]), 1);
    assert_eq!(code_mode_notifications(&events).len(), 1);
    let messages = &requests[1].messages;
    let call_index = messages
        .iter()
        .position(|m| m.role == Role::Assistant)
        .unwrap();
    assert!(matches!(
        messages[call_index + 1].content[0],
        ApiContentBlock::ToolResult(_)
    ));
    assert_eq!(tools.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn code_mode_resumed_history_and_child_handles_have_independent_lifecycles() {
    let mock = MockModelClient::new();
    let parent = SessionHandle::new(Arc::new(mock.clone()));
    let engine = Arc::new(Engine::new());
    let tools = CodeModeTools::new(true);
    mock.push_turn(text_turn("parent", "完成"));
    let mut params = code_mode_params("mock");
    params
        .messages
        .insert(0, ApiMessage::assistant_text("恢复的历史"));
    let events = code_mode_query(engine.clone(), parent.clone(), params, tools.clone()).await;
    assert_eq!(code_mode_notifications(&events).len(), 1);
    let child = parent.fork_for_sub_agent(None);
    mock.push_turn(text_turn("child", "完成"));
    let events = code_mode_query(engine, child, code_mode_params("mock"), tools).await;
    let requests = mock.captured_requests();
    assert_eq!(requests[0].system.as_deref(), Some("原有 system 前缀"));
    assert!(requests[1].system.as_deref().unwrap().contains(PRIORITY));
    assert!(code_mode_notifications(&events).is_empty());
}

struct ActivateCodeMode(Arc<CodeModeTools>);

impl AttachmentPoller for ActivateCodeMode {
    fn poll(&self, _request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        self.0.available.store(true, Ordering::SeqCst);
        Vec::new()
    }
}

#[tokio::test]
async fn code_mode_first_eager_poll_can_enable_system_priority_without_forcing_routing() {
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("done", "完成"));
    let tools = CodeModeTools::new(false);
    let mut params = code_mode_params("mock").with_attachment_poller(
        Arc::new(ActivateCodeMode(tools.clone())),
        "session",
        "turn",
    );
    params.tools.clear();
    params.next_tool_choice = Some(ToolChoice::None);
    let events = code_mode_query(
        Arc::new(Engine::new()),
        SessionHandle::new(Arc::new(mock.clone())),
        params,
        tools,
    )
    .await;
    let requests = mock.captured_requests();
    assert!(requests[0].system.as_deref().unwrap().contains(PRIORITY));
    assert_eq!(requests[0].tool_choice, Some(ToolChoice::None));
    assert!(requests[0].tools.iter().any(|tool| tool.name == "run_code"));
    assert!(code_mode_notifications(&events).is_empty());
}

struct ResetCodeMode(AtomicBool);

impl AttachmentPoller for ResetCodeMode {
    fn poll(&self, _request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        Vec::new()
    }
    fn take_context_reset(&self) -> Option<Vec<ApiMessage>> {
        self.0
            .swap(false, Ordering::SeqCst)
            .then(|| vec![ApiMessage::user_text("清理上下文后继续")])
    }
}

#[tokio::test]
async fn code_mode_context_reset_matrix_preserves_initial_placement() {
    for initially_available in [false, true] {
        let tools = CodeModeTools::new(initially_available);
        let engine = build_engine_with(Arc::new(SwitchCodeMode(tools.clone())));
        let mock = MockModelClient::new();
        mock.push_turn(tool_turn(
            "switch",
            "SwitchCodeMode",
            "switch_id",
            r#"{"enabled":true}"#,
        ));
        mock.push_turn(text_turn("done", "完成"));
        let mut params = code_mode_params("mock").with_attachment_poller(
            Arc::new(ResetCodeMode(AtomicBool::new(true))),
            "session",
            "turn",
        );
        params.tools.extend(tools_from_engine(&engine));
        params.post_context_reset_system = params.system.clone();
        let events = code_mode_query(
            engine,
            SessionHandle::new(Arc::new(mock.clone())),
            params,
            tools,
        )
        .await;
        assert!(events
            .iter()
            .any(|event| matches!(event, QueryEvent::ContextReset { .. })));
        let requests = mock.captured_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].system, requests[1].system);
        assert_eq!(
            requests[1].system.as_deref().unwrap().contains(PRIORITY),
            initially_available
        );
        assert_eq!(
            priority_messages(&requests[1]),
            usize::from(!initially_available)
        );
        assert_eq!(
            code_mode_notifications(&events).len(),
            usize::from(!initially_available)
        );
    }
}

#[tokio::test]
async fn code_mode_auto_compact_matrix_restores_priority_before_provider_request() {
    for initially_available in [false, true] {
        let mock = MockModelClient::new();
        let session = SessionHandle::new(Arc::new(mock.clone()));
        let engine = Arc::new(Engine::new());
        let tools = CodeModeTools::new(initially_available);
        mock.push_turn(text_turn("initial", "完成"));
        code_mode_query(
            engine.clone(),
            session.clone(),
            code_mode_params("mock"),
            tools.clone(),
        )
        .await;
        let first = mock.captured_requests()[0].clone();
        let mut params = code_mode_params("mock");
        params.messages = first.messages.clone();
        for _ in 0..8 {
            params.messages.push(ApiMessage::assistant_text("旧的回复"));
            params.messages.push(ApiMessage::user_text("旧的请求"));
        }
        let prune = PruneLevelHandle::with_context_window(PruneLevel::Conservative, 120_000);
        prune.report_usage(prune.budget.auto_compact_threshold());
        params.prune_level = Some(prune);
        tools.available.store(true, Ordering::SeqCst);
        mock.push_turn(text_turn("summary", "历史摘要"));
        mock.push_turn(text_turn("done", "完成"));
        let events = code_mode_query(engine, session, params, tools).await;
        assert!(events
            .iter()
            .any(|event| matches!(event, QueryEvent::CompactingFinished { .. })));
        let requests = mock.captured_requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].system, first.system);
        assert_eq!(
            priority_messages(&requests[2]),
            usize::from(!initially_available)
        );
        assert_eq!(
            code_mode_notifications(&events).len(),
            usize::from(!initially_available)
        );
    }
}
