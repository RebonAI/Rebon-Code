use super::*;

#[tokio::test]
async fn engine_query_executor_publishes_generated_session_title() {
    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let main_client = MockModelClient::new();
    main_client.push_turn(text_turn("main_reply", "done"));
    let title_client = MockModelClient::new();
    title_client.push_turn(text_turn(
        "title_reply",
        r#"{"title":"Fix terminal title"}"#,
    ));
    let title_client_for_assert = title_client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(ForkingModelClient {
        main: main_client,
        title: title_client,
    });

    let projects_root_dir = temp_projects_root("session_title_update");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());
    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state.clone())
        .with_title_model("small-title-model");

    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(TextContent {
            text: "make terminal title update while loading".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
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

    let title = wait_for_session_title_update(&publisher).await;
    assert_eq!(title.as_deref(), Some("Fix terminal title"));
    assert_eq!(
        state.get_session(&session.id).unwrap().title.as_deref(),
        Some("Fix terminal title")
    );
    assert_eq!(
        rebon_session::load_session_title(projects_root, &cwd, &session.id).as_deref(),
        Some("Fix terminal title")
    );
    let captured = title_client_for_assert.captured_requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].model, "small-title-model");
}

#[tokio::test]
async fn engine_query_executor_generates_session_title_without_client_fork() {
    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(text_turn(
        "title_reply",
        r#"{"title":"Summarize session request"}"#,
    ));
    client.push_turn(text_turn("main_reply", "done"));
    let client_for_assert = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("session_title_no_fork");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());
    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "main-model")
        .with_server_state(state.clone())
        .with_title_model("small-title-model");

    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(TextContent {
            text: "summarize the session request into a useful title".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
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

    let title = wait_for_session_title_update(&publisher).await;
    assert_eq!(title.as_deref(), Some("Summarize session request"));
    assert_eq!(
        state.get_session(&session.id).unwrap().title.as_deref(),
        Some("Summarize session request")
    );
    let captured = client_for_assert.captured_requests();
    assert_eq!(captured[0].model, "small-title-model");
    assert_eq!(captured[1].model, "main-model");
}

#[tokio::test]
async fn engine_query_executor_runs_full_turn_with_mock_model() {
    // Engine + one recording tool.
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "file body"})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    // Mock model: turn 1 calls Read, turn 2 replies with the result.
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_1",
        "{\"path\":\"a.rs\"}",
    ));
    client.push_turn(text_turn("msg_done", "saw file body"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    // Build the executor + invoke it via the PromptExecutor trait.
    let projects_root_dir = temp_projects_root("full_turn");
    let projects_root = projects_root_dir.path();
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_max_iterations(5);

    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: "sess-e2e".into(),
        cwd: "/tmp/repo".into(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: "please read a.rs".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
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

    let outcome = executor.execute(request).await.unwrap();
    assert_eq!(outcome.stop_reason, AcpStopReason::EndTurn);

    // Tool was invoked once.
    assert_eq!(tool.call_count(), 1);

    // Publisher saw at least one agent_message_chunk update.
    let updates = publisher.snapshot();
    let saw_chunk = updates
        .iter()
        .any(|u| matches!(&u.update, SessionUpdate::AgentMessageChunk { .. }));
    assert!(
        saw_chunk,
        "publisher should have observed an agent_message_chunk"
    );

    // Transcript file exists and contains a user + assistant entries.
    let transcript_path =
        rebon_session::transcript_file_path(projects_root, "/tmp/repo", "sess-e2e");
    assert!(
        transcript_path.exists(),
        "transcript file should be written"
    );
    let raw = std::fs::read_to_string(&transcript_path).unwrap();
    // The file should contain at least one "user" and one "assistant" entry.
    assert!(raw.contains("\"type\":\"user\""));
    assert!(raw.contains("\"type\":\"assistant\""));
}

#[tokio::test]
async fn engine_query_executor_publishes_tool_dispatch_updates() {
    // Verify the executor forwards ToolDispatchStart / ToolDispatchResult
    // as ACP ToolCall + ToolCallUpdate updates through the publisher.
    let tool = Arc::new(RecordingTool::new(
        "Read",
        json!({"contents": "file body", "file_path": "a.rs"}),
    ));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_dispatch",
        "{\"file_path\":\"a.rs\"}",
    ));
    client.push_turn(text_turn("msg_done", "done"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("tool_dispatch_updates");
    let projects_root = projects_root_dir.path();
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_max_iterations(5);

    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: "sess-dispatch".into(),
        cwd: "/tmp/repo".into(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: "read a.rs".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
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

    let outcome = executor.execute(request).await.unwrap();
    assert_eq!(outcome.stop_reason, AcpStopReason::EndTurn);
    assert_eq!(tool.call_count(), 1);

    let updates = publisher.snapshot();

    // Should see a ToolCall update (initial Pending status).
    let saw_tool_call = updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCall {
                tool_call_id,
                title,
                status: ToolCallStatus::Pending,
                ..
            } if tool_call_id == "toolu_dispatch" && title.contains("Read")
        )
    });
    assert!(
        saw_tool_call,
        "publisher should have observed a ToolCall(Pending) update"
    );

    // Should see a ToolCallUpdate with InProgress status.
    let saw_in_progress = updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status: Some(ToolCallStatus::InProgress),
                ..
            } if tool_call_id == "toolu_dispatch"
        )
    });
    assert!(
        saw_in_progress,
        "publisher should have observed a ToolCallUpdate(InProgress) update"
    );

    // Should see a ToolCallUpdate with Completed status.
    let saw_completed = updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status: Some(ToolCallStatus::Completed),
                ..
            } if tool_call_id == "toolu_dispatch"
        )
    });
    assert!(
        saw_completed,
        "publisher should have observed a ToolCallUpdate(Completed) update"
    );

    // Completed update should carry file locations extracted from
    // the tool result.
    let completed_locations = updates.iter().find_map(|u| match &u.update {
        SessionUpdate::ToolCallUpdate {
            tool_call_id,
            status: Some(ToolCallStatus::Completed),
            locations,
            ..
        } if tool_call_id == "toolu_dispatch" => locations.clone(),
        _ => None,
    });
    assert!(
        completed_locations.is_some(),
        "completed update should carry file locations"
    );
    let locs = completed_locations.unwrap();
    assert_eq!(locs.len(), 1);
    assert_eq!(locs[0].path, "a.rs");
}

#[tokio::test]
async fn engine_query_executor_publishes_tool_failure_status() {
    // When a tool fails, the publisher should see a Failed status update.
    let tool = Arc::new(RecordingTool::failing("Read"));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_bad",
        "Read",
        "toolu_fail",
        "{\"path\":\"a.rs\"}",
    ));
    client.push_turn(text_turn("msg_recover", "tool failed"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("tool_failure_status");
    let projects_root = projects_root_dir.path();
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_max_iterations(5);

    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: "sess-fail".into(),
        cwd: "/tmp/repo".into(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: "read a.rs".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
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

    let outcome = executor.execute(request).await.unwrap();
    assert_eq!(outcome.stop_reason, AcpStopReason::EndTurn);

    let updates = publisher.snapshot();

    // Should see a ToolCallUpdate with Failed status.
    let saw_failed = updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status: Some(ToolCallStatus::Failed),
                ..
            } if tool_call_id == "toolu_fail"
        )
    });
    assert!(
        saw_failed,
        "publisher should have observed a ToolCallUpdate(Failed) update"
    );

    // Failed update should carry error text in content.
    let fail_content = updates.iter().find_map(|u| match &u.update {
        SessionUpdate::ToolCallUpdate {
            tool_call_id,
            status: Some(ToolCallStatus::Failed),
            content,
            ..
        } if tool_call_id == "toolu_fail" => content.clone(),
        _ => None,
    });
    assert!(
        fail_content.is_some(),
        "failed update should carry error content"
    );
    let blocks = fail_content.unwrap();
    assert!(!blocks.is_empty());
    // Verify the error text mentions "execution failed".
    let text = match &blocks[0] {
        ToolCallContent::Content(c) => match &c.content {
            AcpContentBlock::Text(t) => t.text.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    assert!(
        text.contains("execution failed"),
        "error content should mention 'execution failed', got: {text}"
    );
}

#[tokio::test]
async fn engine_query_executor_publishes_multiple_tool_calls_in_sequence() {
    // Two successive tool-call iterations should each publish
    // their own ToolCall + InProgress + Completed lifecycle.
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "ok"})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    let client = MockModelClient::new();
    // Turn 1: tool call.
    client.push_turn(tool_turn(
        "msg_t1",
        "Read",
        "toolu_first",
        "{\"file_path\":\"a.rs\"}",
    ));
    // Turn 2: another tool call.
    client.push_turn(tool_turn(
        "msg_t2",
        "Read",
        "toolu_second",
        "{\"file_path\":\"b.rs\"}",
    ));
    // Turn 3: final text.
    client.push_turn(text_turn("msg_done", "both read"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("multi_tool");
    let projects_root = projects_root_dir.path();
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_max_iterations(5);

    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: "sess-multi".into(),
        cwd: "/tmp/repo".into(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: "read both".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
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

    let outcome = executor.execute(request).await.unwrap();
    assert_eq!(outcome.stop_reason, AcpStopReason::EndTurn);
    assert_eq!(tool.call_count(), 2);

    let updates = publisher.snapshot();

    // Both tool calls should have ToolCall(Pending) entries.
    let first_pending = updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCall {
                tool_call_id,
                status: ToolCallStatus::Pending,
                ..
            } if tool_call_id == "toolu_first"
        )
    });
    let second_pending = updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCall {
                tool_call_id,
                status: ToolCallStatus::Pending,
                ..
            } if tool_call_id == "toolu_second"
        )
    });
    assert!(first_pending, "first tool call should be published");
    assert!(second_pending, "second tool call should be published");

    // Both should reach Completed.
    let first_completed = updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status: Some(ToolCallStatus::Completed),
                ..
            } if tool_call_id == "toolu_first"
        )
    });
    let second_completed = updates.iter().any(|u| {
        matches!(
            &u.update,
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status: Some(ToolCallStatus::Completed),
                ..
            } if tool_call_id == "toolu_second"
        )
    });
    assert!(first_completed, "first tool call should complete");
    assert!(second_completed, "second tool call should complete");
}

#[tokio::test]
async fn same_response_same_file_writes_do_not_both_succeed() {
    let target_dir = tempfile::tempdir().unwrap();
    let file = target_dir.path().join("shared.txt");
    std::fs::write(&file, "original").unwrap();

    let engine = build_engine_with_tools(vec![
        Arc::new(rebon_tool::ReadTool),
        Arc::new(rebon_tool::WriteTool),
    ]);
    let client = MockModelClient::new();
    let read_input = serde_json::to_string(&json!({ "file_path": file })).unwrap();
    let first_write = serde_json::to_string(&json!({
        "file_path": file,
        "content": "first"
    }))
    .unwrap();
    let second_write = serde_json::to_string(&json!({
        "file_path": file,
        "content": "second"
    }))
    .unwrap();
    client.push_turn(tool_turn("msg_read", "Read", "toolu_read", &read_input));
    client.push_turn(two_tool_turn(
        "msg_writes",
        ("Write", "toolu_first_write", &first_write),
        ("Write", "toolu_second_write", &second_write),
    ));
    client.push_turn(text_turn("msg_done", "write conflict handled"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("same_response_file_writes");
    let executor = EngineQueryExecutor::new(engine, client, projects_root_dir.path(), "mock-model")
        .with_max_iterations(5);
    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: "sess-same-response-writes".into(),
        cwd: target_dir.path().to_string_lossy().into_owned(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: "read, then try two writes in one response".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: Some(publisher.clone() as Arc<dyn SessionUpdatePublisher>),
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

    let outcome = executor.execute(request).await.unwrap();
    assert_eq!(outcome.stop_reason, AcpStopReason::EndTurn);
    let final_content = std::fs::read_to_string(&file).unwrap();
    assert!(final_content == "first" || final_content == "second");

    let updates = publisher.snapshot();
    let terminal_status = |tool_call_id: &str| {
        updates.iter().find_map(|update| match &update.update {
            SessionUpdate::ToolCallUpdate {
                tool_call_id: candidate,
                status: Some(status),
                ..
            } if candidate == tool_call_id
                && matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed) =>
            {
                Some(*status)
            }
            _ => None,
        })
    };
    let statuses = [
        terminal_status("toolu_first_write").expect("first write terminal status"),
        terminal_status("toolu_second_write").expect("second write terminal status"),
    ];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == ToolCallStatus::Completed)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == ToolCallStatus::Failed)
            .count(),
        1
    );
}

#[tokio::test]
async fn engine_query_executor_propagates_cancel_through_acp_prompt_cancel() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

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

    let projects_root_dir = temp_projects_root("cancel");
    let executor = EngineQueryExecutor::new(engine, client, projects_root_dir.path(), "mock-model");

    let cancel = CancelToken::new();
    cancel.cancel(); // Pre-cancel so the first iteration observes it.
    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: "sess-cancel".into(),
        cwd: "/tmp/repo".into(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: "spin forever".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: None,
        permission_publisher: None,
        cancel,
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
    let err = executor.execute(request).await.unwrap_err();
    assert!(matches!(err, PromptExecutorError::Cancelled));
}

#[tokio::test]
async fn engine_query_executor_cancels_while_a_tool_is_running() {
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let engine = build_engine_with(Arc::new(HangingTool {
        started: started.clone(),
        dropped: dropped.clone(),
    }));
    let client = Arc::new(MockModelClient::new());
    client.push_turn(tool_turn("msg_tool", "Hanging", "toolu_hang", "{}"));
    let model_client: Arc<dyn ModelClient> = client.clone();
    let projects_root_dir = temp_projects_root("cancel_running_tool");
    let executor = Arc::new(EngineQueryExecutor::new(
        engine,
        model_client,
        projects_root_dir.path(),
        "mock-model",
    ));
    let cancel = CancelToken::new();
    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: "sess-cancel-running-tool".into(),
        cwd: "/tmp/repo".into(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: "hang".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: None,
        permission_publisher: None,
        cancel: cancel.clone(),
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
    let running_executor = executor.clone();
    let execution = tokio::spawn(async move { running_executor.execute(request).await });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tool should start");

    cancel.cancel();
    let err = tokio::time::timeout(std::time::Duration::from_secs(1), execution)
        .await
        .expect("executor should stop after cancellation")
        .expect("executor task should not panic")
        .unwrap_err();
    assert!(matches!(err, PromptExecutorError::Cancelled));
    assert!(
        dropped.load(Ordering::Acquire),
        "executor cancellation must drop the pending tool future"
    );

    client.push_turn(text_turn("msg_after_cancel", "continued"));
    let next = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: "sess-cancel-running-tool".into(),
        cwd: "/tmp/repo".into(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: "continue".into(),
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
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), executor.execute(next))
        .await
        .expect("the session should accept a turn after cancellation")
        .expect("the next turn should succeed");
    assert_eq!(outcome.stop_reason, AcpStopReason::EndTurn);
}

#[tokio::test]
async fn engine_query_executor_invalidates_previous_response_id_on_cancel() {
    // Regression: after Esc-cancel, the server-side response-id
    // chain must be severed. Otherwise the next turn sends
    // `previous_response_id` pointing at a response that holds an
    // orphan function_call (no function_call_output), and the
    // server rejects with 400 "No tool output found for function
    // call". Users then had to retype the prompt a second time
    // because the 400 itself clears last_response_id, so retry
    // finally succeeds via full replay.
    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    // Keep a typed handle so we can inspect the invalidate counter.
    let mock = Arc::new(MockModelClient::new());
    mock.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_1",
        "{\"path\":\"a.rs\"}",
    ));
    let client_arc: Arc<dyn ModelClient> = mock.clone();

    let projects_root_dir = temp_projects_root("cancel_invalidate");
    let executor =
        EngineQueryExecutor::new(engine, client_arc, projects_root_dir.path(), "mock-model");

    let cancel = CancelToken::new();
    cancel.cancel(); // Pre-cancel so the driver observes it on first iteration.
    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: "sess-cancel-invalidate".into(),
        cwd: "/tmp/repo".into(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: "spin forever".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: None,
        permission_publisher: None,
        cancel,
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
    let err = executor.execute(request).await.unwrap_err();
    assert!(matches!(err, PromptExecutorError::Cancelled));

    assert!(
        mock.invalidate_previous_response_id_count() >= 1,
        "cancel handler must invalidate previous_response_id so the next \
             turn does a full replay with synthesized tool_results"
    );
}

#[tokio::test]
async fn engine_query_executor_respects_tool_filter() {
    use rebon_tool::ToolFilter;

    // Register two tools; the filter should hide one.
    struct ApproveBroker;
    #[async_trait]
    impl crate::PermissionBroker for ApproveBroker {
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
    let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveBroker));
    engine.register_tool(Arc::new(RecordingTool::new("Read", json!({}))));
    engine.register_tool(Arc::new(RecordingTool::new("Write", json!({}))));
    let engine = Arc::new(engine);

    // MockModelClient — keep a typed clone so we can inspect
    // the recorded request after the run.
    let mock = Arc::new(MockModelClient::new());
    mock.push_turn(text_turn("msg_1", "hi"));
    let client_arc: Arc<dyn ModelClient> = mock.clone();

    let projects_root_dir = temp_projects_root("tool_filter");
    let projects_root = projects_root_dir.path();
    let executor = EngineQueryExecutor::new(engine, client_arc, projects_root, "mock-model")
        .with_tool_filter(ToolFilter::allow_only(["Read"]));

    let outcome = executor
        .execute(PromptRequest {
            user_prompt: None,
            effort_is_session_default: false,
            session_id: "sess-filter".into(),
            cwd: "/tmp".into(),
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
        })
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, AcpStopReason::EndTurn);

    // The captured request should carry ONLY the `Read` tool.
    let captured = mock.captured_requests();
    assert_eq!(captured.len(), 1);
    let tool_names: Vec<_> = captured[0].tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(tool_names, vec!["Read".to_string()]);
    assert!(!tool_names.contains(&"Write".to_string()));
}

#[tokio::test]
async fn engine_query_executor_round_trips_tool_use_through_transcript() {
    // Full end-to-end: run a session that calls a tool,
    // persist the whole turn to disk, reload the session,
    // and verify the replayed history carries the tool_use
    // block + tool_result block (not just the text).
    let tool = Arc::new(RecordingTool::new(
        "Read",
        json!({"contents": "real file body"}),
    ));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    // Mock client: turn 1 = tool use, turn 2 = final text.
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_1",
        "{\"path\":\"a.rs\"}",
    ));
    client.push_turn(text_turn("msg_final", "here's what I found"));
    let client_arc: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("round_trip");
    let projects_root = projects_root_dir.path();
    let session_id = "sess-round-trip".to_string();
    let cwd = "/tmp/rt".to_string();

    // First run: writes the transcript.
    let executor = EngineQueryExecutor::new(
        engine.clone(),
        client_arc.clone(),
        projects_root,
        "mock-model",
    );
    let request = PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session_id.clone(),
        cwd: cwd.clone(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: "read a.rs".into(),
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
    };
    let outcome = executor.execute(request).await.unwrap();
    assert_eq!(outcome.stop_reason, AcpStopReason::EndTurn);

    // Transcript file should exist and contain the full round
    // trip: user / assistant(with tool_use) / user(with
    // tool_result) / assistant(final).
    let transcript_path = rebon_session::transcript_file_path(projects_root, &cwd, &session_id);
    assert!(transcript_path.exists(), "transcript should be persisted");
    let raw = std::fs::read_to_string(&transcript_path).unwrap();
    assert!(raw.contains("\"type\":\"tool_use\""));
    assert!(raw.contains("\"toolu_1\""));
    assert!(raw.contains("\"type\":\"tool_result\""));
    assert!(raw.contains("\"tool_use_id\":\"toolu_1\""));

    // Now load the session into a fresh ServerState and
    // replay via transcript_to_api_messages.
    let state = Arc::new(rebon_session_state::ServerState::new());
    state.mark_initialized().unwrap();
    state
        .load_session(projects_root, &session_id, &cwd, None, Vec::new())
        .expect("load_session failed");
    let record = state.get_session(&session_id).unwrap();
    let messages = transcript_to_api_messages(&record.loaded_transcript);

    // Expected replayed history:
    //   [0] user:      "read a.rs"
    //   [1] assistant: [tool_use Read{path:"a.rs"}]
    //   [2] user:      [tool_result toolu_1]
    //   [3] assistant: "here's what I found"
    assert_eq!(messages.len(), 4, "expected 4 replayed messages");
    assert_eq!(messages[0].role, rebon_api::Role::User);
    assert_eq!(messages[0].content[0].as_text(), Some("read a.rs"));

    assert_eq!(messages[1].role, rebon_api::Role::Assistant);
    let found_tool_use = messages[1]
            .content
            .iter()
            .any(|b| matches!(b, rebon_api::ContentBlock::ToolUse(tu) if tu.id == "toolu_1" && tu.name == "Read"));
    assert!(
        found_tool_use,
        "assistant iteration 0 should replay the Read tool_use block"
    );

    assert_eq!(messages[2].role, rebon_api::Role::User);
    let found_tool_result = messages[2].content.iter().any(|b| {
        matches!(
            b,
            rebon_api::ContentBlock::ToolResult(tr) if tr.tool_use_id == "toolu_1"
        )
    });
    assert!(
        found_tool_result,
        "user iteration 1 should replay the tool_result batch"
    );

    assert_eq!(messages[3].role, rebon_api::Role::Assistant);
    let final_text: String = messages[3]
        .content
        .iter()
        .filter_map(|b| b.as_text().map(str::to_string))
        .collect();
    assert_eq!(final_text, "here's what I found");
}

#[tokio::test]
async fn streamed_generated_image_is_kept_in_persisted_and_reloaded_executor_history() {
    let engine = build_engine_with_tools(Vec::new());
    let client = MockModelClient::new();
    client.push_turn(generated_image_turn(
        "msg_image",
        &[("ig_history", "aGlzdG9yeQ==", "image/png")],
        StopReason::EndTurn,
    ));
    let projects_root_dir = temp_projects_root("generated_image_history");
    let projects_root = projects_root_dir.path();
    let session_id = "sess-image-history";
    let cwd = projects_root.to_string_lossy().to_string();
    let executor = EngineQueryExecutor::new(engine, Arc::new(client), projects_root, "mock-model");

    let outcome = executor
        .execute(basic_prompt_request(session_id, &cwd))
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, AcpStopReason::EndTurn);

    let state = Arc::new(rebon_session_state::ServerState::new());
    state.mark_initialized().unwrap();
    state
        .load_session(projects_root, session_id, &cwd, None, Vec::new())
        .expect("load persisted image session");
    let record = state.get_session(session_id).unwrap();
    let image = record
        .loaded_transcript
        .iter()
        .filter(|entry| entry.entry_type == "assistant")
        .filter_map(|entry| entry.raw["message"]["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "generated_image")
        .expect("generated image in reloaded transcript");
    assert_eq!(image["id"], "ig_history");
    assert_eq!(image["data"], "aGlzdG9yeQ==");
    assert!(image.get("saved_path").is_none());
}
