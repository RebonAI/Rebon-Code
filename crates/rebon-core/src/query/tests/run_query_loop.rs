use super::*;

#[tokio::test]
async fn run_query_applies_model_context_window_before_pre_turn_compaction() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);

    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "ok"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());

    let mut messages = vec![ApiMessage::user_text("original")];
    for _ in 0..8 {
        messages.push(ApiMessage::assistant_text("reply"));
        messages.push(ApiMessage::user_text("x".repeat(16_000)));
    }
    let handle = PruneLevelHandle::with_model_context_windows(
        PruneLevel::Conservative,
        80_000,
        [("large-model", 1_000_000)],
    );
    handle.report_usage(79_000);
    let params = QueryParams {
        max_tokens: 4_096,
        prune_level: Some(handle.clone()),
        ..QueryParams::new("large-model", messages)
    };

    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert_eq!(handle.budget.context_window(), 1_000_000);
    let captured = mock.captured_requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].messages.len(), 17);
}

#[tokio::test]
async fn run_query_uses_client_prune_handle_for_worker_context_window() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);

    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "ok"));
    let base: Arc<dyn ModelClient> = Arc::new(mock.clone());
    let handle = PruneLevelHandle::with_model_context_windows(
        PruneLevel::Conservative,
        80_000,
        [("large-model", 1_000_000)],
    );
    handle.report_usage(79_000);
    let client: Arc<dyn ModelClient> = Arc::new(rebon_api::ContextPruneMiddleware::wrap(
        base,
        rebon_api::ContextPruneConfig::default(),
        handle.clone(),
    ));

    let mut messages = vec![ApiMessage::user_text("original")];
    for _ in 0..8 {
        messages.push(ApiMessage::assistant_text("reply"));
        messages.push(ApiMessage::user_text("x".repeat(16_000)));
    }
    let params = QueryParams {
        max_tokens: 4_096,
        ..QueryParams::new("large-model", messages)
    };

    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert_eq!(handle.budget.context_window(), 1_000_000);
    let captured = mock.captured_requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].messages.len(), 17);
}

#[tokio::test]
async fn run_query_single_text_turn_emits_done() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);

    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "hello world"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("say hello")]);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    // Expect at least one stream event + IterationComplete + Done.
    assert!(events
        .iter()
        .any(|e| matches!(e, QueryEvent::Stream(StreamEvent::MessageStart { .. }))));
    let done = events.last().expect("stream ends with Done");
    match done {
        QueryEvent::Done {
            final_message,
            stop_reason,
            ..
        } => {
            assert_eq!(final_message.text(), "hello world");
            assert_eq!(*stop_reason, StopReason::EndTurn);
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn terminal_hook_forces_tool_choice_only_when_supported() {
    let fired = Arc::new(AtomicBool::new(false));

    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.set_supports_forced_tool_choice(true);
    client.push_turn(text_turn("msg_1", "missing data"));
    client.push_turn(text_turn("msg_2", "done"));
    let mock = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("schema agent")])
        .with_turn_hook(
            "tests/forced-structured-output",
            crate::turn_hook::Order::LAST,
            test_turn_end_hook(move |event, context| {
                if fired.swap(true, Ordering::SeqCst) {
                    return;
                }
                continue_terminal_turn(
                    event,
                    context,
                    ApiMessage::user_text("call StructuredOutput now"),
                    Some(ToolChoice::Tool {
                        name: rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME.to_string(),
                    }),
                );
            }),
        );

    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let _ = drain(&mut rx).await;
    let captured = mock.captured_requests();
    assert_eq!(captured.len(), 2);
    assert!(
        captured[0].tool_choice.is_none(),
        "initial request must not force StructuredOutput"
    );
    assert_eq!(
        captured[1].tool_choice,
        Some(ToolChoice::Tool {
            name: rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME.to_string()
        })
    );
}

#[tokio::test]
async fn terminal_hook_forced_tool_choice_degrades_when_unsupported() {
    let fired = Arc::new(AtomicBool::new(false));

    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "missing data"));
    client.push_turn(text_turn("msg_2", "done"));
    let mock = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("schema agent")])
        .with_turn_hook(
            "tests/forced-structured-output",
            crate::turn_hook::Order::LAST,
            test_turn_end_hook(move |event, context| {
                if fired.swap(true, Ordering::SeqCst) {
                    return;
                }
                continue_terminal_turn(
                    event,
                    context,
                    ApiMessage::user_text("call StructuredOutput now"),
                    Some(ToolChoice::Tool {
                        name: rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME.to_string(),
                    }),
                );
            }),
        );

    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let _ = drain(&mut rx).await;
    let captured = mock.captured_requests();
    assert_eq!(captured.len(), 2);
    assert!(captured[0].tool_choice.is_none());
    assert!(captured[1].tool_choice.is_none());
}

#[tokio::test]
async fn run_query_context_overflow_retries_with_smaller_history() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);

    let client = MockModelClient::new();
    client.push_error(ModelError::Permanent(
        "ws error (context_length_exceeded): Your input exceeds the context window of this model"
            .into(),
    ));
    client.push_turn(text_turn("msg_retry", "recovered"));
    let mock = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let mut messages = vec![ApiMessage::user_text("original")];
    for i in 0..8 {
        messages.push(ApiMessage::assistant_text(format!("reply-{i}")));
        messages.push(ApiMessage::user_text("x".repeat(16_000)));
    }
    let handle = PruneLevelHandle::with_context_window(PruneLevel::Conservative, 80_000);
    let params = QueryParams {
        max_tokens: 4_096,
        prune_level: Some(handle),
        ..QueryParams::new("mock", messages)
    };

    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert_eq!(mock.call_count(), 2);
    assert!(mock.invalidate_previous_response_id_count() >= 1);
    let captured = mock.captured_requests();
    let first = crate::context_accounting::estimate_messages_input_tokens(
        captured[0].system.as_deref(),
        &captured[0].messages,
    );
    let second = crate::context_accounting::estimate_messages_input_tokens(
        captured[1].system.as_deref(),
        &captured[1].messages,
    );
    assert!(
        second < first,
        "retry should send less context: {first} -> {second}"
    );
}

#[tokio::test]
async fn run_query_repeated_context_overflow_keeps_shrinking_instead_of_failing() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);

    let client = MockModelClient::new();
    client.push_error(ModelError::Permanent(
        "ws error (context_length_exceeded): Your input exceeds the context window of this model"
            .into(),
    ));
    client.push_error(ModelError::Permanent(
        "ws error (context_length_exceeded): Your input exceeds the context window of this model"
            .into(),
    ));
    client.push_turn(text_turn("msg_retry", "recovered"));
    let mock = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let mut messages = vec![ApiMessage::user_text("original")];
    for i in 0..20 {
        messages.push(ApiMessage::assistant_text(format!("reply-{i}")));
        messages.push(ApiMessage::user_text("x".repeat(16_000)));
    }
    let handle = PruneLevelHandle::with_context_window(PruneLevel::Conservative, 80_000);
    let params = QueryParams {
        max_tokens: 4_096,
        prune_level: Some(handle),
        ..QueryParams::new("mock", messages)
    };

    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert_eq!(mock.call_count(), 3);
    let captured = mock.captured_requests();
    let first = crate::context_accounting::estimate_messages_input_tokens(
        captured[0].system.as_deref(),
        &captured[0].messages,
    );
    let second = crate::context_accounting::estimate_messages_input_tokens(
        captured[1].system.as_deref(),
        &captured[1].messages,
    );
    let third = crate::context_accounting::estimate_messages_input_tokens(
        captured[2].system.as_deref(),
        &captured[2].messages,
    );
    assert!(
        second < first,
        "first retry should shrink: {first} -> {second}"
    );
    assert!(
        third < second,
        "second retry should shrink again: {second} -> {third}"
    );
}

#[tokio::test]
async fn run_query_replaces_dynamic_transient_context_each_iteration() {
    struct SequencedTransientPoller(AtomicUsize);

    impl AttachmentPoller for SequencedTransientPoller {
        fn poll(&self, _request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
            Vec::new()
        }

        fn transient_context(&self) -> Option<String> {
            Some(format!("roster-{}", self.0.fetch_add(1, Ordering::SeqCst)))
        }
    }

    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "file body"})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_roster",
        "{\"path\":\"a.rs\"}",
    ));
    client.push_turn(text_turn("msg_done", "done"));
    let captured_client = client.clone();
    let params = QueryParams {
        transient_context_message: Some("base-transient".into()),
        attachment_poller: Some(AttachmentPollerBinding::new(
            Arc::new(SequencedTransientPoller(AtomicUsize::new(0))),
            "session-transient",
            "turn-transient",
        )),
        ..QueryParams::new("mock", vec![ApiMessage::user_text("delegate if useful")])
            .with_tools(tools_from_engine(&engine))
    };

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    let requests = captured_client.captured_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].transient_context.as_deref(),
        Some("base-transient\n\nroster-0")
    );
    assert_eq!(
        requests[1].transient_context.as_deref(),
        Some("base-transient\n\nroster-1")
    );
    assert!(!requests[1]
        .transient_context
        .as_deref()
        .unwrap()
        .contains("roster-0"));
    assert!(requests.iter().all(|request| {
        text_message_texts(&request.messages)
            .into_iter()
            .all(|text| !text.contains("roster-"))
    }));
}

#[tokio::test]
async fn run_query_executes_tool_and_continues_with_result() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "file body"})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let client = MockModelClient::new();
    // First turn: assistant decides to call Read.
    client.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_1",
        "{\"path\":\"a.rs\"}",
    ));
    // Second turn: assistant emits final text using the tool result.
    client.push_turn(text_turn("msg_done", "saw file body"));
    let client: Arc<dyn ModelClient> = Arc::new(client.clone());

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    // Tool should have been invoked exactly once with the parsed input.
    assert_eq!(tool.call_count(), 1);
    // Tool dispatch events should be present.
    let dispatch_start = events.iter().find(|e| {
        matches!(
            e,
            QueryEvent::ToolDispatchStart { name, .. } if name == "Read"
        )
    });
    assert!(dispatch_start.is_some());
    let dispatch_result = events.iter().find(|e| {
        matches!(
            e,
            QueryEvent::ToolDispatchResult {
                name,
                outcome: Ok(_),
                ..
            } if name == "Read"
        )
    });
    assert!(dispatch_result.is_some());

    // Final Done event should report the second turn's message.
    let done = events.last().expect("stream ends with Done");
    match done {
        QueryEvent::Done { final_message, .. } => {
            assert_eq!(final_message.text(), "saw file body");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn run_query_auto_continues_text_after_max_tokens() {
    let engine = Arc::new(Engine::new());
    let mock = MockModelClient::new();
    mock.push_turn(text_turn_with_stop(
        "msg_part_1",
        "{\"items\":[",
        StopReason::MaxTokens,
    ));
    mock.push_turn(text_turn("msg_part_2", "1,2,3]}"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());

    let params =
        QueryParams::new("mock", vec![ApiMessage::user_text("emit json")]).with_max_iterations(3);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let iterations = events
        .iter()
        .filter(|event| matches!(event, QueryEvent::IterationComplete { .. }))
        .count();
    assert_eq!(iterations, 1);
    let done = events.last().expect("terminal event");
    match done {
        QueryEvent::Done {
            final_message,
            stop_reason,
            ..
        } => {
            assert_eq!(*stop_reason, StopReason::EndTurn);
            assert_eq!(final_message.text(), "{\"items\":[1,2,3]}");
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let requests = mock.captured_requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]
        .messages
        .last()
        .and_then(|message| message.content.first())
        .and_then(ApiContentBlock::as_text)
        .is_some_and(|text| text.contains("Continue exactly where")));
}

#[tokio::test]
async fn run_query_auto_continues_encrypted_thinking_after_max_tokens() {
    let engine = Arc::new(Engine::new());
    let mock = MockModelClient::new();
    mock.push_turn(thinking_turn_with_stop(
        "msg_part_1",
        "planning the next edit",
        Some("encrypted-reasoning"),
        None,
        StopReason::MaxTokens,
    ));
    mock.push_turn(text_turn("msg_part_2", "finished"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("finish the edit")])
        .with_max_iterations(3);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let done = events.last().expect("terminal event");
    match done {
        QueryEvent::Done {
            final_message,
            stop_reason,
            ..
        } => {
            assert_eq!(*stop_reason, StopReason::EndTurn);
            assert_eq!(final_message.text(), "finished");
            assert!(final_message.content.iter().any(|block| matches!(
                block,
                ApiContentBlock::Thinking(thinking)
                    if thinking.data.as_deref() == Some("encrypted-reasoning")
            )));
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let requests = mock.captured_requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ApiContentBlock::Thinking(thinking)
                    if thinking.data.as_deref() == Some("encrypted-reasoning")
            )
        })
    }));
}

#[tokio::test]
async fn run_query_auto_continues_signed_thinking_after_max_tokens() {
    let engine = Arc::new(Engine::new());
    let mock = MockModelClient::new();
    mock.push_turn(thinking_turn_with_stop(
        "msg_part_1",
        "planning the next edit",
        None,
        Some("thinking-signature"),
        StopReason::MaxTokens,
    ));
    mock.push_turn(text_turn("msg_part_2", "finished"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("finish the edit")])
        .with_max_iterations(3);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(
        events.last(),
        Some(QueryEvent::Done {
            stop_reason: StopReason::EndTurn,
            ..
        })
    ));
    let requests = mock.captured_requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ApiContentBlock::Thinking(thinking)
                    if thinking.signature.as_deref() == Some("thinking-signature")
            )
        })
    }));
}

#[tokio::test]
async fn run_query_does_not_continue_unsigned_thinking_after_max_tokens() {
    let engine = Arc::new(Engine::new());
    let mock = MockModelClient::new();
    mock.push_turn(thinking_turn_with_stop(
        "msg_part_1",
        "planning the next edit",
        None,
        None,
        StopReason::MaxTokens,
    ));
    mock.push_turn(text_turn("msg_part_2", "must not be requested"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("finish the edit")])
        .with_max_iterations(3);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(
        events.last(),
        Some(QueryEvent::Done {
            stop_reason: StopReason::MaxTokens,
            ..
        })
    ));
    assert_eq!(mock.captured_requests().len(), 1);
}

#[tokio::test]
async fn run_query_continues_unsigned_thinking_when_provider_allows_replay() {
    // Providers that drop unreplayable reasoning instead of rejecting
    // it never emit a signature at all (OpenAI-style dialects, every
    // plugin provider). Gating continuation on one there strands the
    // turn: a reasoning-heavy model that spends its whole output
    // budget thinking would end with nothing emitted and no retry.
    let engine = Arc::new(Engine::new());
    let mock = MockModelClient::new();
    mock.set_thinking_replay_requires_signature(false);
    mock.push_turn(thinking_turn_with_stop(
        "msg_part_1",
        "planning the next edit",
        None,
        None,
        StopReason::MaxTokens,
    ));
    mock.push_turn(text_turn("msg_part_2", "the continued answer"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("finish the edit")])
        .with_max_iterations(3);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    match events.last().expect("terminal event") {
        QueryEvent::Done { final_message, .. } => {
            assert_eq!(final_message.text(), "the continued answer");
        }
        other => panic!("expected Done, got {other:?}"),
    }
    assert_eq!(mock.captured_requests().len(), 2);
}

#[tokio::test]
async fn run_query_emits_truncated_text_when_max_tokens_hits_final_iteration() {
    let engine = Arc::new(Engine::new());
    let mock = MockModelClient::new();
    mock.push_turn(text_turn_with_stop(
        "msg_part_1",
        "{\"items\":[",
        StopReason::MaxTokens,
    ));
    // A continuation turn that must never be requested: the iteration
    // budget is exhausted, so the truncated text is the final answer.
    mock.push_turn(text_turn("msg_part_2", "1,2,3]}"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());

    let params =
        QueryParams::new("mock", vec![ApiMessage::user_text("emit json")]).with_max_iterations(1);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let done = events.last().expect("terminal event");
    match done {
        QueryEvent::Done {
            final_message,
            stop_reason,
            ..
        } => {
            assert_eq!(*stop_reason, StopReason::MaxTokens);
            assert_eq!(final_message.text(), "{\"items\":[");
        }
        other => panic!("expected Done, got {other:?}"),
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, QueryEvent::Error(_))),
        "truncated final response must not surface as an error"
    );
    assert_eq!(mock.captured_requests().len(), 1);
}

#[tokio::test]
async fn run_query_hands_a_truncated_tool_call_back_to_the_model() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let mock = MockModelClient::new();
    mock.push_turn(truncated_tool_turn("msg_tool", "toolu_1"));
    mock.push_turn(text_turn("msg_retry", "splitting the work"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert_eq!(
        tool.call_count(),
        0,
        "a cut-off input never reaches the tool"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        QueryEvent::ToolDispatchResult { tool_use_id, outcome: Err(err), .. }
            if tool_use_id == "toolu_1" && err.contains("cut off by the output token limit")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, QueryEvent::Error(_))),
        "one truncation is recovered, not surfaced as a failed turn"
    );
    assert!(matches!(
        events.last(),
        Some(QueryEvent::Done { final_message, .. }) if final_message.text() == "splitting the work"
    ));

    let requests = mock.captured_requests();
    assert_eq!(requests.len(), 2);
    let (replayed, result) = replayed_tool_call(&requests[1].messages, "toolu_1");
    assert_eq!(
        replayed.input,
        json!({}),
        "the partial input never goes back on the wire"
    );
    assert!(result.is_error);
    assert!(result
        .content
        .as_text()
        .expect("text result")
        .contains("cut off by the output token limit"));
    assert_eq!(
        mock.invalidate_previous_response_id_count(),
        1,
        "the server-side continuation points at a call that never closed"
    );
}

#[tokio::test]
async fn run_query_runs_the_complete_calls_beside_a_truncated_one() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let mock = MockModelClient::new();
    mock.push_turn(cut_off_by_max_tokens(two_tool_turn(
        "msg_tool",
        ("Read", "toolu_done", "{\"path\":\"a.rs\"}"),
        ("Read", "toolu_cut", "{\"path\":"),
    )));
    mock.push_turn(text_turn("msg_retry", "done"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read both")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert_eq!(
        tool.call_count(),
        1,
        "a call whose input closed before the cut is complete and runs"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        QueryEvent::ToolDispatchResult { tool_use_id, outcome: Ok(_), .. }
            if tool_use_id == "toolu_done"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        QueryEvent::ToolDispatchResult { tool_use_id, outcome: Err(err), .. }
            if tool_use_id == "toolu_cut" && err.contains("cut off by the output token limit")
    )));

    let requests = mock.captured_requests();
    assert_eq!(requests.len(), 2);
    let (done, done_result) = replayed_tool_call(&requests[1].messages, "toolu_done");
    assert_eq!(done.input, json!({"path": "a.rs"}));
    assert!(!done_result.is_error);
    let (cut, cut_result) = replayed_tool_call(&requests[1].messages, "toolu_cut");
    assert_eq!(cut.input, json!({}));
    assert!(cut_result.is_error);
}

#[tokio::test]
async fn run_query_gives_up_after_repeated_truncated_tool_calls() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let mock = MockModelClient::new();
    mock.push_turn(truncated_tool_turn("msg_0", "toolu_0"));
    mock.push_turn(truncated_tool_turn("msg_1", "toolu_1"));
    mock.push_turn(truncated_tool_turn("msg_2", "toolu_2"));
    // Never requested: the third cut in a row ends the turn.
    mock.push_turn(text_turn("msg_never", "unreachable"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert_eq!(tool.call_count(), 0);
    assert_eq!(
        mock.captured_requests().len(),
        3,
        "two retries, then the turn stops"
    );
    assert!(
        matches!(events.last(), Some(QueryEvent::Error(message)) if message.contains("truncated while emitting a tool call"))
    );
    // The final cut-off message is still committed ahead of its results
    // so the transcript pairs the tool_use with the error.
    let committed = events
        .iter()
        .position(|event| {
            matches!(event, QueryEvent::IterationComplete { message, .. } if message.id == "msg_2")
        })
        .expect("the last assistant message is committed before the turn fails");
    let answered = events
        .iter()
        .position(|event| {
            matches!(event, QueryEvent::ToolDispatchResult { tool_use_id, outcome: Err(_), .. } if tool_use_id == "toolu_2")
        })
        .expect("the last tool call is answered with an error");
    assert!(committed < answered);
    assert!(events.iter().any(|event| matches!(
        event,
        QueryEvent::IterationComplete { message, .. }
            if message.id == "msg_2" && message.tool_uses().all(|tool_use| tool_use.input == json!({}))
    )));
}

#[tokio::test]
async fn run_query_resets_the_truncation_budget_after_a_clean_tool_round() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let mock = MockModelClient::new();
    mock.push_turn(truncated_tool_turn("msg_0", "toolu_0"));
    mock.push_turn(tool_turn("msg_1", "Read", "toolu_1", "{\"path\":\"a.rs\"}"));
    mock.push_turn(truncated_tool_turn("msg_2", "toolu_2"));
    mock.push_turn(truncated_tool_turn("msg_3", "toolu_3"));
    mock.push_turn(text_turn("msg_4", "done"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert_eq!(tool.call_count(), 1);
    assert_eq!(mock.captured_requests().len(), 5);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, QueryEvent::Error(_))),
        "a clean round in between means the cuts were not consecutive"
    );
    assert!(matches!(
        events.last(),
        Some(QueryEvent::Done { final_message, .. }) if final_message.text() == "done"
    ));
}

#[tokio::test]
async fn run_query_surfaces_tool_execution_error_as_tool_result() {
    let tool = Arc::new(RecordingTool::failing("Read"));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_bad",
        "Read",
        "toolu_1",
        "{\"path\":\"a.rs\"}",
    ));
    client.push_turn(text_turn("msg_recover", "sorry, I could not read"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    // The failing tool should surface as a ToolDispatchResult(Err)
    // and the loop should continue to the recovery turn.
    let failure = events.iter().find_map(|e| match e {
        QueryEvent::ToolDispatchResult {
            outcome: Err(err), ..
        } => Some(err.clone()),
        _ => None,
    });
    assert!(failure.is_some());
    assert!(failure.unwrap().contains("execution failed"));

    let done = events.last().expect("ends with Done");
    match done {
        QueryEvent::Done { final_message, .. } => {
            assert!(final_message.text().contains("sorry"));
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn run_query_keeps_model_error_detail_separate_from_display_message() {
    let engine = build_engine_with(Arc::new(PresentedErrorTool));
    let mock = MockModelClient::new();
    mock.push_turn(tool_turn("msg_send", "SendMessage", "toolu_send", "{}"));
    mock.push_turn(text_turn("msg_recover", "continued"));
    let client_handle = mock.clone();
    let client: Arc<dyn ModelClient> = Arc::new(mock);

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("send")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let presentation = events.iter().find_map(|event| match event {
        QueryEvent::ToolDispatchResult {
            outcome: Err(model_message),
            error_presentation: Some(presentation),
            ..
        } => {
            assert_eq!(model_message, &presentation.model_message);
            Some(presentation)
        }
        _ => None,
    });
    let presentation = presentation.expect("structured tool error event");
    assert_eq!(
        presentation.display_message,
        "Agent is no longer available."
    );
    assert!(presentation.model_message.contains("spawn a fresh worker"));

    let captured = client_handle.captured_requests();
    assert!(matches!(
        &captured[1].messages.last().unwrap().content[0],
        ApiContentBlock::ToolResult(result)
            if result.content.as_text().is_some_and(|text| text == presentation.model_message)
    ));
}

#[tokio::test]
async fn run_query_reports_repeated_transient_start_failure_after_partial_turn() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "hi"})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let mock = MockModelClient::new();
    mock.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_1",
        "{\"path\":\"a.rs\"}",
    ));
    mock.push_error(ModelError::transient("network down"));
    mock.push_error(ModelError::transient("network down"));
    let client: Arc<dyn ModelClient> = Arc::new(mock);

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine))
        .with_max_iterations(3);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let last = events.last().expect("terminal event");
    match last {
        QueryEvent::Error(message) => {
            assert!(message.contains("model stream start failed"), "{message}");
            assert!(message.contains("network down"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn turn_budget_warning_is_owned_by_turn_hook_seat() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_budget",
        "{\"path\":\"a.rs\"}",
    ));
    client.push_turn(text_turn("msg_done", "done"));
    let captured_client = client.clone();
    let mut params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine))
        .with_max_iterations(11);
    params.turn_hooks = crate::turn_hook::TurnHooks::without_seat_for_test();

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    let requests = captured_client.captured_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(turn_budget_warning_count(&requests[1].messages), 0);
}

#[tokio::test]
async fn turn_budget_warning_is_visible_once_in_original_history_position() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    for iteration in 0..4 {
        client.push_turn(tool_turn(
            &format!("msg_tool_{iteration}"),
            "Read",
            &format!("toolu_budget_{iteration}"),
            "{\"path\":\"a.rs\"}",
        ));
    }
    client.push_turn(text_turn("msg_done", "done"));
    let captured_client = client.clone();
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine))
        .with_max_iterations(13);

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    let requests = captured_client.captured_requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(turn_budget_warning_count(&requests[2].messages), 0);

    let threshold_history = &requests[3].messages;
    assert_eq!(turn_budget_warning_count(threshold_history), 1);
    assert_eq!(
        threshold_history.last(),
        Some(&ApiMessage::user_text(turn_budget_warning_text(13)))
    );
    assert!(matches!(
        threshold_history
            .get(threshold_history.len().saturating_sub(2))
            .and_then(|message| message.content.first()),
        Some(ApiContentBlock::ToolResult(result)) if result.tool_use_id == "toolu_budget_2"
    ));

    assert_eq!(turn_budget_warning_count(&requests[4].messages), 1);
}

#[tokio::test]
async fn turn_budget_warning_stays_absent_for_small_iteration_limit() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let engine = build_engine_with(tool.clone() as Arc<dyn Tool>);
    let client = MockModelClient::new();
    for iteration in 0..3 {
        client.push_turn(tool_turn(
            &format!("msg_small_{iteration}"),
            "Read",
            &format!("toolu_small_{iteration}"),
            "{\"path\":\"a.rs\"}",
        ));
    }
    let captured_client = client.clone();
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine))
        .with_max_iterations(2);

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(events
        .iter()
        .any(|event| matches!(event, QueryEvent::IterationLimitReached { iterations: 2 })));
    assert_eq!(tool.call_count(), 2);
    let requests = captured_client.captured_requests();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|request| turn_budget_warning_count(&request.messages) == 0));
}

#[tokio::test(start_paused = true)]
async fn turn_budget_warning_is_not_shifted_by_max_tokens_or_transient_retry() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let engine = build_engine_with(tool.clone() as Arc<dyn Tool>);

    let max_tokens_client = MockModelClient::new();
    max_tokens_client.push_turn(text_turn_with_stop(
        "msg_partial",
        "partial",
        StopReason::MaxTokens,
    ));
    max_tokens_client.push_turn(tool_turn(
        "msg_tool_after_continuation",
        "Read",
        "toolu_after_continuation",
        "{\"path\":\"a.rs\"}",
    ));
    max_tokens_client.push_turn(text_turn("msg_done_after_continuation", "done"));
    let max_tokens_capture = max_tokens_client.clone();
    let max_tokens_params = QueryParams::new("mock", vec![ApiMessage::user_text("continue")])
        .with_tools(tools_from_engine(&engine))
        .with_max_iterations(11);
    let mut max_tokens_rx = run_query(
        engine.clone(),
        SessionHandle::new(Arc::new(max_tokens_client)),
        max_tokens_params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let max_tokens_events = drain(&mut max_tokens_rx).await;
    assert!(matches!(
        max_tokens_events.last(),
        Some(QueryEvent::Done { .. })
    ));
    let max_tokens_requests = max_tokens_capture.captured_requests();
    assert_eq!(max_tokens_requests.len(), 3);
    assert!(max_tokens_requests[1]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(ApiContentBlock::as_text)
        .any(|text| text.contains("Continue exactly where")));
    assert!(max_tokens_requests
        .iter()
        .all(|request| turn_budget_warning_count(&request.messages) == 0));

    let retry_client = MockModelClient::new();
    retry_client.push_error(ModelError::transient("retry once"));
    retry_client.push_turn(tool_turn(
        "msg_tool_after_retry",
        "Read",
        "toolu_after_retry",
        "{\"path\":\"a.rs\"}",
    ));
    retry_client.push_turn(text_turn("msg_done_after_retry", "done"));
    let retry_capture = retry_client.clone();
    let retry_params = QueryParams::new("mock", vec![ApiMessage::user_text("retry")])
        .with_tools(tools_from_engine(&engine))
        .with_max_iterations(11);
    let mut retry_rx = run_query(
        engine,
        SessionHandle::new(Arc::new(retry_client)),
        retry_params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let retry_events = drain(&mut retry_rx).await;
    assert!(matches!(retry_events.last(), Some(QueryEvent::Done { .. })));
    let retry_requests = retry_capture.captured_requests();
    assert_eq!(retry_requests.len(), 3);
    assert!(retry_requests
        .iter()
        .all(|request| turn_budget_warning_count(&request.messages) == 0));
}

#[tokio::test]
async fn turn_budget_phase_skips_terminal_cancel_and_hard_error_paths() {
    let terminal_client = MockModelClient::new();
    terminal_client.push_turn(text_turn("msg_terminal", "done"));
    let (terminal_seat, terminal_calls) = turn_budget_recording_seat();
    let terminal_params = QueryParams::new("mock", vec![ApiMessage::user_text("finish")])
        .with_turn_hook_seat(terminal_seat)
        .with_max_iterations(11);
    let mut terminal_rx = run_query(
        build_engine_with_tools(Vec::new()),
        SessionHandle::new(Arc::new(terminal_client)),
        terminal_params,
        ToolContext::new(),
        CancelToken::new(),
    );
    let terminal_events = drain(&mut terminal_rx).await;
    assert!(matches!(
        terminal_events.last(),
        Some(QueryEvent::Done { .. })
    ));
    assert_eq!(terminal_calls.load(Ordering::SeqCst), 0);

    let error_client = MockModelClient::new();
    error_client.push_error(ModelError::other("hard failure"));
    let (error_seat, error_calls) = turn_budget_recording_seat();
    let error_params = QueryParams::new("mock", vec![ApiMessage::user_text("fail")])
        .with_turn_hook_seat(error_seat)
        .with_max_iterations(11);
    let mut error_rx = run_query(
        build_engine_with_tools(Vec::new()),
        SessionHandle::new(Arc::new(error_client)),
        error_params,
        ToolContext::new(),
        CancelToken::new(),
    );
    let error_events = drain(&mut error_rx).await;
    assert!(matches!(error_events.last(), Some(QueryEvent::Error(_))));
    assert_eq!(error_calls.load(Ordering::SeqCst), 0);

    let cancelled_client = MockModelClient::new();
    cancelled_client.push_turn(text_turn("msg_unused", "unused"));
    let (cancelled_seat, cancelled_calls) = turn_budget_recording_seat();
    let cancelled_params = QueryParams::new("mock", vec![ApiMessage::user_text("cancel")])
        .with_turn_hook_seat(cancelled_seat)
        .with_max_iterations(11);
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut cancelled_rx = run_query(
        build_engine_with_tools(Vec::new()),
        SessionHandle::new(Arc::new(cancelled_client)),
        cancelled_params,
        ToolContext::new(),
        cancel,
    );
    let cancelled_events = drain(&mut cancelled_rx).await;
    assert!(matches!(
        cancelled_events.last(),
        Some(QueryEvent::Cancelled)
    ));
    assert_eq!(cancelled_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn run_query_respects_max_iterations() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    // Model keeps asking for the same tool forever.
    let client = MockModelClient::new();
    for _ in 0..5 {
        client.push_turn(tool_turn(
            "msg_loop",
            "Read",
            "toolu_1",
            "{\"path\":\"a.rs\"}",
        ));
    }
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("spin forever")])
        .with_tools(tools_from_engine(&engine))
        .with_max_iterations(3);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let limited = events
        .iter()
        .any(|e| matches!(e, QueryEvent::IterationLimitReached { iterations: 3 }));
    assert!(limited, "expected IterationLimitReached event");
    assert_eq!(tool.call_count(), 3);
}

#[tokio::test]
async fn run_query_forwards_attachment_poller_output_between_iterations() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    // Two iterations: first one uses the tool, second one ends.
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_1",
        "{\"path\":\"a.rs\"}",
    ));
    client.push_turn(text_turn("msg_done", "all set"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let injection = ApiMessage::user_text("<system-reminder>\nmode=plan\n</system-reminder>");
    let poller = Arc::new(ScriptedPoller::with_messages(vec![vec![injection.clone()]]));
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_attachment_poller(
            poller.clone() as Arc<dyn crate::query::AttachmentPoller>,
            "session-poller",
            "turn-poller",
        );
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    // AttachmentInjected event fires once with the scripted
    // message and iteration == 1 (right before the 2nd turn).
    let injected = events
        .iter()
        .find_map(|e| match e {
            QueryEvent::AttachmentInjected { iteration, message } => {
                Some((*iteration, message.clone()))
            }
            _ => None,
        })
        .expect("AttachmentInjected event");
    assert_eq!(injected.0, 1);
    assert_eq!(injected.1, injection);

    // The poller was called exactly once (after iteration 0's
    // tool round, before iteration 1's model call). It should
    // NOT be called after the final iteration, because the loop
    // exits on Done before reaching the injector.
    let calls = poller.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0],
        (
            "session-poller".to_string(),
            "turn-poller".to_string(),
            1,
            AttachmentPollPhase::Regular,
        )
    );

    assert!(!injected
            .1
            .content
            .iter()
            .any(|block| matches!(block, ApiContentBlock::Text(text) if text.text.starts_with("<rebon-queued-user-input uuid=\""))));
    // Final Done event is the second turn.
    let done = events.last().expect("stream ends with Done");
    match done {
        QueryEvent::Done { final_message, .. } => {
            assert_eq!(final_message.text(), "all set");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn run_query_eager_poller_injects_terminal_notification_before_first_request() {
    let engine = build_engine_with(Arc::new(RecordingTool::new("Read", json!({"ok": 1}))));
    let mock = Arc::new(MockModelClient::new());
    mock.push_turn(text_turn("msg_done", "handled prior failure"));
    let client: Arc<dyn ModelClient> = mock.clone();
    let notification = "<task-notification><status>failed</status><summary>unsupported model</summary></task-notification>";
    let poller = Arc::new(EagerPoller::new(vec![vec![ApiMessage::user_text(
        notification,
    )]]));
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("continue")])
        .with_attachment_poller(
            poller.clone() as Arc<dyn crate::query::AttachmentPoller>,
            "session-poller",
            "turn-poller",
        );
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );

    let _events = drain(&mut rx).await;
    let requests = mock.captured_requests();

    assert_eq!(requests.len(), 1);
    assert!(text_message_texts(&requests[0].messages)
        .iter()
        .any(|text| text.contains("unsupported model")));
    assert_eq!(poller.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn run_query_eager_poller_continues_after_terminal_notification_arrives_during_request() {
    let engine = build_engine_with(Arc::new(RecordingTool::new("Read", json!({"ok": 1}))));
    let mock = Arc::new(MockModelClient::new());
    mock.push_turn(text_turn("msg_waiting", "worker may still be running"));
    mock.push_turn(text_turn("msg_done", "reported worker failure"));
    let client: Arc<dyn ModelClient> = mock.clone();
    let notification = "<task-notification><status>failed</status><summary>unsupported model</summary></task-notification>";
    let poller = Arc::new(EagerPoller::new(vec![
        Vec::new(),
        vec![ApiMessage::user_text(notification)],
        Vec::new(),
    ]));
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("wait for worker")])
        .with_attachment_poller(
            poller.clone() as Arc<dyn crate::query::AttachmentPoller>,
            "session-poller",
            "turn-poller",
        );
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );

    let events = drain(&mut rx).await;
    let requests = mock.captured_requests();

    assert_eq!(requests.len(), 2);
    assert!(text_message_texts(&requests[1].messages)
        .iter()
        .any(|text| text.contains("unsupported model")));
    assert!(matches!(
        events.last(),
        Some(QueryEvent::Done { final_message, .. }) if final_message.text() == "reported worker failure"
    ));
    assert_eq!(poller.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn run_query_eager_poller_extends_last_iteration_for_terminal_notification() {
    let engine = build_engine_with(Arc::new(RecordingTool::new("Read", json!({"ok": 1}))));
    let mock = Arc::new(MockModelClient::new());
    mock.push_turn(text_turn("msg_waiting", "worker may still be running"));
    mock.push_turn(text_turn("msg_done", "reported last-moment failure"));
    let client: Arc<dyn ModelClient> = mock.clone();
    let notification = "<task-notification><status>failed</status><summary>unsupported model</summary></task-notification>";
    let poller = Arc::new(EagerPoller::new(vec![
        Vec::new(),
        vec![ApiMessage::user_text(notification)],
    ]));
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("wait for worker")])
        .with_max_iterations(1)
        .with_attachment_poller(
            poller.clone() as Arc<dyn crate::query::AttachmentPoller>,
            "session-poller",
            "turn-poller",
        );
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );

    let events = drain(&mut rx).await;
    let requests = mock.captured_requests();

    assert_eq!(requests.len(), 2);
    assert!(text_message_texts(&requests[1].messages)
        .iter()
        .any(|text| text.contains("unsupported model")));
    assert!(matches!(
        events.last(),
        Some(QueryEvent::Done { final_message, .. })
            if final_message.text() == "reported last-moment failure"
    ));
    assert_eq!(poller.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn run_query_does_not_poll_round_attachments_after_last_allowed_iteration() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let engine = build_engine_with(tool);
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_1",
        "{\"path\":\"a.rs\"}",
    ));
    let client: Arc<dyn ModelClient> = Arc::new(client);
    let poller = Arc::new(ScriptedPoller::with_messages(vec![vec![
        ApiMessage::user_text("must remain pending"),
    ]]));
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine))
        .with_max_iterations(1)
        .with_attachment_poller(
            poller.clone() as Arc<dyn crate::query::AttachmentPoller>,
            "session-poller",
            "turn-poller",
        );
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );

    let events = drain(&mut rx).await;

    assert!(poller.calls.lock().unwrap().is_empty());
    assert!(!events
        .iter()
        .any(|event| matches!(event, QueryEvent::AttachmentInjected { .. })));
}

#[tokio::test]
async fn run_query_adds_attachment_report_paths_before_next_tool_round() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool_1",
        "Read",
        "toolu_1",
        "{\"path\":\"a.rs\"}",
    ));
    client.push_turn(tool_turn(
        "msg_tool_2",
        "Read",
        "toolu_2",
        "{\"path\":\"worker.report.md\"}",
    ));
    client.push_turn(text_turn("msg_done", "all set"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let report_path = PathBuf::from("worker.report.md");
    let poller = Arc::new(ScriptedPoller::with_messages_and_report_paths(
        vec![
            vec![ApiMessage::user_text(
                "<task-notification><status>completed</status></task-notification>",
            )],
            Vec::new(),
        ],
        vec![vec![report_path.clone()], Vec::new()],
    ));
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("run worker")])
        .with_tools(tools_from_engine(&engine))
        .with_attachment_poller(
            poller as Arc<dyn crate::query::AttachmentPoller>,
            "session-poller",
            "turn-poller",
        );
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );

    let _events = drain(&mut rx).await;
    let paths_by_call = tool.coordinator_report_paths_by_call();

    assert_eq!(paths_by_call.len(), 2);
    assert!(paths_by_call[0].is_empty());
    assert_eq!(paths_by_call[1], vec![report_path]);
}

#[tokio::test]
async fn run_query_poller_not_called_when_first_iteration_ends_turn() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    // Single text turn: poller should never fire.
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_one", "done immediately"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let poller = Arc::new(ScriptedPoller::with_messages(vec![vec![
        ApiMessage::user_text("should never see this"),
    ]]));
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("hi")])
        .with_attachment_poller(
            poller.clone() as Arc<dyn crate::query::AttachmentPoller>,
            "session-poller",
            "turn-poller",
        );
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    // No AttachmentInjected event.
    assert!(!events
        .iter()
        .any(|e| matches!(e, QueryEvent::AttachmentInjected { .. })));
    assert_eq!(poller.calls.lock().unwrap().len(), 0);
}
