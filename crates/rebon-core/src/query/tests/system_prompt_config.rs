use super::*;

#[tokio::test]
async fn execute_with_system_prompt_config_resolves_lazily() {
    let _guard = env_lock();
    let _stable_guard = EnvVarGuard::set("REBON_STABLE_BASE_SYSTEM", "0");
    // Verify that with_system_prompt_config produces a non-None
    // system prompt that reaches the model request.
    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "configured response"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("system_prompt_config");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());

    let config = crate::system_prompt::SystemPromptConfig {
        model: "mock-model".to_string(),
        model_marketing_name: Some("Mock Model".to_string()),
        knowledge_cutoff: Some("May 2025".to_string()),
        tool_names: vec!["Bash".to_string()],
        deferred_tool_names: Vec::new(),
        platform: "test".to_string(),
        shell: "bash".to_string(),
        os_version: "TestOS 1.0".to_string(),
        language: Some("Japanese".to_string()),
        normal_system_prompt_override: None,
        minimal_system_prompt_override: None,
        chat_system_prompt_override: None,
        auto_continue_background_agents: true,
    };

    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system_prompt_config(config)
        .with_server_state(state.clone());

    let cancel = CancelToken::new();
    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "test".into(),
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

    // Should succeed — the system prompt config should be
    // resolved into a non-empty system string for the model.
    let outcome = executor.execute(request).await;
    assert!(outcome.is_ok(), "execute failed: {:?}", outcome.err());
    let captured = client_handle.captured_requests();
    let system = captured[0]
        .system
        .as_deref()
        .expect("configured system prompt");
    assert!(system.contains("# Language\nEvery reply must be in Japanese."));
}

#[tokio::test]
async fn system_prompt_config_filters_deferred_tools_for_execution_policy() {
    let _guard = env_lock();
    let _stable_guard = EnvVarGuard::set("REBON_STABLE_BASE_SYSTEM", "1");

    let engine = build_engine_with_tools(vec![
        Arc::new(RecordingTool::deferred("Read", Value::Null)) as Arc<dyn Tool>,
        Arc::new(RecordingTool::deferred("TeamCreate", Value::Null)) as Arc<dyn Tool>,
        Arc::new(RecordingTool::deferred("TodoWrite", Value::Null)) as Arc<dyn Tool>,
        Arc::new(RecordingTool::deferred("CronCreate", Value::Null)) as Arc<dyn Tool>,
    ]);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "configured response"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("system_prompt_policy_filter");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());

    let config = crate::system_prompt::SystemPromptConfig {
        model: "mock-model".to_string(),
        model_marketing_name: Some("Mock Model".to_string()),
        knowledge_cutoff: Some("May 2025".to_string()),
        tool_names: vec!["AskUserQuestion".to_string()],
        deferred_tool_names: engine.deferred_tool_names(),
        platform: "test".to_string(),
        shell: "bash".to_string(),
        os_version: "TestOS 1.0".to_string(),
        language: None,
        normal_system_prompt_override: None,
        minimal_system_prompt_override: None,
        chat_system_prompt_override: None,
        auto_continue_background_agents: true,
    };

    let executor = EngineQueryExecutor::new(engine.clone(), client, projects_root, "mock-model")
        .with_system_prompt_config(config.clone())
        .with_server_state(state.clone());

    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "test".into(),
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
        execution_policy: Some(ExecutionPolicy::ultraplan(
            rebon_types::UltraplanContext::planning_turn(
                "run-1",
                "plan_mode_active",
                rebon_types::PolicyMode::Observe,
            ),
        )),
        replay_requests: Vec::new(),
        skill_invocations: Vec::new(),
    };

    let outcome = executor.execute(request).await;
    assert!(outcome.is_ok(), "execute failed: {:?}", outcome.err());

    let captured = client_handle.captured_requests();
    let request = captured
        .into_iter()
        .next()
        .expect("captured provider request");
    let prompt = request
        .system
        .as_deref()
        .expect("captured request system prompt");
    let runtime = captured_runtime_context_text(&request);
    assert!(!prompt.contains("deferred tools are now available via ToolSearch"));
    assert!(!prompt.contains("AskUserQuestion"));
    assert!(!prompt.contains("TeamCreate"));
    assert!(!prompt.contains("TodoWrite"));
    assert!(!prompt.contains("CronCreate"));
    assert!(!runtime.contains("AskUserQuestion"));
    assert!(!runtime.contains("TeamCreate"));
    assert!(!runtime.contains("TodoWrite"));
    assert!(!runtime.contains("CronCreate"));
    assert!(request.transient_context.is_none());

    assert!(config
        .deferred_tool_names
        .contains(&"TeamCreate".to_string()));
    assert!(config
        .deferred_tool_names
        .contains(&"TodoWrite".to_string()));
    assert!(config
        .deferred_tool_names
        .contains(&"CronCreate".to_string()));
}

#[tokio::test]
async fn system_prompt_config_preserves_deferred_tools_without_policy() {
    let _guard = env_lock();
    let _stable_guard = EnvVarGuard::set("REBON_STABLE_BASE_SYSTEM", "1");

    let engine = build_engine_with_tools(vec![
        Arc::new(RecordingTool::deferred("TeamCreate", Value::Null)) as Arc<dyn Tool>,
        Arc::new(RecordingTool::deferred("TodoWrite", Value::Null)) as Arc<dyn Tool>,
        Arc::new(RecordingTool::deferred("CronCreate", Value::Null)) as Arc<dyn Tool>,
    ]);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "configured response"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("system_prompt_no_policy");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());

    let config = crate::system_prompt::SystemPromptConfig {
        model: "mock-model".to_string(),
        model_marketing_name: Some("Mock Model".to_string()),
        knowledge_cutoff: Some("May 2025".to_string()),
        tool_names: vec!["Read".to_string()],
        deferred_tool_names: engine.deferred_tool_names(),
        platform: "test".to_string(),
        shell: "bash".to_string(),
        os_version: "TestOS 1.0".to_string(),
        language: None,
        normal_system_prompt_override: None,
        minimal_system_prompt_override: None,
        chat_system_prompt_override: None,
        auto_continue_background_agents: true,
    };

    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system_prompt_config(config)
        .with_server_state(state.clone());

    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "test".into(),
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

    let outcome = executor.execute(request).await;
    assert!(outcome.is_ok(), "execute failed: {:?}", outcome.err());

    let captured = client_handle.captured_requests();
    let request = captured
        .into_iter()
        .next()
        .expect("captured provider request");
    let prompt = request
        .system
        .as_deref()
        .expect("captured request system prompt");
    let runtime = captured_runtime_context_text(&request);
    assert!(!prompt.contains("TeamCreate"));
    assert!(!prompt.contains("TodoWrite"));
    assert!(!prompt.contains("CronCreate"));
    assert!(runtime.contains("TeamCreate"));
    assert!(runtime.contains("TodoWrite"));
    assert!(runtime.contains("CronCreate"));
    assert!(request.transient_context.is_none());
}

#[tokio::test]
async fn coordinator_system_prompt_omits_agent_worktree_description_by_default() {
    let mut engine = Engine::with_builtin_tools();
    engine.register_tool(Arc::new(
        rebon_plugin_agents::AgentTool::with_registry_and_options(
            Arc::new(AgentRegistry::builtins_only()),
            false,
        ),
    ));
    let engine = Arc::new(engine);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "configured response"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("coord_prompt_no_worktree");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());
    let config = crate::system_prompt::SystemPromptConfig {
        model: "mock-model".to_string(),
        model_marketing_name: None,
        knowledge_cutoff: None,
        tool_names: vec!["Agent".to_string()],
        deferred_tool_names: Vec::new(),
        platform: "test".to_string(),
        shell: "bash".to_string(),
        os_version: "TestOS 1.0".to_string(),
        language: None,
        normal_system_prompt_override: None,
        minimal_system_prompt_override: None,
        chat_system_prompt_override: None,
        auto_continue_background_agents: true,
    };
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system_prompt_config(config)
        .with_server_state(state.clone())
        .with_coordinator_mode(true);

    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "test".into(),
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
        coordinator_mode: Some(true),
        coordinator_report_paths: Vec::new(),
        user_message_uuid: None,
        background_agent_system: None,
        background_agent_tool_filter: None,
        execution_policy: None,
        replay_requests: Vec::new(),
        skill_invocations: Vec::new(),
    };

    let outcome = executor.execute(request).await;
    assert!(outcome.is_ok(), "execute failed: {:?}", outcome.err());
    let prompt = client_handle
        .captured_requests()
        .into_iter()
        .next()
        .and_then(|request| request.system)
        .expect("captured request system prompt");
    assert!(!prompt.contains("isolated git worktree"));
    assert!(!prompt.contains("worker-local branch"));
    assert!(!prompt.contains("batch-worker"));
    assert!(prompt.contains("Use `task_kind: \"implementation\"`"));
}

/// A main session is told where its scratchpad is, not just sub-agents.
///
/// The resolver takes the session id; the turn that renders the prompt
/// has to hand it over. Passing `None` there resolves no directory and
/// the section silently disappears from every main-session prompt while
/// `build_tool_context` still auto-approves writes into it — the model
/// gets the write root and never the address.
///
/// The second turn asserts the other half: the resolved directory is a
/// pure function of (cwd, session id), so the frozen runtime-context
/// block keys the same both turns and stays byte-identical.
#[tokio::test]
async fn session_system_prompt_names_the_session_scratchpad_directory() {
    let _guard = env_lock();
    let engine = Arc::new(Engine::with_builtin_tools());
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_scratchpad_1", "first"));
    client.push_turn(text_turn("msg_scratchpad_2", "second"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("session_scratchpad_prompt");
    let projects_root = projects_root_dir.path();
    let cwd = projects_root.to_string_lossy().to_string();
    let session_id = "session-scratchpad-prompt";
    let config = crate::system_prompt::SystemPromptConfig {
        model: "mock-model".to_string(),
        model_marketing_name: None,
        knowledge_cutoff: None,
        tool_names: vec!["Read".to_string()],
        deferred_tool_names: Vec::new(),
        platform: "test".to_string(),
        shell: "bash".to_string(),
        os_version: "TestOS 1.0".to_string(),
        language: None,
        normal_system_prompt_override: None,
        minimal_system_prompt_override: None,
        chat_system_prompt_override: None,
        auto_continue_background_agents: true,
    };
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system_prompt_config(config);

    executor
        .execute(basic_prompt_request(session_id, &cwd))
        .await
        .expect("first turn");
    executor
        .execute(basic_prompt_request(session_id, &cwd))
        .await
        .expect("second turn");

    let expected_dir = crate::system_prompt::scratchpad_dir_for(&cwd, session_id);
    let captured = client_handle.captured_requests();
    assert_eq!(captured.len(), 2, "two turns captured");
    let prompt_text = |request: &rebon_api::CreateMessageRequest| {
        format!(
            "{}\n{}",
            request.system.as_deref().unwrap_or_default(),
            captured_runtime_context_text(request)
        )
    };
    let first = prompt_text(&captured[0]);
    assert!(
        first.contains("# Scratchpad Directory"),
        "main session prompt must carry the scratchpad section: {first}"
    );
    assert!(
        first.contains(&expected_dir),
        "prompt must name {expected_dir}: {first}"
    );
    assert_eq!(
        captured_runtime_context_text(&captured[0]),
        captured_runtime_context_text(&captured[1]),
        "the frozen runtime context must be byte-identical across turns"
    );
}

#[tokio::test]
async fn coordinator_system_prompt_mentions_worktree_when_config_enabled() {
    let mut engine = Engine::with_builtin_tools();
    engine.register_tool(Arc::new(
        rebon_plugin_agents::AgentTool::with_registry_and_options(
            Arc::new(AgentRegistry::builtins_only()),
            true,
        ),
    ));
    let engine = Arc::new(engine);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "configured response"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("coord_prompt_with_worktree");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());
    let config = crate::system_prompt::SystemPromptConfig {
        model: "mock-model".to_string(),
        model_marketing_name: None,
        knowledge_cutoff: None,
        tool_names: vec!["Agent".to_string()],
        deferred_tool_names: Vec::new(),
        platform: "test".to_string(),
        shell: "bash".to_string(),
        os_version: "TestOS 1.0".to_string(),
        language: None,
        normal_system_prompt_override: None,
        minimal_system_prompt_override: None,
        chat_system_prompt_override: None,
        auto_continue_background_agents: true,
    };
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system_prompt_config(config)
        .with_server_state(state.clone())
        .with_coordinator_mode(true)
        .with_coordinator_use_worktree(true);

    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "test".into(),
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
        coordinator_mode: Some(true),
        coordinator_report_paths: Vec::new(),
        user_message_uuid: None,
        background_agent_system: None,
        background_agent_tool_filter: None,
        execution_policy: None,
        replay_requests: Vec::new(),
        skill_invocations: Vec::new(),
    };

    let outcome = executor.execute(request).await;
    assert!(outcome.is_ok(), "execute failed: {:?}", outcome.err());
    let prompt = client_handle
        .captured_requests()
        .into_iter()
        .next()
        .and_then(|request| request.system)
        .expect("captured request system prompt");
    assert!(prompt.contains("worktree"));
    assert!(prompt.contains("worker-local branch"));
}

#[tokio::test]
async fn tool_use_round_trip_entries_accumulated_in_transcript() {
    // A tool-use turn should produce user + assistant + tool_result
    // entries in loaded_transcript.
    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool.clone() as Arc<dyn Tool>);
    let client = MockModelClient::new();
    // Turn: tool call then final text
    client.push_turn(tool_turn(
        "msg_tool",
        "Bash",
        "toolu_1",
        r#"{"command":"echo hi"}"#,
    ));
    client.push_turn(text_turn("msg_done", "done"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("history_tool_use");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());

    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state.clone());

    let cancel = CancelToken::new();
    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "run a command".into(),
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
    executor.execute(request).await.unwrap();

    let record = state.get_session(&session.id).unwrap();
    // Should have: user, assistant (tool_use), user (tool_result), assistant (text)
    assert!(
        record.loaded_transcript.len() >= 4,
        "expected at least 4 entries for tool-use round trip, got {}",
        record.loaded_transcript.len()
    );

    // Verify entry types in order
    let types: Vec<&str> = record
        .loaded_transcript
        .iter()
        .map(|e| e.entry_type.as_str())
        .collect();
    assert_eq!(types[0], "user", "first entry should be user prompt");
    assert_eq!(
        types[1], "assistant",
        "second should be assistant (tool_use)"
    );
    assert_eq!(types[2], "user", "third should be user (tool_result)");
    assert_eq!(
        types[3], "assistant",
        "fourth should be assistant (final text)"
    );
}

#[tokio::test]
async fn persists_tool_use_results_map_for_edit_tool() {
    let raw = json!({
        "type": "update",
        "filePath": "/tmp/a.rs",
        "oldString": "before",
        "newString": "after",
        "originalFile": "before",
    });
    let entry = run_single_tool_turn(
        "edit_persist",
        "Edit",
        "toolu_edit",
        raw.clone(),
        false,
        false,
    )
    .await;
    let results = entry
        .raw
        .get("toolUseResults")
        .and_then(Value::as_object)
        .expect("toolUseResults should be present");
    let got = results
        .get("toolu_edit")
        .expect("tool_use_id should map to raw output");
    assert_eq!(
        got.get("filePath").and_then(Value::as_str),
        Some("/tmp/a.rs")
    );
    assert_eq!(got.get("oldString").and_then(Value::as_str), Some("before"));
    assert_eq!(got.get("newString").and_then(Value::as_str), Some("after"));
}

#[tokio::test]
async fn persists_tool_use_results_map_for_write_tool() {
    let raw = json!({
        "type": "create",
        "filePath": "/tmp/new.rs",
        "oldString": "",
        "newString": "fresh content",
    });
    let entry = run_single_tool_turn(
        "write_persist",
        "Write",
        "toolu_write",
        raw.clone(),
        false,
        false,
    )
    .await;
    let got = entry
        .raw
        .get("toolUseResults")
        .and_then(Value::as_object)
        .and_then(|m| m.get("toolu_write"))
        .expect("toolUseResults should carry the Write payload");
    assert_eq!(
        got.get("filePath").and_then(Value::as_str),
        Some("/tmp/new.rs")
    );
    assert_eq!(
        got.get("newString").and_then(Value::as_str),
        Some("fresh content")
    );
    assert_eq!(got.get("oldString").and_then(Value::as_str), Some(""));
}

#[tokio::test]
async fn trims_read_tool_body_from_transcript() {
    let raw = json!({
        "file": {
            "filePath": "/tmp/big.rs",
            "content": "this is a very long file body that should not land in the transcript",
        },
        "type": "text",
        "numLines": 42,
    });
    let entry =
        run_single_tool_turn("read_trim", "Read", "toolu_read", raw.clone(), false, false).await;
    let got = entry
        .raw
        .get("toolUseResults")
        .and_then(Value::as_object)
        .and_then(|m| m.get("toolu_read"))
        .expect("toolUseResults should carry the Read payload");
    let file = got
        .get("file")
        .and_then(Value::as_object)
        .expect("file object should survive the trim");
    assert!(
        !file.contains_key("content"),
        "Read body should be stripped from the transcript"
    );
    assert_eq!(
        file.get("filePath").and_then(Value::as_str),
        Some("/tmp/big.rs"),
        "structural fields should be preserved"
    );
    assert_eq!(got.get("numLines").and_then(Value::as_i64), Some(42));
}

#[tokio::test]
async fn skips_failed_tool_output_in_tool_use_results() {
    let entry = run_single_tool_turn(
        "fail_skip",
        "Read",
        "toolu_fail",
        Value::Null, // ignored when failing = true
        true,
        false,
    )
    .await;
    match entry.raw.get("toolUseResults") {
        None => {} // preferred shape — key elided entirely
        Some(Value::Object(map)) => assert!(
            !map.contains_key("toolu_fail"),
            "failed tool should not leak a raw output into toolUseResults"
        ),
        Some(other) => panic!("toolUseResults must be an object, got {other:?}"),
    }
}

#[tokio::test]
async fn typed_failure_persists_display_metadata_outside_tool_use_results() {
    let entry = run_single_tool_turn(
        "presented_fail_skip",
        "SendMessage",
        "toolu_presented_fail",
        Value::Null,
        false,
        true,
    )
    .await;

    match entry.raw.get("toolUseResults") {
        None => {}
        Some(Value::Object(map)) => assert!(!map.contains_key("toolu_presented_fail")),
        Some(other) => panic!("toolUseResults must be an object, got {other:?}"),
    }
    let presentation = entry
        .raw
        .get("toolErrorPresentations")
        .and_then(Value::as_object)
        .and_then(|map| map.get("toolu_presented_fail"))
        .expect("typed display metadata should be persisted separately");
    assert_eq!(
        presentation.get("code").and_then(Value::as_str),
        Some("agent_closed")
    );
    assert_eq!(
        presentation.get("displayMessage").and_then(Value::as_str),
        Some("Agent is no longer available.")
    );
    assert!(!presentation.to_string().contains("fresh worker"));

    let model_content = entry.raw["message"]["content"][0]["content"]
        .as_str()
        .expect("model tool result should remain textual");
    assert!(model_content.contains("fresh worker"));
}

/// Codex-style Responses backends reject a request outright
/// (`invalid_function_parameters`, 400) when any function schema has
/// something other than a plain `type: "object"` at the top level — a
/// top-level `oneOf`/`anyOf`/`allOf`/`enum`/`const`/`not` poisons the
/// whole turn. PlanLedger and TaskStop both shipped with one; sweep
/// every registered tool so a new tool cannot reintroduce it.
#[test]
fn builtin_tool_schemas_are_provider_compatible() {
    let mut engine = Engine::with_builtin_tools();
    // Not in the builtin registration list, but reachable through the
    // deferred-tool selector.
    engine.register_tool(Arc::new(rebon_plugin_tasks::TaskStopTool));
    for snapshot in engine.tool_snapshots() {
        let schema = &snapshot.input_schema;
        assert_eq!(
            schema.get("type").and_then(Value::as_str),
            Some("object"),
            "tool `{}` schema must have top-level type \"object\"",
            snapshot.name
        );
        for key in ["oneOf", "anyOf", "allOf", "enum", "const", "not", "$ref"] {
            assert!(
                schema.get(key).is_none(),
                "tool `{}` schema must not have top-level `{key}`",
                snapshot.name
            );
        }
    }
}

/// One executor, many sessions, one policy handle each — the shape `--acp`
/// and the `serve` page behind it run on.
///
/// The handle says *whose* session it is, so a server that mints a session
/// per client cannot hold one fixed handle. Before the resolver it held
/// none, and every hook those two surfaces' users had configured went unrun.
/// What this pins is that the resolver is asked per turn and that its answer
/// reaches the gate: the same executor refuses one session's `Bash` and runs
/// the other's.
#[tokio::test(flavor = "current_thread")]
async fn a_shared_executor_gates_each_turn_with_that_sessions_policy_handle() {
    let _guard = env_lock();

    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool.clone() as Arc<dyn Tool>);
    let client = MockModelClient::new();
    // One tool call and one closing message per session, in the order the
    // two turns consume them.
    for (call, tool_message, text_message) in
        [("call_1", "msg_1", "msg_2"), ("call_2", "msg_3", "msg_4")]
    {
        client.push_turn(tool_turn(tool_message, "Bash", call, r#"{"command":"ls"}"#));
        client.push_turn(text_turn(text_message, "done"));
    }
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("per-session-policy");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let refused = state.create_session(cwd.clone(), Vec::new());
    let allowed = state.create_session(cwd.clone(), Vec::new());

    let refused_id = refused.id.clone();
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state.clone())
        // A store, because the hook wrapper only wraps a broker that exists;
        // both real hosts of the resolver set one.
        .with_policy_store(crate::policy::PolicyStore::new())
        .with_policy_resolver(Arc::new(move |session_id: &str, cwd: &str| {
            let sources = crate::policy_seat::PolicySources::default().with_context(
                crate::policy_seat::PolicyContext {
                    cwd: cwd.to_string(),
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
            );
            if session_id == refused_id {
                sources.with_subscriber(
                    "test/refuses-every-tool",
                    crate::turn_hook::Order::NORMAL,
                    Arc::new(RefusesEveryTool),
                )
            } else {
                sources
            }
        }));

    let prompt = |session_id: &str| rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session_id.to_string(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "list the files".into(),
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

    executor
        .execute(prompt(&refused.id))
        .await
        .expect("the refused session's turn still completes");
    assert_eq!(
        tool.call_count(),
        0,
        "the subscriber on this session's handle did not gate its tool call"
    );

    executor
        .execute(prompt(&allowed.id))
        .await
        .expect("the other session's turn completes");
    assert_eq!(
        tool.call_count(),
        1,
        "one session's refusal leaked onto another session's turn"
    );
}
