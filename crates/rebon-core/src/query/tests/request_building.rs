use super::*;

#[test]
fn stable_session_prompt_cache_key_is_session_stable_and_model_scoped() {
    let a = stable_session_prompt_cache_key("sess-abc", "gpt-5.5");
    let b = stable_session_prompt_cache_key("sess-abc", "gpt-5.5");
    let other_model = stable_session_prompt_cache_key("sess-abc", "gpt-5.4");
    let other_session = stable_session_prompt_cache_key("sess-def", "gpt-5.5");

    assert_eq!(a, b);
    assert_ne!(a, other_model);
    assert_ne!(a, other_session);
    assert!(a.starts_with("rebon-session-"));
    assert!(!a.contains("sess-abc"));
}

#[test]
fn parent_context_capsule_materializes_recent_turns_without_raw_tool_pairing() {
    let context = rebon_tool::ContextRequest {
        mode: rebon_tool::ContextShareMode::CompactWithRecentTurns,
        include_recent_turns: Some(2),
        include_tool_results: rebon_tool::ToolResultMode::Facts,
        include_files: rebon_tool::FileContextMode::References,
        instructions: Some("prefer current files".into()),
    };
    let messages = vec![
        ApiMessage::user_text("please inspect crates/rebon-core/src/query.rs"),
        ApiMessage {
            role: Role::Assistant,
            content: vec![ApiContentBlock::ToolUse(ToolUseBlock {
                id: "toolu_123".into(),
                name: "Read".into(),
                input: serde_json::json!({ "file_path": "crates/rebon-core/src/query.rs" }),
            })],
        },
        ApiMessage {
            role: Role::User,
            content: vec![ApiContentBlock::ToolResult(ToolResultBlock {
                tool_use_id: "toolu_123".into(),
                content: ToolResultContent::text("file content here"),
                is_error: false,
            })],
        },
    ];

    let capsule = build_parent_context_capsule_from_messages(&context, &messages).unwrap();
    let text = capsule.text.as_ref();

    assert!(text.contains("recent_turns:"));
    assert!(text.contains("tool_use: name=Read input_hash="));
    assert!(text.contains("tool_result: tool_use_id=toolu_123"));
    assert!(text.contains("referenced_files:"));
    assert!(text.contains("crates/rebon-core/src/query.rs"));
    assert!(!text.contains("file content here"));
}

#[test]
fn channel_permission_broker_is_discoverable_through_hook_and_policy_wrappers() {
    let (channel_broker, _rx) = crate::permission::ChannelPermissionBroker::new("session-a");
    let channel_arc: Arc<dyn PermissionBroker> = Arc::new(channel_broker.clone());
    let policy = crate::policy_seat::PolicySources::default();
    let hooked: Arc<dyn PermissionBroker> = Arc::new(HookedPermissionBroker::new(
        channel_arc.clone(),
        policy.clone(),
    ));
    let rules_then_hooked: Arc<dyn PermissionBroker> = Arc::new(
        crate::policy::RulesBasedPermissionBroker::new(crate::policy::PolicyStore::new(), hooked),
    );
    let hooked_then_rules: Arc<dyn PermissionBroker> = Arc::new(HookedPermissionBroker::new(
        Arc::new(crate::policy::RulesBasedPermissionBroker::new(
            crate::policy::PolicyStore::new(),
            channel_arc,
        )),
        policy,
    ));

    assert!(channel_permission_broker_from(rules_then_hooked.as_ref()).is_some());
    assert!(channel_permission_broker_from(hooked_then_rules.as_ref()).is_some());
}

#[test]
fn build_model_request_forwards_anthropic_context_management() {
    let manager = ContextManager::new(None, vec![ApiMessage::user_text("hi")]);
    let mut params = QueryParams::new("claude-sonnet-4-6", Vec::new());
    params.context_management = Some(
        rebon_api::ContextManagementConfig::anthropic_full_history_replay_with_thresholds(
            50_000, 100_000,
        ),
    );

    let request = build_model_request(&manager, &params);
    let context_management = request.context_management.expect("context management");
    let value = serde_json::to_value(context_management).unwrap();
    let edits = value["edits"].as_array().unwrap();
    assert_eq!(edits[0]["type"], "clear_thinking_20251015");
    assert_eq!(edits[1]["type"], "clear_tool_uses_20250919");
    assert_eq!(edits[2]["type"], "compact_20260112");
}

#[test]
fn request_prompt_cost_report_includes_tools_and_messages() {
    let request = CreateMessageRequest {
        model: "test-model".into(),
        messages: vec![ApiMessage::user_text("hello history")],
        system: Some("system text".into()),
        transient_context: Some("transient text".into()),
        tools: vec![
            ApiTool {
                name: "small".into(),
                description: "tiny".into(),
                input_schema: serde_json::json!({"type":"object"}),
            },
            ApiTool {
                name: "large".into(),
                description: "x".repeat(500),
                input_schema: serde_json::json!({"type":"object"}),
            },
        ],
        tool_choice: None,
        max_tokens: 100,
        temperature: None,
        stop_sequences: Vec::new(),
        stream: true,
        metadata: None,
        thinking: None,
        reasoning_effort: None,
        reasoning_mode: None,
        reasoning_summary: None,
        web_search: None,
        context_management: None,
        cache_trace_context: None,
        compaction_trigger: false,
    };

    let report = build_request_prompt_cost_report(&request);

    assert!(report
        .sections
        .iter()
        .any(|section| section.name == "provider system prompt"));
    let tools = report
        .sections
        .iter()
        .find(|section| section.name == "provider-visible tools")
        .expect("tool section");
    assert_eq!(tools.top_details(1)[0].name, "large");
    assert!(report
        .sections
        .iter()
        .any(|section| section.name == "message/history blocks"));
}

#[test]
fn request_prompt_cost_report_reflects_auto_compacted_execution_history() {
    let raw_history = vec![
        ApiMessage::user_text("please inspect the code"),
        ApiMessage {
            role: Role::Assistant,
            content: vec![ApiContentBlock::ToolUse(ToolUseBlock {
                id: "old_read".into(),
                name: "Read".into(),
                input: json!({"file_path": "src/lib.rs"}),
            })],
        },
        ApiMessage {
            role: Role::User,
            content: vec![ApiContentBlock::ToolResult(ToolResultBlock {
                tool_use_id: "old_read".into(),
                content: ToolResultContent::text("large old tool output ".repeat(2_000)),
                is_error: false,
            })],
        },
        ApiMessage::user_text("tail prompt"),
        ApiMessage::assistant_text("tail response"),
    ];
    let compacted_history = auto_compact_truncate(raw_history.clone(), 1);

    let raw_request = CreateMessageRequest {
        model: "test-model".into(),
        messages: raw_history,
        system: None,
        transient_context: None,
        tools: Vec::new(),
        tool_choice: None,
        max_tokens: 100,
        temperature: None,
        stop_sequences: Vec::new(),
        stream: true,
        metadata: None,
        thinking: None,
        reasoning_effort: None,
        reasoning_mode: None,
        reasoning_summary: None,
        web_search: None,
        context_management: None,
        cache_trace_context: None,
        compaction_trigger: false,
    };
    let compacted_request = CreateMessageRequest {
        messages: compacted_history,
        ..raw_request.clone()
    };

    let raw_messages = build_request_prompt_cost_report(&raw_request)
        .sections
        .into_iter()
        .find(|section| section.name == "message/history blocks")
        .expect("raw message section");
    let compacted_messages = build_request_prompt_cost_report(&compacted_request)
        .sections
        .into_iter()
        .find(|section| section.name == "message/history blocks")
        .expect("compacted message section");

    assert!(
        compacted_messages.estimated_tokens < raw_messages.estimated_tokens,
        "compacted replay should cost less than raw replay: compacted={}, raw={}",
        compacted_messages.estimated_tokens,
        raw_messages.estimated_tokens
    );
}

#[test]
fn provider_visible_tool_metadata_stays_bounded() {
    let engine = Engine::with_builtin_tools();
    let tools = eager_tools_from_engine(&engine);
    let report = prompt_tool_metadata_report("provider-visible tools", &tools);

    // This is an explosion guard, not a target for shortening tool contracts.
    // Intentional metadata should have enough headroom on every platform.
    let budget = 10_000;
    assert!(
        report.estimated_tokens < budget,
        "provider-visible metadata estimate was {} tokens; top tools: {:?}",
        report.estimated_tokens,
        report
            .top_details(5)
            .into_iter()
            .map(|detail| (detail.name.clone(), detail.estimated_tokens))
            .collect::<Vec<_>>()
    );
    let bash = report
        .details
        .iter()
        .find(|detail| detail.name == "Bash")
        .expect("Bash metadata detail");
    assert!(
        bash.estimated_tokens < 900,
        "Bash metadata estimate was {} tokens",
        bash.estimated_tokens
    );

    let gateway = tools
        .iter()
        .find(|tool| tool.name == rebon_tool::INVOKE_DEFERRED_TOOL_NAME)
        .expect("gateway tool");
    assert!(gateway.input_schema["properties"]["tool_name"]
        .get("enum")
        .is_none());
    let gateway_detail = report
        .details
        .iter()
        .find(|detail| detail.name == rebon_tool::INVOKE_DEFERRED_TOOL_NAME)
        .expect("gateway metadata detail");
    assert!(
        gateway_detail.estimated_tokens < 120,
        "gateway metadata estimate was {} tokens",
        gateway_detail.estimated_tokens
    );
}
