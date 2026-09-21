use super::*;

#[tokio::test]
async fn assistant_transcript_entry_records_served_model_and_requested_mismatch() {
    // The provider-reported model (from `message_start`) lands in
    // `message.model`; when it differs from the requested model (a dated
    // snapshot alias, or a relay routing to a different backend) the
    // requested one is preserved under `requestedModel`.
    let engine = build_engine_with_tools(Vec::new());
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_model", "hello"));
    let client_arc: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("served_model_mismatch");
    let projects_root = projects_root_dir.path();
    let session_id = "sess-served-model";
    let cwd = "/tmp/served-model";
    let executor = EngineQueryExecutor::new(engine, client_arc, projects_root, "mock-model");
    executor
        .execute(basic_prompt_request(session_id, cwd))
        .await
        .unwrap();

    let assistant = read_assistant_transcript_entries(projects_root, cwd, session_id);
    assert_eq!(assistant.len(), 1, "expected one assistant entry");
    assert_eq!(assistant[0]["message"]["id"], "msg_model");
    assert_eq!(assistant[0]["message"]["model"], "mock");
    assert_eq!(assistant[0]["requestedModel"], "mock-model");
}

#[tokio::test]
async fn assistant_transcript_entry_falls_back_to_requested_model_when_unreported() {
    // Relays that don't echo a model in `message_start` must not leave
    // the transcript without one — the requested model is the best
    // available answer, and no `requestedModel` sibling is written.
    let engine = build_engine_with_tools(Vec::new());
    let client = MockModelClient::new();
    let mut turn = text_turn("msg_noreport", "hello");
    if let StreamEvent::MessageStart { model, .. } = &mut turn[0] {
        model.clear();
    }
    client.push_turn(turn);
    let client_arc: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("served_model_fallback");
    let projects_root = projects_root_dir.path();
    let session_id = "sess-model-fallback";
    let cwd = "/tmp/model-fallback";
    let executor = EngineQueryExecutor::new(engine, client_arc, projects_root, "mock-model");
    executor
        .execute(basic_prompt_request(session_id, cwd))
        .await
        .unwrap();

    let assistant = read_assistant_transcript_entries(projects_root, cwd, session_id);
    assert_eq!(assistant.len(), 1, "expected one assistant entry");
    assert_eq!(assistant[0]["message"]["model"], "mock-model");
    assert!(
        assistant[0].get("requestedModel").is_none(),
        "fallback must not duplicate the model under requestedModel"
    );
}

#[tokio::test]
async fn assistant_transcript_entry_omits_requested_model_when_it_matches_served() {
    let engine = build_engine_with_tools(Vec::new());
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_match", "hello"));
    let client_arc: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("served_model_match");
    let projects_root = projects_root_dir.path();
    let session_id = "sess-model-match";
    let cwd = "/tmp/model-match";
    // Requested model matches the mock stream's `message_start` model.
    let executor = EngineQueryExecutor::new(engine, client_arc, projects_root, "mock");
    executor
        .execute(basic_prompt_request(session_id, cwd))
        .await
        .unwrap();

    let assistant = read_assistant_transcript_entries(projects_root, cwd, session_id);
    assert_eq!(assistant.len(), 1, "expected one assistant entry");
    assert_eq!(assistant[0]["message"]["model"], "mock");
    assert!(
        assistant[0].get("requestedModel").is_none(),
        "matching served/requested models must not write requestedModel"
    );
}

#[tokio::test]
async fn engine_query_executor_replays_loaded_transcript_into_history() {
    use rebon_session_state::ServerState;

    // Build a session record with a pre-seeded loaded_transcript.
    let state = Arc::new(ServerState::new());
    state.mark_initialized().unwrap();
    let sid = "sess-replay".to_string();
    // Insert a session whose `loaded_transcript` carries one
    // user + one assistant entry. We use the public create +
    // a direct mutation via a test-only helper below.
    insert_session_with_transcript(
        &state,
        &sid,
        "/tmp/replay",
        vec![
            make_user_entry("u1", "what is 2+2?"),
            make_assistant_entry("a1", "u1", "4"),
        ],
    );

    // Tool-less engine + mock client that returns a single
    // text turn. The mock captures the request, so we can
    // assert the replayed messages showed up in the history.
    let engine = Arc::new(Engine::new());
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_final", "still 4"));
    let client_arc: Arc<dyn ModelClient> = Arc::new(client.clone());

    let projects_root_dir = temp_projects_root("replay");
    let executor =
        EngineQueryExecutor::new(engine, client_arc, projects_root_dir.path(), "mock-model")
            .with_server_state(state);

    let outcome = executor
        .execute(PromptRequest {
            user_prompt: None,
            effort_is_session_default: false,
            session_id: sid.clone(),
            cwd: "/tmp/replay".into(),
            prompt: vec![AcpContentBlock::Text(TextContent {
                text: "are you sure?".into(),
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

    // The mock saw exactly one call (the single turn we
    // pushed). The request message list should contain the
    // replayed user + replayed assistant + the new user
    // prompt, in order.
    let captured = client.captured_requests();
    assert_eq!(captured.len(), 1);
    let req = &captured[0];
    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[0].role, rebon_api::Role::User);
    assert_eq!(req.messages[0].content[0].as_text(), Some("what is 2+2?"));
    assert_eq!(req.messages[1].role, rebon_api::Role::Assistant);
    assert_eq!(req.messages[1].content[0].as_text(), Some("4"));
    assert_eq!(req.messages[2].role, rebon_api::Role::User);
    assert_eq!(req.messages[2].content[0].as_text(), Some("are you sure?"));
}

#[test]
fn transcript_to_api_messages_reconstructs_tool_use_blocks() {
    let entries = vec![
        make_user_entry("u1", "read a.rs"),
        rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "a1".into(),
            parent_uuid: Some("u1".into()),
            timestamp: Some("2026-04-09T00:01:00.000Z".into()),
            raw: json!({
                "type": "assistant",
                "uuid": "a1",
                "parentUuid": "u1",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Let me read that."},
                        {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "Read",
                            "input": { "file_path": "a.rs" }
                        }
                    ]
                }
            }),
        },
    ];
    let messages = transcript_to_api_messages(&entries);
    // 3 messages: user, assistant (with tool_use), synthetic user
    // (repair inserts a tool_result for the orphan tool_use).
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].role, rebon_api::Role::User);

    let assistant = &messages[1];
    assert_eq!(assistant.role, rebon_api::Role::Assistant);
    assert_eq!(assistant.content.len(), 2);
    match &assistant.content[0] {
        rebon_api::ContentBlock::Text(t) => assert_eq!(t.text, "Let me read that."),
        other => panic!("expected text block, got {other:?}"),
    }
    match &assistant.content[1] {
        rebon_api::ContentBlock::ToolUse(tu) => {
            assert_eq!(tu.id, "toolu_1");
            assert_eq!(tu.name, "Read");
            assert_eq!(tu.input, json!({"file_path": "a.rs"}));
        }
        other => panic!("expected tool_use block, got {other:?}"),
    }
    // Synthetic tool_result appended by ensure_tool_result_pairing.
    let synthetic_user = &messages[2];
    assert_eq!(synthetic_user.role, rebon_api::Role::User);
    match &synthetic_user.content[0] {
        rebon_api::ContentBlock::ToolResult(tr) => {
            assert_eq!(tr.tool_use_id, "toolu_1");
            assert!(tr.is_error);
        }
        other => panic!("expected synthetic tool_result, got {other:?}"),
    }
}

#[test]
fn transcript_to_api_messages_reconstructs_tool_result_blocks() {
    // A preceding assistant message with matching tool_use blocks
    // is required so ensure_tool_result_pairing does not strip the
    // tool_results as orphans.
    let entries = vec![
        rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "a1".into(),
            parent_uuid: None,
            timestamp: Some("2026-04-09T00:01:00.000Z".into()),
            raw: json!({
                "type": "assistant",
                "uuid": "a1",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}},
                        {"type": "tool_use", "id": "toolu_2", "name": "Read", "input": {}}
                    ]
                }
            }),
        },
        rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-tr".into(),
            parent_uuid: Some("a1".into()),
            timestamp: Some("2026-04-09T00:02:00.000Z".into()),
            raw: json!({
                "type": "user",
                "uuid": "u-tr",
                "parentUuid": "a1",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_1",
                            "content": "file body goes here",
                            "is_error": false
                        },
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_2",
                            "content": "permission denied",
                            "is_error": true
                        }
                    ]
                }
            }),
        },
    ];
    let messages = transcript_to_api_messages(&entries);
    assert_eq!(messages.len(), 2);
    let user = &messages[1];
    assert_eq!(user.role, rebon_api::Role::User);
    assert_eq!(user.content.len(), 2);
    match &user.content[0] {
        rebon_api::ContentBlock::ToolResult(tr) => {
            assert_eq!(tr.tool_use_id, "toolu_1");
            assert_eq!(tr.content, "file body goes here");
            assert!(!tr.is_error);
        }
        other => panic!("expected tool_result block, got {other:?}"),
    }
    match &user.content[1] {
        rebon_api::ContentBlock::ToolResult(tr) => {
            assert_eq!(tr.tool_use_id, "toolu_2");
            assert!(tr.is_error);
        }
        other => panic!("expected tool_result block, got {other:?}"),
    }
}

#[test]
fn transcript_to_api_messages_drops_malformed_tool_use_blocks() {
    let entries = vec![rebon_session::TranscriptEntry {
        entry_type: "assistant".into(),
        uuid: "a-bad".into(),
        parent_uuid: None,
        timestamp: None,
        raw: json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "tool_use", "input": {}},  // missing id + name
                    {"type": "text", "text": " there"}
                ]
            }
        }),
    }];
    let messages = transcript_to_api_messages(&entries);
    assert_eq!(messages.len(), 1);
    // The malformed tool_use is dropped; the two text blocks
    // survive.
    assert_eq!(messages[0].content.len(), 2);
}

#[test]
fn transcript_to_api_messages_preserves_thinking_blocks() {
    let entries = vec![rebon_session::TranscriptEntry {
        entry_type: "assistant".into(),
        uuid: "a1".into(),
        parent_uuid: None,
        timestamp: None,
        raw: json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "let me think...", "signature": "sig-1"},
                    {"type": "redacted_thinking", "data": "ENCRYPTED"},
                    {"type": "text", "text": "ok, the answer is 42"}
                ]
            }
        }),
    }];
    let messages = transcript_to_api_messages(&entries);
    assert_eq!(messages.len(), 1);
    // Reasoning is preserved across resume (matching the live loop) so
    // providers that must replay it — DeepSeek reasoning_content, else a
    // 400 — have it; the wire layer decides what is actually sent.
    assert_eq!(messages[0].content.len(), 3);
    match &messages[0].content[0] {
        rebon_api::ContentBlock::Thinking(tb) => {
            assert_eq!(tb.thinking, "let me think...");
            assert_eq!(tb.signature.as_deref(), Some("sig-1"));
            assert!(tb.data.is_none());
        }
        other => panic!("expected thinking block, got {other:?}"),
    }
    match &messages[0].content[1] {
        rebon_api::ContentBlock::Thinking(tb) => {
            assert!(tb.thinking.is_empty());
            assert_eq!(tb.data.as_deref(), Some("ENCRYPTED"));
        }
        other => panic!("expected redacted thinking block, got {other:?}"),
    }
    assert_eq!(
        messages[0].content[2].as_text(),
        Some("ok, the answer is 42")
    );
}

/// A compaction block stands in for the history it replaced, so unlike
/// the other server-side blocks it has to survive resume — dropping it
/// would come back with the summary's inputs gone and nothing in their
/// place. A block carrying neither payload is a husk and is dropped.
#[test]
fn transcript_to_api_messages_preserves_compaction_blocks() {
    let entries = vec![rebon_session::TranscriptEntry {
        entry_type: "assistant".into(),
        uuid: "a1".into(),
        parent_uuid: None,
        timestamp: None,
        raw: json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "compaction", "encrypted_content": "opaque-blob"},
                    {"type": "compaction", "content": "a plaintext summary"},
                    {"type": "compaction"},
                ]
            }
        }),
    }];
    let messages = transcript_to_api_messages(&entries);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content.len(), 2);
    match &messages[0].content[0] {
        rebon_api::ContentBlock::Compaction(block) => {
            assert_eq!(block.encrypted_content.as_deref(), Some("opaque-blob"));
            assert!(block.content.is_none());
        }
        other => panic!("expected compaction block, got {other:?}"),
    }
    match &messages[0].content[1] {
        rebon_api::ContentBlock::Compaction(block) => {
            assert_eq!(block.content.as_deref(), Some("a plaintext summary"));
            assert!(block.encrypted_content.is_none());
        }
        other => panic!("expected compaction block, got {other:?}"),
    }
}

#[test]
fn transcript_to_api_messages_extracts_text_from_message_field() {
    let entries = vec![
        make_user_entry("u1", "hello"),
        make_assistant_entry("a1", "u1", "hi there"),
        // Unknown entry type should be skipped.
        TranscriptEntry {
            entry_type: "attachment".into(),
            uuid: "att1".into(),
            parent_uuid: Some("a1".into()),
            timestamp: Some("2026-04-09T00:00:02.000Z".into()),
            raw: json!({ "text": "file body" }),
        },
    ];
    let messages = transcript_to_api_messages(&entries);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content[0].as_text(), Some("hello"));
    assert_eq!(messages[1].content[0].as_text(), Some("hi there"));
}

#[test]
fn transcript_to_api_messages_handles_content_as_block_array() {
    let entries = vec![TranscriptEntry {
        entry_type: "assistant".into(),
        uuid: "a1".into(),
        parent_uuid: None,
        timestamp: Some("2026-04-09T00:00:00.000Z".into()),
        raw: json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "hello "},
                    {"type": "text", "text": "world"},
                ]
            }
        }),
    }];
    let messages = transcript_to_api_messages(&entries);
    assert_eq!(messages.len(), 1);
    // Each text block is preserved as its own ContentBlock
    // (lossless round-trip with the API wire format), not
    // concatenated together.
    assert_eq!(messages[0].content.len(), 2);
    assert_eq!(messages[0].content[0].as_text(), Some("hello "));
    assert_eq!(messages[0].content[1].as_text(), Some("world"));
}

#[test]
fn transcript_to_api_messages_skips_empty_and_unknown_entries() {
    let entries = vec![
        TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u".into(),
            parent_uuid: None,
            timestamp: None,
            raw: json!({}),
        },
        TranscriptEntry {
            entry_type: "system".into(),
            uuid: "s".into(),
            parent_uuid: None,
            timestamp: None,
            raw: json!({ "message": { "content": "ignored" } }),
        },
    ];
    let messages = transcript_to_api_messages(&entries);
    assert!(messages.is_empty());
}

#[test]
fn ensure_tool_result_pairing_inserts_synthetic_for_orphan_tool_use() {
    // Simulate: assistant called a tool, user cancelled before result.
    let entries = vec![
        rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u1".into(),
            parent_uuid: None,
            timestamp: None,
            raw: json!({
                "message": { "role": "user", "content": "read foo" }
            }),
        },
        rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "a1".into(),
            parent_uuid: Some("u1".into()),
            timestamp: None,
            raw: json!({
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "toolu_orphan", "name": "Read", "input": {"path": "foo.rs"}}
                    ]
                }
            }),
        },
        // No tool_result follows — user pressed Esc.
        rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u2".into(),
            parent_uuid: None,
            timestamp: None,
            raw: json!({
                "message": { "role": "user", "content": "try again" }
            }),
        },
    ];
    let messages = transcript_to_api_messages(&entries);
    // Should be: user, assistant, patched-user (synthetic tool_result + "try again")
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[1].role, Role::Assistant);
    // The next user message was patched with the synthetic tool_result.
    assert_eq!(messages[2].role, Role::User);
    assert_eq!(messages[2].content.len(), 2);
    // First content is the runtime text "try again".
    assert_eq!(messages[2].content[0].as_text(), Some("try again"));
    // Second is the synthetic tool_result.
    match &messages[2].content[1] {
        rebon_api::ContentBlock::ToolResult(tr) => {
            assert_eq!(tr.tool_use_id, "toolu_orphan");
            assert!(tr.is_error);
            assert!(tr.content.contains("interrupted"));
        }
        other => panic!("expected synthetic tool_result, got {other:?}"),
    }
}

#[test]
fn ensure_tool_result_pairing_patches_existing_user_message() {
    // Simulate: assistant called two tools, only one got a result.
    let entries = vec![
        rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u1".into(),
            parent_uuid: None,
            timestamp: None,
            raw: json!({
                "message": { "role": "user", "content": "do two things" }
            }),
        },
        rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "a1".into(),
            parent_uuid: Some("u1".into()),
            timestamp: None,
            raw: json!({
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "tu_1", "name": "Read", "input": {}},
                        {"type": "tool_use", "id": "tu_2", "name": "Bash", "input": {}}
                    ]
                }
            }),
        },
        // Only tu_1 got a result before cancellation.
        rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-tr".into(),
            parent_uuid: Some("a1".into()),
            timestamp: None,
            raw: json!({
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "tu_1", "content": "ok"}
                    ]
                }
            }),
        },
    ];
    let messages = transcript_to_api_messages(&entries);
    assert_eq!(messages.len(), 3);
    // The user message should now have both tool_results.
    let user = &messages[2];
    assert_eq!(user.content.len(), 2);
    match &user.content[0] {
        rebon_api::ContentBlock::ToolResult(tr) => assert_eq!(tr.tool_use_id, "tu_1"),
        other => panic!("expected tool_result for tu_1, got {other:?}"),
    }
    match &user.content[1] {
        rebon_api::ContentBlock::ToolResult(tr) => {
            assert_eq!(tr.tool_use_id, "tu_2");
            assert!(tr.is_error);
        }
        other => panic!("expected synthetic tool_result for tu_2, got {other:?}"),
    }
}

#[test]
fn ensure_tool_result_pairing_noop_when_paired() {
    // Normal case: tool_use has matching tool_result.
    let entries = vec![
        rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "a1".into(),
            parent_uuid: None,
            timestamp: None,
            raw: json!({
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "tu_ok", "name": "Read", "input": {}}
                    ]
                }
            }),
        },
        rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-tr".into(),
            parent_uuid: Some("a1".into()),
            timestamp: None,
            raw: json!({
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "tu_ok", "content": "data"}
                    ]
                }
            }),
        },
    ];
    let messages = transcript_to_api_messages(&entries);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].content.len(), 1);
    match &messages[1].content[0] {
        rebon_api::ContentBlock::ToolResult(tr) => {
            assert_eq!(tr.tool_use_id, "tu_ok");
            assert!(!tr.is_error);
        }
        other => panic!("expected original tool_result, got {other:?}"),
    }
}
// -------------------------------------------------------------------
// Multi-turn conversation history tests
// -------------------------------------------------------------------

#[tokio::test]
async fn execute_pushes_entries_to_loaded_transcript_after_turn() {
    // A single text-only turn should push both the user prompt
    // and the assistant response into loaded_transcript.
    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "hello from model"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("history_push_single");
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
            text: "hello".into(),
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
    // At minimum: 1 user entry + 1 assistant entry
    assert!(
        record.loaded_transcript.len() >= 2,
        "expected at least 2 entries, got {}",
        record.loaded_transcript.len()
    );
    assert_eq!(record.loaded_transcript[0].entry_type, "user");
    assert_eq!(
        record.loaded_transcript.last().unwrap().entry_type,
        "assistant"
    );
}

#[tokio::test]
async fn executor_polls_with_exact_session_and_turn_context() {
    #[derive(Default)]
    struct ContextRecordingPoller {
        calls: Mutex<Vec<(String, String, u64, AttachmentPollPhase)>>,
    }

    impl crate::query::AttachmentPoller for ContextRecordingPoller {
        fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
            self.calls.lock().unwrap().push((
                request.session_id.to_string(),
                request.turn_id.to_string(),
                request.next_iteration,
                request.phase,
            ));
            Vec::new()
        }
    }

    let engine = build_engine_with(Arc::new(RecordingTool::new("Bash", json!({"ok": true}))));
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_context", "done"));
    let projects_root_dir = temp_projects_root("attachment_poller_exact_context");
    let cwd = projects_root_dir.path().to_string_lossy().to_string();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let session = state.create_session(cwd.clone(), Vec::new());
    let poller = Arc::new(ContextRecordingPoller::default());
    let executor = EngineQueryExecutor::new(
        engine,
        Arc::new(client),
        projects_root_dir.path(),
        "mock-model",
    )
    .with_server_state(state)
    .with_extra_attachment_poller(poller.clone());
    let user_message_uuid = "u-context";

    executor
        .execute(mobile_prompt_request(
            &session.id,
            &cwd,
            "verify context",
            user_message_uuid,
        ))
        .await
        .unwrap();

    let turn_id = format!("{}:{user_message_uuid}", session.id);
    assert_eq!(
        *poller.calls.lock().unwrap(),
        vec![
            (
                session.id.clone(),
                turn_id.clone(),
                0,
                AttachmentPollPhase::Eager,
            ),
            (session.id, turn_id, 1, AttachmentPollPhase::Eager),
        ]
    );
}

#[tokio::test]
async fn completed_mobile_prompt_replay_is_a_durable_no_op() {
    #[derive(Default)]
    struct ReplayNoopPoller {
        poll_calls: AtomicUsize,
        finish_calls: AtomicUsize,
    }

    impl crate::query::AttachmentPoller for ReplayNoopPoller {
        fn poll(&self, _request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
            self.poll_calls.fetch_add(1, Ordering::SeqCst);
            vec![ApiMessage::user_text("must remain pending")]
        }

        fn finish_turn(&self, _succeeded: bool) {
            self.finish_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);
    let projects_root_dir = temp_projects_root("mobile_prompt_durable_noop");
    let projects_root = projects_root_dir.path();
    let cwd = projects_root.to_string_lossy().to_string();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let session = state.create_session(cwd.clone(), Vec::new());
    let uuid = "u-mobile-command";
    write_transcript_fixture(
        projects_root,
        &cwd,
        &session.id,
        vec![
            make_user_entry(uuid, "already handled"),
            make_assistant_entry("a-mobile-answer", uuid, "done"),
        ],
    );
    let path = rebon_session::transcript_file_path(projects_root, &cwd, &session.id);
    let before = rebon_session::load_raw_transcript_from_file(&path)
        .unwrap()
        .unwrap()
        .parsed_row_count;
    let poller = Arc::new(ReplayNoopPoller::default());
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state)
        .with_extra_attachment_poller(poller.clone());

    let outcome = executor
        .execute(mobile_prompt_request(
            &session.id,
            &cwd,
            "already handled",
            uuid,
        ))
        .await
        .unwrap();

    assert_eq!(outcome, PromptOutcome::end_turn());
    assert!(client_handle.captured_requests().is_empty());
    assert_eq!(poller.poll_calls.load(Ordering::SeqCst), 0);
    assert_eq!(poller.finish_calls.load(Ordering::SeqCst), 0);
    let raw = rebon_session::load_raw_transcript_from_file(&path)
        .unwrap()
        .unwrap();
    assert_eq!(raw.parsed_row_count, before);
    assert_eq!(
        raw.entries
            .iter()
            .filter(|entry| entry.uuid == uuid)
            .count(),
        1
    );
}

#[tokio::test]
async fn incomplete_mobile_prompt_replay_resumes_without_duplicate_user_row() {
    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_mobile_resume", "resumed"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);
    let projects_root_dir = temp_projects_root("mobile_prompt_resume");
    let projects_root = projects_root_dir.path();
    let cwd = projects_root.to_string_lossy().to_string();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let session_id = "sess-mobile-incomplete";
    let uuid = "u-mobile-incomplete";
    write_transcript_fixture(
        projects_root,
        &cwd,
        session_id,
        vec![make_user_entry(uuid, "resume me")],
    );
    let session = state
        .load_session(projects_root, session_id, &cwd, None, Vec::new())
        .unwrap();
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state);

    executor
        .execute(mobile_prompt_request(&session.id, &cwd, "resume me", uuid))
        .await
        .unwrap();

    assert_eq!(client_handle.captured_requests().len(), 1);
    let path = rebon_session::transcript_file_path(projects_root, &cwd, &session.id);
    let raw = rebon_session::load_raw_transcript_from_file(&path)
        .unwrap()
        .unwrap();
    assert_eq!(
        raw.entries
            .iter()
            .filter(|entry| entry.uuid == uuid)
            .count(),
        1
    );
    assert_eq!(
        raw.entries.last().unwrap().parent_uuid.as_deref(),
        Some(uuid)
    );
}

#[tokio::test]
async fn mobile_prompt_uuid_reuse_with_different_content_is_rejected() {
    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);
    let projects_root_dir = temp_projects_root("mobile_prompt_conflict");
    let projects_root = projects_root_dir.path();
    let cwd = projects_root.to_string_lossy().to_string();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let session = state.create_session(cwd.clone(), Vec::new());
    let uuid = "u-mobile-conflict";
    write_transcript_fixture(
        projects_root,
        &cwd,
        &session.id,
        vec![make_user_entry(uuid, "original")],
    );
    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state);

    let error = executor
        .execute(mobile_prompt_request(&session.id, &cwd, "different", uuid))
        .await
        .unwrap_err();

    assert!(error
        .to_string()
        .contains("already used for different content"));
    assert!(client_handle.captured_requests().is_empty());
    let path = rebon_session::transcript_file_path(projects_root, &cwd, &session.id);
    let raw = rebon_session::load_raw_transcript_from_file(&path)
        .unwrap()
        .unwrap();
    assert_eq!(
        raw.entries
            .iter()
            .filter(|entry| entry.uuid == uuid)
            .count(),
        1
    );
}

#[tokio::test]
async fn empty_prompt_resumes_from_saved_user_tail_without_duplicate_user() {
    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "continued from saved prompt"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("history_resume_only");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());
    assert!(state.push_transcript_entries(
        &session.id,
        vec![make_user_entry("u-active", "already saved prompt")]
    ));

    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state.clone());

    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: String::new(),
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

    let captured = client_handle.captured_requests();
    assert_eq!(captured.len(), 1);
    let user_messages = captured[0]
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .collect::<Vec<_>>();
    assert_eq!(user_messages.len(), 1);
    assert_eq!(
        user_messages[0].content[0].as_text(),
        Some("already saved prompt")
    );

    let record = state.get_session(&session.id).unwrap();
    let user_entry_count = record
        .loaded_transcript
        .iter()
        .filter(|entry| entry.entry_type == "user")
        .count();
    // The previously loaded user row was moved into the engine window. ACP
    // retains only rows produced after that handoff.
    assert_eq!(user_entry_count, 0);
    let assistant = record.loaded_transcript.last().unwrap();
    assert_eq!(assistant.entry_type, "assistant");
    assert_eq!(assistant.parent_uuid.as_deref(), Some("u-active"));
}

#[tokio::test]
async fn empty_prompt_resumes_from_durable_interrupted_tool_result_tail() {
    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "continued after interrupted tool"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("history_resume_interrupted_tool");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());
    let user = make_user_entry("u-interrupted", "already saved prompt");
    let assistant_tool_use = rebon_session::TranscriptEntry {
        entry_type: "assistant".into(),
        uuid: "a-interrupted".into(),
        parent_uuid: Some("u-interrupted".into()),
        timestamp: Some("2026-04-09T00:01:00.000Z".into()),
        raw: json!({
            "type": "assistant",
            "uuid": "a-interrupted",
            "parentUuid": "u-interrupted",
            "timestamp": "2026-04-09T00:01:00.000Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "tool-interrupted", "name": "Read", "input": {}}],
                "stop_reason": "tool_use"
            }
        }),
    };
    let repaired_tool_result = rebon_session::TranscriptEntry {
        entry_type: "user".into(),
        uuid: "u-interrupted-result".into(),
        parent_uuid: Some("a-interrupted".into()),
        timestamp: Some("2026-04-09T00:02:00.000Z".into()),
        raw: json!({
            "type": "user",
            "uuid": "u-interrupted-result",
            "parentUuid": "a-interrupted",
            "timestamp": "2026-04-09T00:02:00.000Z",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tool-interrupted",
                    "content": "[Tool result missing — the `Read` call was interrupted]",
                    "is_error": true
                }]
            }
        }),
    };
    assert!(state.push_transcript_entries(
        &session.id,
        vec![user, assistant_tool_use, repaired_tool_result]
    ));

    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state.clone());
    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd,
        prompt: Vec::new(),
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

    let captured = client_handle.captured_requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].messages.last().unwrap().role, Role::User);
    assert!(matches!(
        &captured[0].messages.last().unwrap().content[0],
        ApiContentBlock::ToolResult(result)
            if result.tool_use_id == "tool-interrupted" && result.is_error
    ));
    let record = state.get_session(&session.id).unwrap();
    assert_eq!(
        record
            .loaded_transcript
            .last()
            .unwrap()
            .parent_uuid
            .as_deref(),
        Some("u-interrupted-result")
    );
}

#[tokio::test]
async fn two_turn_sequence_replays_full_history_while_retaining_only_pending_raw() {
    // Two sequential execute() calls persist the complete JSONL history, while
    // ACP retains only the latest pending suffix between engine ingestions.
    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    // Turn 1: simple text response
    client.push_turn(text_turn("msg_1", "response to turn 1"));
    // Turn 2: simple text response
    client.push_turn(text_turn("msg_2", "response to turn 2"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("history_two_turns");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());

    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state.clone());

    // Turn 1
    let cancel1 = CancelToken::new();
    let request1 = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "first question".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: None,
        permission_publisher: None,
        cancel: cancel1,
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
    executor.execute(request1).await.unwrap();

    let after_turn1 = state.get_session(&session.id).unwrap();
    let turn1_count = after_turn1.loaded_transcript.len();
    assert!(turn1_count >= 2, "turn 1 should produce at least 2 entries");

    // Turn 2
    let cancel2 = CancelToken::new();
    let request2 = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "second question".into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: None,
        permission_publisher: None,
        cancel: cancel2,
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
    executor.execute(request2).await.unwrap();

    let after_turn2 = state.get_session(&session.id).unwrap();
    assert!(
        after_turn2.loaded_transcript.len() >= 2,
        "ACP should retain the latest pending turn suffix"
    );
    assert!(
        after_turn2.loaded_transcript.len() <= turn1_count.saturating_add(1),
        "resident raw history must not grow with total session length"
    );
    let transcript_path = rebon_session::transcript_file_path(projects_root, &cwd, &session.id);
    let persisted = rebon_session::load_transcript_from_file(&transcript_path)
        .unwrap()
        .expect("complete persisted transcript");
    assert!(
        persisted.messages.len() >= 4,
        "disk remains the complete source of truth"
    );
}
