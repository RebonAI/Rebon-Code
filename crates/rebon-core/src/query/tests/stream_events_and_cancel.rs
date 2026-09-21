use super::*;

#[tokio::test]
async fn run_query_cancellation_short_circuits() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let client = MockModelClient::new();
    // Load plenty of work; cancellation should stop it early.
    for _ in 0..5 {
        client.push_turn(tool_turn(
            "msg_loop",
            "Read",
            "toolu_1",
            "{\"path\":\"a.rs\"}",
        ));
    }
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let cancel = CancelToken::new();
    cancel.cancel(); // Cancel before starting.

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("spin forever")]);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        cancel,
    );
    let events = drain(&mut rx).await;
    let cancelled = events.iter().any(|e| matches!(e, QueryEvent::Cancelled));
    assert!(cancelled, "expected Cancelled event");
}

#[tokio::test]
async fn run_query_cancellation_interrupts_an_in_flight_tool() {
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let engine = build_engine_with(Arc::new(HangingTool {
        started: started.clone(),
        dropped: dropped.clone(),
    }));

    let client = MockModelClient::new();
    client.push_turn(tool_turn("msg_tool", "Hanging", "toolu_hang", "{}"));
    let client: Arc<dyn ModelClient> = Arc::new(client);
    let cancel = CancelToken::new();
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("hang")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        cancel.clone(),
    );

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tool should start");

    cancel.cancel();
    let mut interrupted_result = false;
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match rx.recv().await {
                Some(QueryEvent::ToolDispatchResult {
                    tool_use_id,
                    outcome: Err(error),
                    ..
                }) if tool_use_id == "toolu_hang" && error == "Interrupted by user" => {
                    interrupted_result = true;
                }
                Some(QueryEvent::Cancelled) => break,
                Some(_) => {}
                None => panic!("query ended without a Cancelled event"),
            }
        }
    })
    .await
    .expect("cancel should interrupt the in-flight tool");

    assert!(
        interrupted_result,
        "the cancelled tool needs a terminal result"
    );
    assert!(
        dropped.load(Ordering::Acquire),
        "cancelling the turn must drop the pending tool future"
    );
}

#[tokio::test]
async fn run_query_cancellation_interrupts_retry_middleware_backoff() {
    let engine = Arc::new(Engine::new());
    let mock = Arc::new(MockModelClient::new());
    mock.push_error(ModelError::transient("retry me"));
    mock.push_turn(text_turn("msg_after_retry", "done"));
    let inner: Arc<dyn ModelClient> = mock.clone();
    let notifier = RetryNotifier::new();
    let config = RetryConfig {
        max_retries: 3,
        initial_backoff: std::time::Duration::from_secs(60),
        backoff_multiplier: 1.0,
        max_backoff: std::time::Duration::from_secs(60),
        max_consecutive_overloaded: 3,
        max_retry_after: rebon_api::MAX_HONOURED_RETRY_AFTER,
    };
    let client: Arc<dyn ModelClient> = Arc::new(RetryMiddleware::wrap_with_notifier(
        inner,
        config,
        notifier.clone(),
    ));
    let cancel = CancelToken::new();
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("start")]);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        cancel.clone(),
    );

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if notifier.current().is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("RetryMiddleware notifier should enter backoff");
    assert_eq!(mock.call_count(), 1);
    assert!(notifier.current().is_some());

    cancel.cancel();
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("cancel should interrupt RetryMiddleware backoff")
        .expect("cancelled event");
    assert!(matches!(event, QueryEvent::Cancelled));
    assert_eq!(mock.call_count(), 1);
    assert!(notifier.current().is_none());
}

#[tokio::test]
async fn forward_stream_event_announces_client_tool_use_as_tool_call() {
    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let mut state = StreamForwardState::default();
    let start = StreamEvent::ContentBlockStart {
        index: 0,
        content_block: ContentBlockStart::ToolUse {
            id: "toolu_skill".into(),
            name: "Skill".into(),
        },
    };
    let input = StreamEvent::ContentBlockDelta {
        index: 0,
        delta: ContentBlockDelta::InputJsonDelta {
            partial_json: "{\"skill\":\"imagegen\"}".into(),
        },
    };
    let stop = StreamEvent::ContentBlockStop { index: 0 };

    forward_stream_event(
        &mut state,
        &Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
        "sess-skill",
        &start,
    )
    .await;
    forward_stream_event(
        &mut state,
        &Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
        "sess-skill",
        &input,
    )
    .await;
    forward_stream_event(
        &mut state,
        &Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
        "sess-skill",
        &stop,
    )
    .await;

    let updates = publisher.snapshot();
    assert!(updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCall {
                tool_call_id,
                title,
                status: ToolCallStatus::Pending,
                ..
            } if tool_call_id == "toolu_skill" && title == "Skill"
        )
    }));
    let progress_updates = updates
        .iter()
        .filter_map(|u| match &u.update {
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status: Some(ToolCallStatus::InProgress),
                title,
                content,
                ..
            } if tool_call_id == "toolu_skill" => Some((title.as_deref(), content)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(progress_updates.len(), 3);
    assert!(
        progress_updates
            .iter()
            .all(|(_, content)| content.is_none()),
        "pre-output tool updates must not create placeholder body lines"
    );
    assert!(progress_updates
        .iter()
        .any(|(title, _)| *title == Some("/imagegen")));
}

#[tokio::test]
async fn forward_stream_event_announces_server_tool_use_as_tool_call() {
    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let mut state = StreamForwardState::default();
    let event = StreamEvent::ContentBlockStart {
        index: 0,
        content_block: ContentBlockStart::ServerToolUse {
            id: "srv_web".into(),
            name: "web_search".into(),
            input: json!({"query": "rust tui"}),
        },
    };

    forward_stream_event(
        &mut state,
        &Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
        "sess-server-tool",
        &event,
    )
    .await;

    let updates = publisher.snapshot();
    assert!(updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCall {
                tool_call_id,
                title,
                status: ToolCallStatus::Pending,
                ..
            } if tool_call_id == "srv_web" && title == "web_search \"rust tui\""
        )
    }));
    assert!(updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status: Some(ToolCallStatus::InProgress),
                ..
            } if tool_call_id == "srv_web"
        )
    }));
}

#[tokio::test]
async fn forward_stream_event_announces_image_generation_as_tool_call() {
    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let mut state = StreamForwardState::default();
    let start = StreamEvent::ContentBlockStart {
        index: 0,
        content_block: ContentBlockStart::ImageGeneration {
            id: "ig_test".into(),
            status: Some("in_progress".into()),
        },
    };

    forward_stream_event(
        &mut state,
        &Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
        "sess-image",
        &start,
    )
    .await;

    let updates = publisher.snapshot();
    assert!(updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCall {
                tool_call_id,
                title,
                status: ToolCallStatus::Pending,
                ..
            } if tool_call_id == "ig_test" && title == "ImageGeneration"
        )
    }));
    assert!(updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status: Some(ToolCallStatus::InProgress),
                ..
            } if tool_call_id == "ig_test"
        )
    }));
}

#[tokio::test]
async fn publish_generated_image_completion_reports_saved_path() {
    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let mut state = StreamForwardState::default();
    state.image_generation_blocks.insert(0, "ig_done".into());
    let message = AssistantMessage {
        id: "msg-image".into(),
        model: "mock".into(),
        content: vec![ApiContentBlock::GeneratedImage(
            rebon_api::GeneratedImageBlock {
                id: "ig_done".into(),
                status: Some("completed".into()),
                revised_prompt: Some("draw a fox".into()),
                media_type: "image/png".into(),
                data: String::new(),
                saved_path: Some("/tmp/ig_done.png".into()),
            },
        )],
        stop_reason: Some(StopReason::EndTurn),
        usage: Usage::default(),
    };

    publish_generated_image_completion(
        &state,
        &Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
        "sess-image",
        &message,
    )
    .await;

    let updates = publisher.snapshot();
    let completion = updates.iter().find_map(|u| match &u.update {
        SessionUpdate::ToolCallUpdate {
            tool_call_id,
            status: Some(ToolCallStatus::Completed),
            content,
            locations,
            raw_output,
            ..
        } if tool_call_id == "ig_done" => Some((content, locations, raw_output)),
        _ => None,
    });
    let (content, locations, raw_output) = completion.expect("expected image completion update");
    assert!(content.is_none());
    assert_eq!(
        locations
            .as_ref()
            .and_then(|items| items.first())
            .map(|loc| loc.path.as_str()),
        Some("/tmp/ig_done.png")
    );
    assert!(raw_output
        .as_ref()
        .is_some_and(|output| !output.contains_key("saved_path")));
}

#[tokio::test]
async fn orphaned_tool_card_from_abandoned_stream_attempt_fails_at_iteration_end() {
    // A mid-stream WS retry re-drives the turn into the same event stream:
    // the abandoned attempt's tool card was already announced (Pending +
    // InProgress), but its block never reaches the finished message, so
    // dispatch never runs and no terminal update ever arrives.
    // The reconcile pass must fail exactly the orphaned ids — cards whose
    // blocks survive into the finished message stay untouched.
    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let mut state = StreamForwardState::default();
    let announce = |id: &str, name: &str| StreamEvent::ContentBlockStart {
        index: 1,
        content_block: ContentBlockStart::ToolUse {
            id: id.into(),
            name: name.into(),
        },
    };
    // Attempt 1 starts a tool call, then the stream breaks; the retried
    // attempt re-streams the same block INDEX with a fresh id.
    forward_stream_event(
        &mut state,
        &Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
        "sess-orphan",
        &announce("toolu_orphan", "PowerShell"),
    )
    .await;
    forward_stream_event(
        &mut state,
        &Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
        "sess-orphan",
        &announce("toolu_live", "PowerShell"),
    )
    .await;
    // A server-side tool card from the abandoned attempt is orphaned the
    // same way (its Completed arrives only via later stream events).
    forward_stream_event(
        &mut state,
        &Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
        "sess-orphan",
        &StreamEvent::ContentBlockStart {
            index: 2,
            content_block: ContentBlockStart::ServerToolUse {
                id: "srv_orphan".into(),
                name: "web_search".into(),
                input: json!({"query": "abandoned"}),
            },
        },
    )
    .await;

    let message = AssistantMessage {
        id: "msg-retried".into(),
        model: "mock".into(),
        content: vec![ApiContentBlock::ToolUse(ToolUseBlock {
            id: "toolu_live".into(),
            name: "PowerShell".into(),
            input: json!({"command": "Get-ChildItem"}),
        })],
        stop_reason: Some(StopReason::ToolUse),
        usage: Usage::default(),
    };

    publish_orphaned_tool_card_failures(
        &mut state,
        &Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
        "sess-orphan",
        &message,
    )
    .await;

    let updates = publisher.snapshot();
    let failed_ids: Vec<&str> = updates
        .iter()
        .filter_map(|u| match &u.update {
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status: Some(ToolCallStatus::Failed),
                content,
                ..
            } => {
                assert!(
                    content.is_some(),
                    "orphan failure must carry an explanatory content body"
                );
                Some(tool_call_id.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(failed_ids.contains(&"toolu_orphan"));
    assert!(failed_ids.contains(&"srv_orphan"));
    assert!(
        !failed_ids.contains(&"toolu_live"),
        "the retried attempt's surviving tool call is owned by dispatch and must not be failed"
    );
    assert!(
        state.announced_tool_card_ids.is_empty(),
        "reconcile drains the announce map so the next iteration starts clean"
    );

    // Second iteration with no announcements publishes nothing new.
    let update_count = updates.len();
    publish_orphaned_tool_card_failures(
        &mut state,
        &Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
        "sess-orphan",
        &message,
    )
    .await;
    assert_eq!(publisher.snapshot().len(), update_count);
}

#[test]
fn workflow_tools_stay_deferred_without_workflow_policy() {
    let mut engine = Engine::new();
    engine.register_tool(Arc::new(rebon_plugin_workflow::WorkflowTool::new()));
    engine.register_tool(Arc::new(RecordingTool::new(
        rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME,
        Value::Null,
    )));

    assert!(eager_tools_from_engine(&engine).is_empty());
    assert_eq!(
        engine.deferred_tool_names(),
        vec![
            rebon_tool::WORKFLOW_TOOL_NAME.to_string(),
            rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME.to_string(),
        ]
    );
}

#[test]
fn workflow_controller_policy_exposes_workflow_and_scouting_tools() {
    let mut engine = Engine::new();
    engine.register_tool(Arc::new(rebon_plugin_workflow::WorkflowTool::new()));
    engine.register_tool(Arc::new(rebon_tool::ReadTool));
    engine.register_tool(Arc::new(rebon_tool::WriteTool));
    engine.register_tool(Arc::new(rebon_tool::BashTool::default()));
    engine.register_tool(Arc::new(RecordingTool::new(
        rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME,
        Value::Null,
    )));

    let policy = ExecutionPolicy::workflow_controller();
    let effective_filter = combine_filters(None, Some(&policy));
    let projection = runtime_tool_projection(
        &engine,
        true,
        effective_filter.as_ref(),
        Some(&policy),
        &engine.deferred_tool_names(),
        &[],
        None,
    );
    let tool_names = projection.provider_visible_tool_names();

    assert!(tool_names.contains(&rebon_tool::WORKFLOW_TOOL_NAME.to_string()));
    assert!(tool_names.contains(&rebon_tool::RUN_WORKFLOW_ALIAS.to_string()));
    assert!(tool_names.contains(&"Read".to_string()));
    assert!(!tool_names.contains(&"Write".to_string()));
    assert!(!tool_names.contains(&"Bash".to_string()));
    assert!(!projection
        .deferred_tool_names
        .contains(&rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME.to_string()));
}

#[test]
fn tools_from_engine_reflects_registered_tools() {
    let tool: Arc<dyn Tool> = Arc::new(RecordingTool::new("Read", Value::Null));
    let engine = build_engine_with(tool);
    let tools = tools_from_engine(&engine);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "Read");
}

#[test]
fn eager_tools_from_engine_exposes_run_workflow_alias_when_promoted() {
    let mut engine = Engine::new();
    engine.register_tool(Arc::new(rebon_plugin_workflow::WorkflowTool::new()));

    let policy = ExecutionPolicy::default().with_eager_promotions([
        rebon_tool::WORKFLOW_TOOL_NAME,
        rebon_tool::RUN_WORKFLOW_ALIAS,
    ]);
    let tool_names: Vec<_> = eager_tools_from_engine_for_policy(&engine, Some(&policy))
        .into_iter()
        .map(|tool| tool.name)
        .collect();

    assert!(tool_names.contains(&rebon_tool::WORKFLOW_TOOL_NAME.to_string()));
    assert!(tool_names.contains(&rebon_tool::RUN_WORKFLOW_ALIAS.to_string()));
}
