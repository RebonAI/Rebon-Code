use super::*;

#[test]
fn durable_transient_context_inserts_before_current_user_and_skips_duplicates() {
    let mut messages = vec![
        ApiMessage::user_text("previous user"),
        ApiMessage::assistant_text("previous assistant"),
        ApiMessage::user_text("current user"),
    ];
    let mut transient = Some("gitStatus: dirty".to_string());

    let inserted = materialize_durable_transient_context(&mut messages, &mut transient)
        .expect("runtime context inserted");

    assert!(transient.is_none());
    assert_eq!(messages.len(), 4);
    assert!(rebon_api::is_runtime_context_message(&inserted));
    assert!(rebon_api::is_runtime_context_message(&messages[2]));
    assert_eq!(messages[3].content[0].as_text(), Some("current user"));

    messages.push(ApiMessage::assistant_text("reply"));
    messages.push(ApiMessage::user_text("next user"));
    let mut same_transient = Some("gitStatus: dirty".to_string());

    assert!(materialize_durable_transient_context(&mut messages, &mut same_transient).is_none());
    assert!(same_transient.is_none());
    assert_eq!(
        messages
            .iter()
            .filter(|message| rebon_api::is_runtime_context_message(message))
            .count(),
        1
    );
}

#[test]
fn durable_transient_context_requires_current_user_tail() {
    let mut messages = vec![
        ApiMessage::user_text("previous user"),
        ApiMessage::assistant_text("previous assistant"),
    ];
    let mut transient = Some("gitStatus: dirty".to_string());

    assert!(materialize_durable_transient_context(&mut messages, &mut transient).is_none());
    assert_eq!(transient.as_deref(), Some("gitStatus: dirty"));
    assert_eq!(messages.len(), 2);
    assert!(!messages.iter().any(rebon_api::is_runtime_context_message));
}

#[tokio::test]
async fn run_query_materializes_transient_for_clients_without_request_scoped_support() {
    let engine = Arc::new(Engine::new());
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "done"));
    let handle = mock.clone();
    let client: Arc<dyn ModelClient> = Arc::new(NoTransientMockClient::new(mock));
    let mut params = QueryParams::new("mock-model", vec![ApiMessage::user_text("do work")]);
    params.runtime_context_message = Some("stable runtime".to_string());
    params.transient_context_message = Some("gitStatus: dirty".to_string());

    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    while let Some(event) = rx.recv().await {
        if matches!(event, QueryEvent::Done { .. } | QueryEvent::Error(_)) {
            break;
        }
    }

    let request = handle
        .captured_requests()
        .into_iter()
        .next()
        .expect("captured request");
    assert!(request.transient_context.is_none());
    let bodies = request
        .messages
        .iter()
        .filter_map(rebon_api::runtime_context_body_from_message)
        .collect::<Vec<_>>();
    assert_eq!(bodies, vec!["stable runtime", "gitStatus: dirty"]);
    assert_eq!(
        request.messages.last().unwrap().content[0].as_text(),
        Some("do work")
    );
}

#[test]
fn request_budget_estimate_includes_runtime_transient_and_tool_metadata() {
    let manager = ContextManager::new(
        Some("system".to_string()),
        vec![ApiMessage::user_text("short prompt")],
    );
    let mut params = QueryParams::new("mock-model", Vec::new());
    params.system = Some("system".to_string());
    params.runtime_context_message = Some("stable runtime ".repeat(400));
    params.transient_context_message = Some("gitStatus: dirty\n".repeat(200));
    params.tools = vec![ApiTool {
        name: "HugeTool".to_string(),
        description: "large tool metadata ".repeat(200),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "x".repeat(400) }
            }
        }),
    }];

    let manager_only = manager.next_request_estimate();
    let request_estimate = next_request_input_estimate(&manager, &params);
    assert!(
        request_estimate > manager_only.saturating_add(2_000),
        "request estimate should include runtime/transient/tool overhead: manager={manager_only}, request={request_estimate}"
    );

    let history_target =
        history_target_for_request_budget(&manager, &params, request_estimate.saturating_sub(500));
    assert!(
        history_target < request_estimate.saturating_sub(500),
        "history target must subtract dynamic request overhead"
    );
}

#[test]
fn request_budget_estimate_prefers_server_usage_baseline() {
    let mut manager = ContextManager::new(None, vec![ApiMessage::user_text("short prompt")]);
    manager.set_usage_baseline(90_000);
    manager.push_message(ApiMessage::assistant_text("small response"));
    let params = QueryParams::new("mock-model", Vec::new());

    let local_estimate = full_request_input_estimate(&manager, &params);
    let next_request_estimate = next_request_input_estimate(&manager, &params);

    assert!(
        local_estimate < 90_000,
        "test setup requires a local underestimate"
    );
    assert!(
        next_request_estimate >= 90_000,
        "server usage baseline must survive local underestimation: local={local_estimate}, next={next_request_estimate}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn execute_persists_durable_transient_before_user_when_provider_requires_history() {
    if std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| !status.success())
        .unwrap_or(true)
    {
        return;
    }

    let _guard = env_lock();
    let _stable_guard = EnvVarGuard::set("REBON_STABLE_BASE_SYSTEM", "1");
    let projects_root_dir = temp_projects_root("durable_transient_history");
    let projects_root = projects_root_dir.path();
    let run_git = |args: &[&str]| -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(projects_root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };
    assert!(run_git(&["init", "-b", "main"]));
    assert!(run_git(&["config", "user.email", "t@example.com"]));
    assert!(run_git(&["config", "user.name", "Test User"]));
    std::fs::write(projects_root.join("tracked.txt"), "hello").unwrap();
    assert!(run_git(&["add", "tracked.txt"]));
    assert!(run_git(&["commit", "-m", "init"]));
    std::fs::write(projects_root.join("dirty.txt"), "pending").unwrap();

    let engine = Arc::new(Engine::new());
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("msg_1", "done"));
    let handle = mock.clone();
    let client: Arc<dyn ModelClient> = Arc::new(NoTransientMockClient::new(mock));
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system_prompt_config(test_system_prompt_config())
        .with_server_state(state.clone());

    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "do work".into(),
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

    let captured = handle.captured_requests();
    assert_eq!(captured.len(), 1);
    assert!(captured[0].transient_context.is_none());
    let bodies = captured[0]
        .messages
        .iter()
        .filter_map(rebon_api::runtime_context_body_from_message)
        .collect::<Vec<_>>();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[1].contains("gitStatus:"));
    assert!(bodies[1].contains("dirty.txt"));
    assert_eq!(
        captured[0].messages.last().unwrap().content[0].as_text(),
        Some("do work")
    );

    let record = state.get_session(&session.id).expect("session record");
    assert!(
        record.loaded_transcript.len() >= 2,
        "runtime note and user prompt should both persist"
    );
    assert_eq!(
        record.loaded_transcript[0]
            .raw
            .get("runtimeContext")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    let replayed = transcript_to_api_messages(&record.loaded_transcript);
    assert!(rebon_api::is_runtime_context_message(&replayed[0]));
    assert_eq!(replayed[1].content[0].as_text(), Some("do work"));
}

#[test]
fn runtime_context_request_message_is_virtual_only() {
    let manager = ContextManager::new(
        Some("system".to_string()),
        vec![ApiMessage {
            role: Role::User,
            content: vec![ApiContentBlock::Text(TextBlock {
                text: "hello".to_string(),
            })],
        }],
    );

    let request_messages =
        request_messages_with_runtime_context(&manager, Some("dynamic cwd context"));

    assert_eq!(manager.messages_for_request().len(), 1);
    assert_eq!(request_messages.len(), 2);
    assert_eq!(request_messages[0].role, Role::User);
    match &request_messages[0].content[0] {
        ApiContentBlock::Text(text) => {
            assert!(text.text.contains("<system-reminder>"));
            assert!(text.text.contains("<runtime_context>"));
            assert!(text.text.contains("dynamic cwd context"));
            assert!(text.text.contains("</system-reminder>"));
        }
        other => panic!("expected runtime text block, got {other:?}"),
    }
    match &request_messages[1].content[0] {
        ApiContentBlock::Text(text) => assert_eq!(text.text, "hello"),
        other => panic!("expected original user text block, got {other:?}"),
    }
}

#[test]
fn context_reset_restores_runtime_context_message() {
    let engine = Engine::new();
    let mut params = QueryParams::new("model", Vec::new());
    params.runtime_context_message = Some("planning runtime".to_string());
    params.transient_context_message = Some("planning transient".to_string());
    params.post_context_reset_system = Some("base execution system".to_string());
    params.post_context_reset_runtime_context_message = Some("execution runtime".to_string());
    params.post_context_reset_transient_context_message = Some("execution transient".to_string());
    let mut context = ToolContext::new();

    release_request_policy_after_context_reset(&engine, &mut params, &mut context);

    assert_eq!(params.system.as_deref(), Some("base execution system"));
    assert_eq!(
        params.runtime_context_message.as_deref(),
        Some("execution runtime")
    );
    assert_eq!(
        params.transient_context_message.as_deref(),
        Some("execution transient")
    );
}

/// A real git-status block, as each prompt profile renders it.
fn git_status_block(profile: rebon_types::prompt_profile::PromptProfile) -> String {
    let dir = tempfile::tempdir().unwrap();
    let initialised = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .is_ok_and(|status| status.success());
    let body = initialised
        .then(|| {
            crate::system_prompt::collect_git_status_for_profile(
                &dir.path().to_string_lossy(),
                profile,
            )
        })
        .flatten()
        .unwrap_or_else(|| "Current branch: main\n\nStatus:\n(clean)".to_string());
    format!("gitStatus: {body}")
}

fn mentions_git_status(message: &ApiMessage) -> bool {
    message.content.iter().any(|block| {
        block
            .as_text()
            .is_some_and(|text| text.contains("gitStatus:"))
    })
}

/// After a tool round, the request a provider receives ends with the
/// tool_result message: the git status is not appended after it, where it
/// reads to the model as the user speaking again. It went in once, before
/// the turn's prompt. Under both prompt profiles, and whether or not the
/// client supports request-scoped context.
#[tokio::test]
async fn a_tool_round_is_never_followed_by_a_git_status_block() {
    use rebon_types::prompt_profile::PromptProfile;
    for profile in [PromptProfile::Legacy, PromptProfile::Lean] {
        for request_scoped in [true, false] {
            let label = format!("{profile:?}, request-scoped {request_scoped}");
            let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "file body"})));
            let engine = build_engine_with(tool as Arc<dyn Tool>);
            let mock = MockModelClient::new();
            mock.set_supports_request_scoped_transient_context(request_scoped);
            mock.push_turn(tool_turn(
                "msg_tool",
                "Read",
                "toolu_read",
                "{\"path\":\"a.rs\"}",
            ));
            mock.push_turn(text_turn("msg_done", "done"));
            let captured = mock.clone();
            let mut params = QueryParams::new("mock", vec![ApiMessage::user_text("do work")])
                .with_tools(tools_from_engine(&engine));
            params.transient_context_message = Some(git_status_block(profile));

            let mut rx = run_query(
                engine,
                SessionHandle::new(Arc::new(mock)),
                params,
                ToolContext::new().with_cwd("/tmp/repo"),
                CancelToken::new(),
            );
            let events = drain(&mut rx).await;
            assert!(
                matches!(events.last(), Some(QueryEvent::Done { .. })),
                "{label}"
            );

            let requests = captured.captured_requests();
            assert_eq!(requests.len(), 2, "{label}");
            // What the provider sends: any request-scoped context folded in.
            let first = requests[0].messages_with_transient_context();
            let after_tools = requests[1].messages_with_transient_context();

            let last = after_tools.last().unwrap();
            assert_eq!(last.role, rebon_api::Role::User, "{label}");
            assert!(
                last.content
                    .iter()
                    .all(|block| matches!(block, rebon_api::ContentBlock::ToolResult(_))),
                "{label}: something follows the tool results: {last:?}"
            );
            for messages in [&first, &after_tools] {
                let blocks: Vec<usize> = messages
                    .iter()
                    .enumerate()
                    .filter(|(_, message)| mentions_git_status(message))
                    .map(|(index, _)| index)
                    .collect();
                let prompt = messages
                    .iter()
                    .position(|message| {
                        message.content.first().and_then(|block| block.as_text()) == Some("do work")
                    })
                    .unwrap();
                assert_eq!(
                    blocks,
                    vec![prompt - 1],
                    "{label}: one block, right before the prompt"
                );
            }
        }
    }
}

/// Across turns the git status is sent again only when it changed, at the
/// start of the turn that sees the change, before its prompt. Once history
/// no longer holds the last copy (compaction, a reset) it counts as unsent.
#[test]
fn git_status_is_sent_again_only_when_it_changed() {
    let clean = "gitStatus: Current branch: main\n\nStatus:\n(clean)".to_string();
    let dirty = "gitStatus: Current branch: main\n\nStatus:\n M src/lib.rs".to_string();
    let count = |messages: &[ApiMessage]| {
        messages
            .iter()
            .filter(|message| rebon_api::is_runtime_context_message(message))
            .count()
    };

    let mut messages = vec![ApiMessage::user_text("turn one")];
    assert!(
        materialize_durable_transient_context(&mut messages, &mut Some(clean.clone())).is_some()
    );
    messages.push(ApiMessage::assistant_text("reply one"));
    messages.push(ApiMessage::user_text("turn two"));

    // Unchanged: no second block.
    assert!(
        materialize_durable_transient_context(&mut messages, &mut Some(clean.clone())).is_none()
    );
    assert_eq!(count(&messages), 1);

    // Changed: exactly one new block, right before this turn's prompt.
    messages.push(ApiMessage::assistant_text("reply two"));
    messages.push(ApiMessage::user_text("turn three"));
    assert!(
        materialize_durable_transient_context(&mut messages, &mut Some(dirty.clone())).is_some()
    );
    assert_eq!(count(&messages), 2);
    let tail = messages.len();
    assert_eq!(
        rebon_api::runtime_context_body_from_message(&messages[tail - 2]),
        Some(dirty.as_str())
    );
    assert_eq!(messages[tail - 1].content[0].as_text(), Some("turn three"));

    // Compacted history no longer holds it: sent again.
    let mut compacted = vec![
        ApiMessage::user_text("summary of earlier turns"),
        ApiMessage::assistant_text("ok"),
        ApiMessage::user_text("turn four"),
    ];
    assert!(materialize_durable_transient_context(&mut compacted, &mut Some(dirty)).is_some());
    assert_eq!(count(&compacted), 1);
}
