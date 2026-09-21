use super::*;

#[tokio::test]
async fn minimal_tool_search_promotes_full_schema_on_the_next_request() {
    let mut engine = Engine::with_builtin_tools();
    engine.register_tool(Arc::new(GatewayDeferredTool));
    let engine = Arc::new(engine);
    let projection = runtime_tool_projection_for_mode(
        &engine,
        true,
        None,
        None,
        &[],
        &[],
        AgentCapabilityMode::Minimal,
        None,
    );

    let mock = MockModelClient::new();
    mock.push_turn(tool_turn(
        "msg_1",
        rebon_tool::TOOL_SEARCH_TOOL_NAME,
        "toolu_1",
        r#"{"query":"select:Bash,Read,GatewayDeferred"}"#,
    ));
    mock.push_turn(tool_turn(
        "msg_2",
        "GatewayDeferred",
        "toolu_2",
        r#"{"message":"hello"}"#,
    ));
    mock.push_turn(text_turn("msg_3", "done"));
    let captured = mock.clone();

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("go")])
        .with_tools(projection.provider_visible_tools)
        .with_capability_mode(AgentCapabilityMode::Minimal)
        .with_max_iterations(4);
    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(mock)),
        params,
        ToolContext::new().with_tool_search_index(projection.tool_search_index),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    let requests = captured.captured_requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Bash", "Read", "ToolSearch"]
    );
    let promoted_bash = requests[1]
        .tools
        .iter()
        .find(|tool| tool.name == "Bash")
        .expect("Bash should remain provider-visible after schema promotion");
    assert!(promoted_bash.input_schema["properties"]
        .get("timeout")
        .is_some());
    let promoted_read = requests[1]
        .tools
        .iter()
        .find(|tool| tool.name == "Read")
        .expect("Read should remain provider-visible after schema promotion");
    assert!(promoted_read.input_schema["properties"]
        .get("pages")
        .is_some());
    let promoted = requests[1]
        .tools
        .iter()
        .find(|tool| tool.name == "GatewayDeferred")
        .expect("discovered tool should be provider-visible on the next request");
    assert_eq!(promoted.input_schema["required"], json!(["message"]));
}

#[test]
fn anchored_minimal_history_requires_persisted_assistant_output_or_tool_call() {
    assert!(!history_has_anchored_minimal_anchor(&[
        ApiMessage::user_text("prompt only"),
    ]));
    assert!(!history_has_anchored_minimal_anchor(&[ApiMessage {
        role: Role::Assistant,
        content: Vec::new(),
    }]));
    assert!(!history_has_anchored_minimal_anchor(&[ApiMessage {
        role: Role::Assistant,
        content: vec![ApiContentBlock::Thinking(rebon_api::ThinkingBlock {
            thinking: "internal reasoning".into(),
            ..Default::default()
        })],
    }]));
    assert!(!history_has_anchored_minimal_anchor(&[
        ApiMessage::user_text(
            "<system-generated-history-summary>user-only failures</system-generated-history-summary>",
        ),
    ]));
    assert!(history_has_anchored_minimal_anchor(&[
        ApiMessage::assistant_text("answer"),
    ]));
    assert!(history_has_anchored_minimal_anchor(&[ApiMessage {
        role: Role::Assistant,
        content: vec![ApiContentBlock::ToolUse(ToolUseBlock {
            id: "toolu_1".into(),
            name: "Read".into(),
            input: json!({}),
        })],
    }]));
    assert!(history_has_anchored_minimal_anchor(&[ApiMessage {
        role: Role::User,
        content: vec![ApiContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: "toolu_1".into(),
            content: ToolResultContent::text("failed"),
            is_error: true,
        })],
    }]));
}

#[test]
fn anchored_minimal_budget_clamp_is_one_shot_and_skips_reasoning_inclusive_budgets() {
    // The ordinary first turn of a Minimal session: clamp away.
    assert!(anchored_minimal_budget_clamp_applies(true, false, false));
    // A provider whose output budget covers hidden reasoning would spend the
    // whole clamped ceiling on a chain of thought and land no anchor.
    assert!(!anchored_minimal_budget_clamp_applies(true, true, false));
    // A bootstrap that already produced a reply without an anchor — a
    // `Thinking`-only turn — must not be re-clamped, or the phase never ends.
    assert!(!anchored_minimal_budget_clamp_applies(true, false, true));
    // Nothing to clamp once the session has left the bootstrap phase.
    assert!(!anchored_minimal_budget_clamp_applies(false, false, false));
}

#[test]
fn thinking_only_reply_is_an_assistant_turn_but_not_an_anchor() {
    let thinking_only = [
        ApiMessage::user_text("go"),
        ApiMessage {
            role: Role::Assistant,
            content: vec![ApiContentBlock::Thinking(rebon_api::ThinkingBlock {
                thinking: "spent the whole budget reasoning".into(),
                ..Default::default()
            })],
        },
    ];
    assert!(!history_has_anchored_minimal_anchor(&thinking_only));
    assert!(history_has_assistant_turn(&thinking_only));

    assert!(!history_has_assistant_turn(&[ApiMessage::user_text(
        "prompt only"
    )]));
}

#[tokio::test]
async fn anchored_minimal_tool_search_promotes_normal_projection_and_restores_budget() {
    struct PromotedAttachmentPoller;

    impl AttachmentPoller for PromotedAttachmentPoller {
        fn poll(&self, _request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
            Vec::new()
        }

        fn transient_context(&self) -> Option<String> {
            Some("promoted dynamic attachment".into())
        }
    }

    let mut engine = Engine::with_builtin_tools();
    engine.register_tool(Arc::new(GatewayDeferredTool));
    let engine = Arc::new(engine);
    let bootstrap = runtime_tool_projection_for_mode(
        &engine,
        true,
        None,
        None,
        &[],
        &[],
        AgentCapabilityMode::Minimal,
        None,
    );
    let promoted = runtime_tool_projection_for_mode(
        &engine,
        true,
        None,
        None,
        &engine.deferred_tool_names(),
        &[],
        AgentCapabilityMode::Normal,
        None,
    );

    let mock = MockModelClient::new();
    mock.push_turn(tool_turn(
        "msg_1",
        rebon_tool::TOOL_SEARCH_TOOL_NAME,
        "toolu_1",
        r#"{"query":"select:GatewayDeferred"}"#,
    ));
    mock.push_turn(text_turn("msg_2", "done"));
    let captured = mock.clone();
    let params = QueryParams {
        system: Some("minimal persona".into()),
        tools: bootstrap.provider_visible_tools,
        max_tokens: 1024,
        max_iterations: 3,
        capability_mode: AgentCapabilityMode::Minimal,
        anchored_minimal_promotion: Some(AnchoredMinimalPromotion {
            tools: promoted.provider_visible_tools,
            tool_search_index: promoted.tool_search_index,
            runtime_context_message: Some("promoted runtime".into()),
            transient_context_message: Some("promoted transient".into()),
            attachment_poller: Some(AttachmentPollerBinding::new(
                Arc::new(PromotedAttachmentPoller),
                "session-promoted",
                "turn-promoted",
            )),
            max_tokens: 4096,
        }),
        ..QueryParams::new("mock", vec![ApiMessage::user_text("go")])
    };

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(mock)),
        params,
        ToolContext::new().with_tool_search_index(bootstrap.tool_search_index),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    let requests = captured.captured_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].max_tokens, 1024);
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Bash", "Read", "ToolSearch"]
    );
    assert!(requests[0].transient_context.is_none());
    assert_eq!(requests[1].max_tokens, 4096);
    assert_eq!(requests[1].system.as_deref(), Some("minimal persona"));
    assert_eq!(
        requests[1].transient_context.as_deref(),
        Some("promoted transient\n\npromoted dynamic attachment")
    );
    assert!(captured_runtime_context_text(&requests[1]).contains("promoted runtime"));
    assert!(requests[1].tools.iter().any(|tool| tool.name == "Edit"));
    assert!(requests[1]
        .tools
        .iter()
        .any(|tool| tool.name == rebon_tool::TOOL_SEARCH_TOOL_NAME));
    let discovered = requests[1]
        .tools
        .iter()
        .find(|tool| tool.name == "GatewayDeferred")
        .expect("bootstrap discovery should survive promotion");
    assert_eq!(discovered.input_schema["required"], json!(["message"]));
    assert!(!requests[1]
        .tools
        .iter()
        .any(|tool| tool.name == "SaveMemory"));
}

#[tokio::test]
async fn anchored_minimal_promotes_after_a_failed_tool_call() {
    let engine = Arc::new(Engine::with_builtin_tools());
    let bootstrap = runtime_tool_projection_for_mode(
        &engine,
        true,
        None,
        None,
        &[],
        &[],
        AgentCapabilityMode::Minimal,
        None,
    );
    let promoted = runtime_tool_projection_for_mode(
        &engine,
        true,
        None,
        None,
        &engine.deferred_tool_names(),
        &[],
        AgentCapabilityMode::Normal,
        None,
    );
    let mock = MockModelClient::new();
    mock.push_turn(tool_turn(
        "msg_1",
        "Edit",
        "toolu_1",
        r#"{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
    ));
    mock.push_turn(text_turn("msg_2", "recovered"));
    let captured = mock.clone();
    let params = QueryParams {
        tools: bootstrap.provider_visible_tools,
        max_tokens: 1024,
        max_iterations: 3,
        capability_mode: AgentCapabilityMode::Minimal,
        anchored_minimal_promotion: Some(AnchoredMinimalPromotion {
            tools: promoted.provider_visible_tools,
            tool_search_index: promoted.tool_search_index,
            runtime_context_message: None,
            transient_context_message: None,
            attachment_poller: None,
            max_tokens: 3072,
        }),
        ..QueryParams::new("mock", vec![ApiMessage::user_text("edit")])
    };

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(mock)),
        params,
        ToolContext::new().with_tool_search_index(bootstrap.tool_search_index),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(events.iter().any(|event| matches!(
        event,
        QueryEvent::ToolDispatchResult {
            name,
            outcome: Err(_),
            ..
        } if name == "Edit"
    )));
    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    let requests = captured.captured_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].max_tokens, 3072);
    assert!(requests[1].tools.iter().any(|tool| tool.name == "Edit"));
}

#[tokio::test]
async fn anchored_promotion_keeps_tool_results_adjacent_to_their_tool_use() {
    let engine = Arc::new(Engine::with_builtin_tools());
    let bootstrap = runtime_tool_projection_for_mode(
        &engine,
        true,
        None,
        None,
        &[],
        &[],
        AgentCapabilityMode::Minimal,
        None,
    );
    let promoted = runtime_tool_projection_for_mode(
        &engine,
        true,
        None,
        None,
        &engine.deferred_tool_names(),
        &[],
        AgentCapabilityMode::Normal,
        None,
    );
    let mock = MockModelClient::new();
    mock.push_turn(tool_turn(
        "msg_1",
        "Edit",
        "toolu_1",
        r#"{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
    ));
    mock.push_turn(text_turn("msg_2", "done"));
    // The DeepSeek plugin declares `requestScopedTransientContext: false`, so
    // the promoted context is folded into the conversation — the shape that
    // used to split a tool_use from its tool_result.
    mock.set_supports_request_scoped_transient_context(false);
    let captured = mock.clone();
    let params = QueryParams {
        tools: bootstrap.provider_visible_tools,
        max_iterations: 3,
        capability_mode: AgentCapabilityMode::Minimal,
        anchored_minimal_promotion: Some(AnchoredMinimalPromotion {
            tools: promoted.provider_visible_tools,
            tool_search_index: promoted.tool_search_index,
            runtime_context_message: Some("promoted runtime".into()),
            // The promoted context is what used to be spliced in too early.
            transient_context_message: Some("promoted transient".into()),
            attachment_poller: None,
            max_tokens: 3072,
        }),
        ..QueryParams::new("mock", vec![ApiMessage::user_text("edit")])
    };

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(mock)),
        params,
        ToolContext::new().with_tool_search_index(bootstrap.tool_search_index),
        CancelToken::new(),
    );
    drain(&mut rx).await;

    let requests = captured.captured_requests();
    assert_eq!(requests.len(), 2);
    let messages = &requests[1].messages;
    // Every tool_use must be answered by the very next message. A provider
    // that validates call/output adjacency (DeepSeek Responses) rejects the
    // whole request otherwise, blaming a missing tool output.
    for (index, message) in messages.iter().enumerate() {
        let tool_use_ids: Vec<&str> = message
            .content
            .iter()
            .filter_map(|block| match block {
                ApiContentBlock::ToolUse(tool_use) => Some(tool_use.id.as_str()),
                _ => None,
            })
            .collect();
        if tool_use_ids.is_empty() {
            continue;
        }
        let next = messages
            .get(index + 1)
            .expect("a tool_use must be followed by its results");
        let result_ids: Vec<&str> = next
            .content
            .iter()
            .filter_map(|block| match block {
                ApiContentBlock::ToolResult(result) => Some(result.tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            tool_use_ids, result_ids,
            "message {index} calls {tool_use_ids:?} but the next message carries {result_ids:?}"
        );
    }
    assert!(
        messages
            .iter()
            .any(|message| message.content.iter().any(|block| block
                .as_text()
                .is_some_and(|text| text.contains("promoted transient")))),
        "the promoted context must still reach the model, just after the tool results"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn anchored_minimal_text_answer_promotes_after_executor_reload() {
    let _guard = env_lock();
    let _stable_guard = EnvVarGuard::set("REBON_STABLE_BASE_SYSTEM", "1");
    let engine = Arc::new(Engine::with_builtin_tools());
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "first answer"));
    mock.push_turn(text_turn("msg_2", "second answer"));
    let captured = mock.clone();
    let client: Arc<dyn ModelClient> = Arc::new(AnchoredMockClient::new(mock));
    let projects_root_dir = temp_projects_root("anchored-minimal-text-promotion");
    let projects_root = projects_root_dir.path();
    let cwd = projects_root.to_string_lossy().to_string();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let session = state.create_session(cwd.clone(), Vec::new());
    let mut prompt_config = test_system_prompt_config();
    prompt_config.minimal_system_prompt_override = Some("custom minimal persona".to_string());
    let first_executor =
        EngineQueryExecutor::new(engine.clone(), client.clone(), projects_root, "mock-model")
            .with_system_prompt_config(prompt_config.clone())
            .with_capability_mode(AgentCapabilityMode::Minimal)
            .with_max_tokens(4096)
            .with_server_state(state.clone());

    first_executor
        .execute(test_prompt_request(&session.id, &cwd, "first", None))
        .await
        .unwrap();
    drop(first_executor);

    let resumed_executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system_prompt_config(prompt_config)
        .with_capability_mode(AgentCapabilityMode::Minimal)
        .with_max_tokens(4096)
        .with_server_state(state);
    resumed_executor
        .execute(test_prompt_request(&session.id, &cwd, "second", None))
        .await
        .unwrap();

    let requests = captured.captured_requests();
    assert_eq!(requests.len(), 2);
    // No `REBON_ANCHORED_BOOTSTRAP_MAX_TOKENS`: the bootstrap request keeps the
    // caller's budget, because the tool schema — not a cap — is the anchor.
    assert_eq!(requests[0].max_tokens, 4096);
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["bash", "str_replace_editor"]
    );
    assert!(requests[0].transient_context.is_none());
    assert!(!requests[0]
        .messages
        .iter()
        .any(rebon_api::is_runtime_context_message));

    assert_eq!(requests[1].max_tokens, 4096);
    assert_eq!(
        requests[0].system.as_deref(),
        Some("custom minimal persona")
    );
    assert_eq!(requests[1].system, requests[0].system);
    assert!(requests[1].tools.iter().any(|tool| tool.name == "Edit"));
    assert!(requests[1]
        .tools
        .iter()
        .any(|tool| tool.name == rebon_tool::TOOL_SEARCH_TOOL_NAME));
    assert!(!requests[1]
        .tools
        .iter()
        .any(|tool| tool.name == "SaveMemory"));
    let runtime = captured_runtime_context_text(&requests[1]);
    assert!(runtime.contains("Provider-visible tools loaded in advance for this turn"));
    assert!(runtime.contains("The deferred tools below"));
}

#[tokio::test(flavor = "current_thread")]
async fn anchored_minimal_provider_failure_does_not_promote_and_respects_lower_budget() {
    struct BootstrapLeakPoller;

    impl AttachmentPoller for BootstrapLeakPoller {
        fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
            match request.phase {
                AttachmentPollPhase::Regular => {
                    vec![ApiMessage::user_text("bootstrap attachment leak")]
                }
                AttachmentPollPhase::Eager => Vec::new(),
            }
        }

        fn transient_context(&self) -> Option<String> {
            Some("bootstrap transient leak".into())
        }
    }

    let _guard = env_lock();
    let engine = Arc::new(Engine::with_builtin_tools());
    let mock = MockModelClient::new();
    mock.push_error(ModelError::Permanent("bootstrap failed".into()));
    mock.push_turn(text_turn("msg_2", "retry answer"));
    let captured = mock.clone();
    let client: Arc<dyn ModelClient> = Arc::new(AnchoredMockClient::new(mock));
    let projects_root_dir = temp_projects_root("anchored-minimal-failure");
    let projects_root = projects_root_dir.path();
    std::fs::write(projects_root.join("REBON.md"), "initial bootstrap docs").unwrap();
    let cwd = projects_root.to_string_lossy().to_string();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let session = state.create_session(cwd.clone(), Vec::new());
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system_prompt_config(test_system_prompt_config())
        .with_capability_mode(AgentCapabilityMode::Minimal)
        .with_max_tokens(2048)
        .with_extra_attachment_poller(Arc::new(BootstrapLeakPoller))
        .with_server_state(state);

    assert!(executor
        .execute(test_prompt_request(&session.id, &cwd, "first", Some(512)))
        .await
        .is_err());
    std::fs::write(projects_root.join("REBON.md"), "updated bootstrap docs").unwrap();
    executor
        .execute(test_prompt_request(&session.id, &cwd, "retry", None))
        .await
        .unwrap();

    let requests = captured.captured_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].max_tokens, 512);
    assert_eq!(requests[1].max_tokens, 2048);
    for request in &requests {
        assert_eq!(
            request
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bash", "str_replace_editor"]
        );
        assert!(request.transient_context.is_none());
        assert!(!request
            .messages
            .iter()
            .any(rebon_api::is_runtime_context_message));
        assert!(!request.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                block.as_text().is_some_and(|text| {
                    text.contains("bootstrap attachment leak")
                        || text.contains("updated bootstrap docs")
                })
            })
        }));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn anchored_bootstrap_budget_cap_is_opt_in_through_the_environment() {
    let _guard = env_lock();
    let _cap = EnvVarGuard::set("REBON_ANCHORED_BOOTSTRAP_MAX_TOKENS", "1024");
    let engine = Arc::new(Engine::with_builtin_tools());
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "answer"));
    let captured = mock.clone();
    let client: Arc<dyn ModelClient> = Arc::new(AnchoredMockClient::new(mock));
    let projects_root_dir = temp_projects_root("anchored-bootstrap-cap");
    let projects_root = projects_root_dir.path();
    let cwd = projects_root.to_string_lossy().to_string();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let session = state.create_session(cwd.clone(), Vec::new());
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system_prompt_config(test_system_prompt_config())
        .with_capability_mode(AgentCapabilityMode::Minimal)
        .with_max_tokens(4096)
        .with_server_state(state);

    executor
        .execute(test_prompt_request(&session.id, &cwd, "first", None))
        .await
        .unwrap();

    let requests = captured.captured_requests();
    assert_eq!(requests[0].max_tokens, 1024);
}

/// A turn that names no cap sends the model's own output limit. It used to
/// send a flat 4096, which is what `rebon exec` without `--effort` ran on:
/// a tool call larger than that was cut off three rounds running and the
/// run failed.
#[tokio::test(flavor = "current_thread")]
async fn a_turn_without_a_cap_sends_the_models_output_limit() {
    let sent = sent_max_tokens(
        "max-tokens-model-limit",
        "mock-model",
        |executor| executor.with_prune_level(output_limits_handle()),
        None,
    )
    .await;
    assert_eq!(sent, 128_000);
}

/// The limit is the turn's model's, not the one the session started on:
/// the handle is pointed at the model before the cap is read.
#[tokio::test(flavor = "current_thread")]
async fn the_default_cap_follows_the_turns_model() {
    let sent = sent_max_tokens(
        "max-tokens-other-model",
        "other-model",
        |executor| executor.with_prune_level(output_limits_handle()),
        None,
    )
    .await;
    assert_eq!(sent, 32_000);
}

/// With no output limit known — no handle at all, or a model the handle
/// has a window for but no limit — the cap stays at 4096.
#[tokio::test(flavor = "current_thread")]
async fn an_unknown_output_limit_falls_back_to_4096() {
    let no_handle = sent_max_tokens("max-tokens-no-handle", "mock-model", |e| e, None).await;
    assert_eq!(no_handle, 4096);

    let window_only = PruneLevelHandle::with_model_context_windows(
        PruneLevel::Conservative,
        200_000,
        [("windowed-model", 400_000)],
    );
    let windowed = sent_max_tokens(
        "max-tokens-window-only",
        "windowed-model",
        |executor| executor.with_prune_level(window_only),
        None,
    )
    .await;
    assert_eq!(windowed, 4096);
}

/// An explicit cap wins over the model's limit: the request's own first,
/// then the executor's.
#[tokio::test(flavor = "current_thread")]
async fn an_explicit_cap_beats_the_models_output_limit() {
    let from_executor = sent_max_tokens(
        "max-tokens-executor-cap",
        "mock-model",
        |executor| {
            executor
                .with_prune_level(output_limits_handle())
                .with_max_tokens(8192)
        },
        None,
    )
    .await;
    assert_eq!(from_executor, 8192);

    let from_request = sent_max_tokens(
        "max-tokens-request-cap",
        "mock-model",
        |executor| {
            executor
                .with_prune_level(output_limits_handle())
                .with_max_tokens(8192)
        },
        Some(5000),
    )
    .await;
    assert_eq!(from_request, 5000);
}

#[tokio::test(flavor = "current_thread")]
async fn anchored_bootstrap_sends_the_upstream_minimal_persona_and_pair() {
    let _guard = env_lock();
    let engine = Arc::new(Engine::with_builtin_tools());
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "answer"));
    let captured = mock.clone();
    let client: Arc<dyn ModelClient> = Arc::new(AnchoredMockClient::new(mock));
    let projects_root_dir = temp_projects_root("anchored-bootstrap-identity");
    let projects_root = projects_root_dir.path();
    let cwd = projects_root.to_string_lossy().to_string();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let session = state.create_session(cwd.clone(), Vec::new());
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system_prompt_config(test_system_prompt_config())
        .with_capability_mode(AgentCapabilityMode::Minimal)
        .with_max_tokens(4096)
        .with_server_state(state);

    executor
        .execute(test_prompt_request(&session.id, &cwd, "first", None))
        .await
        .unwrap();

    let requests = captured.captured_requests();
    let bootstrap = &requests[0];
    assert_eq!(
        bootstrap.system.as_deref(),
        Some(crate::anchored_minimal::ANCHORED_MINIMAL_PERSONA),
        "an anchored session must not carry Rebon's tool-naming Minimal persona"
    );
    assert_eq!(
        bootstrap
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["bash", "str_replace_editor"]
    );
    let bash = &bootstrap.tools[0];
    assert_eq!(bash.input_schema["required"], json!(["command"]));
    assert!(bash.input_schema.get("additionalProperties").is_none());
    assert_eq!(
        bootstrap.tools[1].input_schema["required"],
        json!(["command", "path"])
    );
}

#[test]
fn anchored_bootstrap_names_resolve_to_real_tools() {
    let engine = Engine::with_builtin_tools();
    for tool in crate::anchored_minimal::anchored_bootstrap_tools() {
        assert!(
            engine.find_tool(&tool.name).is_some(),
            "anchored bootstrap advertises `{}` but nothing dispatches it",
            tool.name
        );
    }
}
