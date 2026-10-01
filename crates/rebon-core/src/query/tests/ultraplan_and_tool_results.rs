use super::*;

/// A provider that fails deterministically before producing output (e.g. a
/// relay with no channel for the requested model) must NOT be replayed in an
/// unbounded tight loop — that hammering is what got users IP-rate-limited
/// (HTTP 429) relay-side. The replay budget caps consecutive transient
/// failures and then surfaces a real error. Paused tokio time makes the
/// inter-retry backoff sleeps instant.
#[tokio::test(start_paused = true)]
async fn transient_stream_failures_give_up_after_replay_budget() {
    let engine =
        build_engine_with(Arc::new(RecordingTool::new("Read", json!({}))) as Arc<dyn Tool>);
    let mock = MockModelClient::new();
    for _ in 0..=MAX_TRANSIENT_REPLAYS {
        mock.push_error(ModelError::Http(
            "responses stream ended before response.completed".into(),
        ));
    }
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());
    let params =
        QueryParams::new("mock", vec![ApiMessage::user_text("hello")]).with_max_iterations(64);

    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let Some(QueryEvent::Error(message)) = events.last() else {
        panic!("expected terminal error event, got {:?}", events.last());
    };
    assert!(
        message.contains("times in a row"),
        "unexpected terminal error: {message}"
    );
    // Initial attempt + MAX_TRANSIENT_REPLAYS replays, then give up — not
    // one request per remaining iteration.
    assert_eq!(
        mock.captured_requests().len(),
        (MAX_TRANSIENT_REPLAYS + 1) as usize
    );
}

#[test]
fn sync_ultraplan_run_handle_replaces_cached_state_with_authoritative_disk_head() {
    let projects_root_dir = temp_projects_root("ultraplan_run_merge");
    let projects_root = projects_root_dir.path();
    let cwd = projects_root.to_string_lossy().to_string();
    let run_id = "run-merge";
    let mut engine_state =
        UltraplanRunState::new(run_id.into(), "session".into(), "task".into(), None, 10);
    engine_state
        .requirement_ledger
        .push(rebon_types::RequirementLedgerEntry {
            id: "R1".into(),
            title: "engine ledger".into(),
            source: RequirementSource::Question,
            round_added: 1,
        });
    let mut disk_state =
        UltraplanRunState::new(run_id.into(), "session".into(), "task".into(), None, 20);
    disk_state.phase = RunPhase::Executing;
    disk_state.round = 2;
    disk_state.last_plan_draft = Some("approved plan".into());
    disk_state.reviewer_verdicts.push(ReviewerVerdictRecord {
        round: 2,
        verdict: "USER_REJECTED".into(),
        blocking_gaps: 1,
        source: VerdictSource::UserRejection,
    });
    rebon_session::save_ultraplan_run(projects_root, &cwd, &disk_state).unwrap();
    let handle = Arc::new(Mutex::new(engine_state));

    assert!(sync_ultraplan_run_handle_from_disk(
        projects_root,
        &cwd,
        &handle
    ));
    let loaded = handle.lock().unwrap().clone();

    assert_eq!(loaded.phase, RunPhase::Executing);
    assert_eq!(loaded.round, 2);
    assert_eq!(loaded.last_plan_draft.as_deref(), Some("approved plan"));
    assert_eq!(loaded.reviewer_verdicts.len(), 1);
    assert!(loaded.requirement_ledger.is_empty());
    assert!(loaded.manifest.is_none());
}

#[test]
fn sync_ultraplan_run_handle_fails_closed_for_missing_or_mismatched_grill_state() {
    let projects_root_dir = temp_projects_root("ultraplan_grill_sync_fail_closed");
    let projects_root = projects_root_dir.path();
    let cwd = projects_root.to_string_lossy().to_string();
    let run_id = "run-grill-sync";
    let grill_state =
        UltraplanRunState::new(run_id.into(), "session-a".into(), "task".into(), None, 10)
            .with_profile(UltraplanProfile::Grill);
    let handle = Arc::new(Mutex::new(grill_state.clone()));

    assert!(!sync_ultraplan_run_handle_from_disk(
        projects_root,
        &cwd,
        &handle
    ));

    let mismatched =
        UltraplanRunState::new(run_id.into(), "session-b".into(), "task".into(), None, 20)
            .with_profile(UltraplanProfile::Grill);
    rebon_session::save_ultraplan_run(projects_root, &cwd, &mismatched).unwrap();

    assert!(!sync_ultraplan_run_handle_from_disk(
        projects_root,
        &cwd,
        &handle
    ));
    assert_eq!(*handle.lock().unwrap(), grill_state);
}

#[test]
fn sync_ultraplan_run_handle_rejects_missing_standard_state() {
    let projects_root_dir = temp_projects_root("ultraplan_standard_sync_missing");
    let projects_root = projects_root_dir.path();
    let cwd = projects_root.to_string_lossy().to_string();
    let handle = Arc::new(Mutex::new(UltraplanRunState::new(
        "run-standard".into(),
        "session".into(),
        "task".into(),
        None,
        10,
    )));

    assert!(!sync_ultraplan_run_handle_from_disk(
        projects_root,
        &cwd,
        &handle
    ));
}

#[tokio::test]
async fn plan_ledger_uses_newer_disk_round_and_preserves_non_material_runner_updates() {
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_runner_update",
        "Read",
        "toolu_runner_update",
        r#"{"file_path":"marker"}"#,
    ));
    client.push_turn(tool_turn(
        "msg_plan_ledger",
        "PlanLedger",
        "toolu_plan_ledger",
        r#"{"operation":"set_requirements","expected_revision":1,"items":[{"id":"R1","title":"first"}]}"#,
    ));
    client.push_turn(text_turn("msg_done", "done"));
    let client: Arc<dyn ModelClient> = Arc::new(client);
    let projects_root_dir = temp_projects_root("ultraplan_ledger_handle");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());
    let run_id = "run-ledger";
    let run_state =
        UltraplanRunState::new(run_id.into(), session.id.clone(), "task".into(), None, 1);
    rebon_session::save_ultraplan_run(projects_root, &cwd, &run_state).unwrap();
    let mut engine = Engine::new().with_permission_broker(Arc::new(AllowAllBroker));
    engine.register_tool(Arc::new(UltraplanDiskUpdateTool {
        projects_root: projects_root.to_path_buf(),
        cwd: cwd.clone(),
        run_id: run_id.into(),
    }));
    engine.register_tool(Arc::new(rebon_plugin_plan_mode::PlanLedgerTool));
    let engine = Arc::new(engine);

    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state.clone());
    let outcome =
        executor
            .execute(rebon_agent_core::PromptRequest {
                user_prompt: None,
                effort_is_session_default: false,
                session_id: session.id.clone(),
                cwd: cwd.clone(),
                prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: "plan".into(),
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
                    UltraplanContext::planning_turn(run_id, "plan", PolicyMode::Enforce),
                )),
                replay_requests: Vec::new(),
                skill_invocations: Vec::new(),
            })
            .await;

    assert!(outcome.is_ok(), "execute failed: {:?}", outcome.err());
    let persisted = rebon_session::load_ultraplan_run(projects_root, &cwd, run_id).unwrap();
    assert_eq!(persisted.phase, RunPhase::Executing);
    assert_eq!(persisted.round, 2);
    assert!(persisted.last_plan_draft.is_none());
    assert_eq!(persisted.reviewer_verdicts.len(), 1);
    assert_eq!(persisted.requirement_ledger.len(), 1);
    assert_eq!(persisted.requirement_ledger[0].id, "R1");
    assert_eq!(persisted.requirement_ledger[0].round_added, 2);
    assert_eq!(
        persisted.requirement_ledger[0].source,
        RequirementSource::Question
    );
}

#[test]
fn ask_user_question_tool_result_uses_specialized_sentence() {
    let value = json!({
        "answers": {
            "Which database should we use?": "Postgres"
        },
        "annotations": null,
        "questions": []
    });

    let content = compact_tool_result_for_model("AskUserQuestion", None, &value, None);

    assert_eq!(
        content,
        "User has answered your questions: \"Which database should we use?\"=\"Postgres\". You can now continue with the user's answers in mind."
    );
    assert!(!content.contains("{\"answers\""));
}

/// A deferred question's result tells the model the answer is still coming,
/// instead of announcing an empty set of answers.
#[test]
fn a_pending_ask_user_question_result_is_the_pending_note() {
    let value = crate::deferred_question::pending_result("toolu_9");
    let content = compact_tool_result_for_model("AskUserQuestion", None, &value, None);
    let text = content.to_plain_text();
    assert!(crate::deferred_question::is_pending_model_text(&text));
    assert!(text.contains("<question-answer tool_use_id=\"toolu_9\">"));
    assert!(!text.contains("User has answered"));
    assert!(!text.contains("\"status\""));
}

#[test]
fn ask_user_question_tool_result_includes_preview_and_notes() {
    let value = json!({
        "answers": {
            "Which UI should we build?": "Cards"
        },
        "annotations": {
            "Which UI should we build?": {
                "preview": "<div>Cards mockup</div>",
                "notes": "Prefer dense layout"
            }
        }
    });

    let content = compact_tool_result_for_model("AskUserQuestion", None, &value, None);

    assert_eq!(
        content,
        "User has answered your questions: \"Which UI should we build?\"=\"Cards\" selected preview:\n<div>Cards mockup</div> user notes: Prefer dense layout. You can now continue with the user's answers in mind."
    );
}

#[test]
fn ask_user_question_tool_result_handles_missing_and_empty_annotations() {
    let missing_annotations = json!({
        "answers": {
            "Pick a runtime?": "Tokio"
        }
    });
    let empty_annotations = json!({
        "answers": {
            "Pick a runtime?": "Tokio"
        },
        "annotations": {}
    });

    let expected = "User has answered your questions: \"Pick a runtime?\"=\"Tokio\". You can now continue with the user's answers in mind.";
    assert_eq!(
        compact_tool_result_for_model("AskUserQuestion", None, &missing_annotations, None),
        expected
    );
    assert_eq!(
        compact_tool_result_for_model("AskUserQuestion", None, &empty_annotations, None),
        expected
    );
}

#[test]
fn generic_tool_result_formatting_remains_json() {
    let value = json!({
        "answers": {
            "Which database should we use?": "Postgres"
        }
    });

    assert_eq!(
        compact_tool_result_for_model("SomeOtherTool", None, &value, None),
        "{\"answers\":{\"Which database should we use?\":\"Postgres\"}}"
    );
}

#[tokio::test]
async fn manual_compact_call_path_preserves_current_plain_user_tail() {
    let engine =
        build_engine_with(Arc::new(RecordingTool::new("Read", json!({}))) as Arc<dyn Tool>);
    let mock = MockModelClient::new();
    // Two turns, because compaction now costs a request of its own: with no
    // compact provider configured the ladder ends at the session's own client,
    // which is this mock. The first turn answers the summary request, the
    // second is the turn under test.
    mock.push_turn(text_turn("msg_0", "a summary of the old messages"));
    mock.push_turn(text_turn("msg_1", "after compact"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());
    let handle = PruneLevelHandle::with_context_window(PruneLevel::Conservative, 120_000);
    handle.budget.force_compact_once();
    let params = QueryParams {
        prune_level: Some(handle),
        max_iterations: 2,
        ..QueryParams::new(
            "mock",
            vec![
                ApiMessage::user_text("old-1"),
                ApiMessage::assistant_text("reply-1"),
                ApiMessage::user_text("old-2"),
                ApiMessage::assistant_text("reply-2"),
                ApiMessage::user_text("old-3"),
                ApiMessage::assistant_text("reply-3"),
                ApiMessage::user_text("old-4"),
                ApiMessage::assistant_text("reply-4"),
                ApiMessage::user_text("old-5"),
                ApiMessage::assistant_text("reply-5"),
                ApiMessage::user_text("current tail"),
            ],
        )
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
    let captured = mock.captured_requests();
    // Two requests now: the summary, then the turn. The turn is the one that
    // has to carry the tail.
    assert_eq!(captured.len(), 2);
    let turn = captured.last().expect("a turn request");
    assert!(turn.messages.len() <= 11);
    let texts = text_message_texts(&turn.messages);
    assert!(texts
        .last()
        .is_some_and(|text| text.contains("current tail")));
    assert!(texts.contains(&"current tail"));
}

#[tokio::test]
async fn auto_pre_turn_compact_call_path_preserves_current_plain_user_tail() {
    let engine =
        build_engine_with(Arc::new(RecordingTool::new("Read", json!({}))) as Arc<dyn Tool>);
    let mock = MockModelClient::new();
    // See the manual-compact test above: the first turn answers the summary
    // request the session-client rung now makes, the second is the real turn.
    mock.push_turn(text_turn("msg_0", "a summary of the old messages"));
    mock.push_turn(text_turn("msg_1", "after auto compact"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());
    let handle = PruneLevelHandle::with_context_window(PruneLevel::Conservative, 120_000);
    handle.report_usage(handle.budget.auto_compact_threshold());
    let params = QueryParams {
        prune_level: Some(handle),
        max_iterations: 1,
        ..QueryParams::new(
            "mock",
            vec![
                ApiMessage::user_text("old-1"),
                ApiMessage::assistant_text("reply-1"),
                ApiMessage::user_text("old-2"),
                ApiMessage::assistant_text("reply-2"),
                ApiMessage::user_text("old-3"),
                ApiMessage::assistant_text("reply-3"),
                ApiMessage::user_text("old-4"),
                ApiMessage::assistant_text("reply-4"),
                ApiMessage::user_text("old-5"),
                ApiMessage::assistant_text("reply-5"),
                ApiMessage::user_text("current tail"),
            ],
        )
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
    let captured = mock.captured_requests();
    // Two requests now: the summary, then the turn. The turn is the one that
    // has to carry the tail.
    assert_eq!(captured.len(), 2);
    let turn = captured.last().expect("a turn request");
    assert!(turn.messages.len() <= 11);
    let texts = text_message_texts(&turn.messages);
    assert!(texts
        .last()
        .is_some_and(|text| text.contains("current tail")));
    assert!(texts.contains(&"current tail"));
}

#[tokio::test]
async fn compact_helper_preserves_user_tail_and_marks_cache_boundary() {
    let mock = MockModelClient::new();
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());
    let mut manager = ContextManager::new(
        None,
        vec![
            ApiMessage::user_text("old"),
            ApiMessage::assistant_text("reply"),
            ApiMessage::user_text("older"),
            ApiMessage::assistant_text("reply 2"),
            ApiMessage::user_text("current tail"),
        ],
    );
    let params = QueryParams::new("mock", Vec::new());

    let session = SessionHandle::new(client);
    let result = compact_with_tail_preservation(
        &mut manager,
        &params,
        &session,
        CompactTrigger::Manual,
        1,
        None,
    )
    .await;

    assert!(result.replaced_messages);
    assert!(result.preserved_user_tail);
    assert!(result.invalidated_previous_response_id);
    assert!(result.new_baseline_required);
    assert_eq!(mock.invalidate_previous_response_id_count(), 1);
    assert_eq!(
        manager.messages().last().unwrap().content[0].as_text(),
        Some("current tail")
    );
}

#[test]
fn mid_turn_compact_guard_only_allows_near_hard_limit() {
    let handle = PruneLevelHandle::with_context_window(PruneLevel::Conservative, 100_000);
    let guard_target = mid_turn_compact_guard_target(&handle, 4_000);

    assert_eq!(guard_target, 88_320);
    assert!(!mid_turn_compact_allowed(&handle, 4_000, guard_target - 1));
    assert!(mid_turn_compact_allowed(&handle, 4_000, guard_target));
}

#[test]
fn mid_turn_compact_guard_honours_the_absolute_auto_compact_cap() {
    let handle = PruneLevelHandle::with_model_context_limits(
        PruneLevel::Conservative,
        1_050_000,
        128_000,
        std::iter::empty::<(String, u32)>(),
        std::iter::empty::<(String, u32)>(),
    );
    assert_eq!(mid_turn_compact_guard_target(&handle, 4_000), 848_240);

    handle
        .budget
        .set_auto_compact_token_limit(Some(rebon_api::DEFAULT_AUTO_COMPACT_TOKEN_LIMIT));
    assert_eq!(mid_turn_compact_guard_target(&handle, 4_000), 244_800);
    assert!(mid_turn_compact_allowed(&handle, 4_000, 244_800));
    assert!(!mid_turn_compact_allowed(&handle, 4_000, 244_799));
}

#[test]
fn configured_output_reserve_sets_five_percent_hard_guard() {
    let handle = PruneLevelHandle::with_model_context_limits(
        PruneLevel::Conservative,
        500_000,
        128_000,
        std::iter::empty::<(String, u32)>(),
        std::iter::empty::<(String, u32)>(),
    );

    assert_eq!(handle.budget.input_token_budget(), 372_000);
    assert_eq!(handle.budget.auto_compact_threshold(), 353_400);
    assert_eq!(
        hard_context_guard_target(Some(&handle), 32_000),
        Some(353_400)
    );
    assert_eq!(budgeted_replay_target(Some(&handle), 32_000), Some(353_400));
}

/// The ChatGPT Codex window as the harness hands it over: 272k of input
/// widened by the 128k output limit, with the session's own `max_tokens`
/// at that limit. Compaction fires first, and the hard guard stays behind
/// it instead of pruning history — and the prompt cache with it — at 80k.
#[test]
fn codex_window_keeps_the_hard_guard_behind_compaction() {
    let handle = PruneLevelHandle::with_model_context_limits(
        PruneLevel::Conservative,
        400_000,
        128_000,
        std::iter::empty::<(String, u32)>(),
        std::iter::empty::<(String, u32)>(),
    );
    handle
        .budget
        .set_auto_compact_token_limit(Some(rebon_api::DEFAULT_AUTO_COMPACT_TOKEN_LIMIT));

    assert_eq!(handle.budget.auto_compact_threshold(), 244_800);
    assert_eq!(
        hard_context_guard_target(Some(&handle), 128_000),
        Some(258_400)
    );
    assert_eq!(
        budgeted_replay_target(Some(&handle), 128_000),
        Some(244_800)
    );
}

#[test]
fn unknown_output_limit_does_not_subtract_a_second_output_reserve() {
    let handle = PruneLevelHandle::with_context_window(PruneLevel::Conservative, 128_000);
    assert_eq!(
        hard_context_guard_target(Some(&handle), 64_000),
        Some(60_800)
    );
    assert_eq!(budgeted_replay_target(Some(&handle), 64_000), Some(60_800));
}

#[test]
fn output_cap_policy_preserves_small_caps_and_only_halves_unknown_limits() {
    for (output_limit, requested, expected) in [
        (0, 128_000, 64_000),
        (0, 4_096, 4_096),
        (16_000, 128_000, 16_000),
        (16_000, 4_096, 4_096),
        (100_000, 128_000, 100_000),
    ] {
        let handle = PruneLevelHandle::with_model_context_limits(
            PruneLevel::Conservative,
            128_000,
            output_limit,
            std::iter::empty::<(String, u32)>(),
            std::iter::empty::<(String, u32)>(),
        );
        assert_eq!(cap_max_tokens_for_model(requested, Some(&handle)), expected);
        assert_eq!(cap_max_tokens_for_model(requested, None), requested);
        assert_eq!(
            hard_context_guard_target(Some(&handle), requested),
            hard_context_guard_target(Some(&handle), expected)
        );
    }
    assert_eq!(hard_context_guard_target(None, 128_000), None);
}

#[tokio::test]
async fn output_cap_policy_clamps_high_and_xhigh_defaults_and_explicit_requests() {
    for output_limit in [0, 16_000] {
        for effort in [
            rebon_types::ReasoningEffort::High,
            rebon_types::ReasoningEffort::XHigh,
        ] {
            let requested = rebon_api::effort::resolve_thinking_from_effort(
                Some(effort),
                rebon_types::effort_indicator::EffortProviderKind::OpenAi,
            )
            .max_tokens
            .unwrap();
            for explicit_request in [false, true] {
                let handle = PruneLevelHandle::with_model_context_limits(
                    PruneLevel::Conservative,
                    128_000,
                    output_limit,
                    std::iter::empty::<(String, u32)>(),
                    std::iter::empty::<(String, u32)>(),
                );
                let expected = if output_limit == 0 {
                    64_000
                } else {
                    output_limit
                };
                let sent = sent_max_tokens(
                    "context-output-cap",
                    "mock-model",
                    |executor| {
                        executor
                            .with_prune_level(handle.clone())
                            .with_max_tokens(if explicit_request { 4_096 } else { requested })
                    },
                    explicit_request.then_some(requested),
                )
                .await;
                assert_eq!(sent, expected);
                assert_eq!(
                    hard_context_guard_target(Some(&handle), sent),
                    Some((128_000 - expected) * 95 / 100)
                );
            }
        }
    }
}

#[tokio::test]
async fn output_cap_policy_keeps_explicit_budgets_without_a_handle() {
    for explicit_request in [false, true] {
        let sent = sent_max_tokens(
            "context-output-cap-no-handle",
            "mock-model",
            |executor| executor.with_max_tokens(128_000),
            explicit_request.then_some(96_000),
        )
        .await;
        assert_eq!(sent, if explicit_request { 96_000 } else { 128_000 });
    }
}

fn hard_guard_history(old_chars: usize, recent_chars: usize) -> Vec<ApiMessage> {
    let mut messages = vec![
        ApiMessage::user_text(format!("old evidence {}", "x".repeat(old_chars))),
        ApiMessage::assistant_text("old answer"),
    ];
    for _ in 0..4 {
        messages.push(ApiMessage::user_text("recent request"));
        messages.push(ApiMessage::assistant_text("y".repeat(recent_chars)));
    }
    messages.push(ApiMessage::user_text("current tail"));
    messages
}

fn hard_guard_params(output_limit: u32, old_chars: usize, recent_chars: usize) -> QueryParams {
    QueryParams {
        max_tokens: 128_000,
        max_iterations: 1,
        prune_level: Some(PruneLevelHandle::with_model_context_limits(
            PruneLevel::Conservative,
            128_000,
            output_limit,
            std::iter::empty::<(String, u32)>(),
            std::iter::empty::<(String, u32)>(),
        )),
        tools: vec![ApiTool {
            name: "Read".into(),
            description: "schema ".repeat(5_000),
            input_schema: json!({"type": "object"}),
        }],
        ..QueryParams::new("mock", hard_guard_history(old_chars, recent_chars))
    }
}

#[tokio::test]
async fn hard_guard_keeps_history_with_large_schema_below_the_input_budget() {
    let mock = MockModelClient::new();
    mock.push_turn(text_turn("turn", "done"));
    let params = hard_guard_params(0, 80_000, 1);
    let original = params.messages.clone();
    let target = hard_context_guard_target(params.prune_level.as_ref(), params.max_tokens).unwrap();
    let manager = ContextManager::new(params.system.clone(), original.clone());
    assert!(next_request_input_estimate(&manager, &params) < target);
    let mut rx = run_query(
        Arc::new(Engine::new()),
        SessionHandle::new(Arc::new(mock.clone())),
        params,
        ToolContext::new(),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;
    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    let captured = mock.captured_requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].max_tokens, 64_000);
    assert_eq!(captured[0].messages, original);
}

#[tokio::test]
async fn pre_turn_hard_guard_summarizes_before_pruning_known_and_unknown_output() {
    let _env = env_lock();
    let _trace_env = EnvVarGuard::set("REBON_CACHE_TRACE", "1");
    for output_limit in [0, 64_000] {
        let mock = MockModelClient::new();
        mock.push_turn(text_turn("summary", "preserved old evidence"));
        mock.push_turn(text_turn("turn", "done"));
        let (seat, observations) = cache_trace_recording_seat();
        let params = hard_guard_params(output_limit, 300_000, 1).with_turn_hook_seat(seat);
        let handle = params.prune_level.as_ref().unwrap();
        let manager = ContextManager::new(params.system.clone(), params.messages.clone());
        let before_tokens = next_request_input_estimate(&manager, &params);
        assert!(
            before_tokens > hard_context_guard_target(Some(handle), params.max_tokens).unwrap()
        );
        if output_limit == 0 {
            assert!(before_tokens < handle.budget.auto_compact_threshold());
        }
        let original_prefix = params.messages[..params.messages.len() - 1].to_vec();
        let mut rx = run_query(
            Arc::new(Engine::new()),
            SessionHandle::new(Arc::new(mock.clone())),
            params,
            ToolContext::new(),
            CancelToken::new(),
        );
        let events = drain(&mut rx).await;
        assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            QueryEvent::CompactingFinished {
                used_model: true,
                ..
            }
        )));
        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 2);
        assert_eq!(
            &captured[0].messages[..original_prefix.len()],
            original_prefix.as_slice()
        );
        let texts = text_message_texts(&captured[1].messages);
        assert!(texts
            .iter()
            .any(|text| text.contains("preserved old evidence")));
        assert_eq!(texts.last(), Some(&"current tail"));
        assert!(observations
            .lock()
            .expect("cache trace observations poisoned")
            .iter()
            .any(|observation| matches!(
                observation,
                CacheTraceObservation::Request {
                    cache_miss_reason: CacheMissReason::CompactReplacedMessages,
                    ..
                }
            )));
    }
}

#[tokio::test]
async fn pre_turn_hard_guard_falls_back_when_summary_fails_or_still_exceeds_budget() {
    let _env = env_lock();
    let _trace_env = EnvVarGuard::set("REBON_CACHE_TRACE", "1");
    for summary_succeeds in [false, true] {
        let mock = MockModelClient::new();
        if summary_succeeds {
            mock.push_turn(text_turn("summary", "preserved old evidence"));
        } else {
            mock.push_error(ModelError::BadRequest("summary unavailable".into()));
        }
        mock.push_turn(text_turn("turn", "done"));
        let (seat, observations) = cache_trace_recording_seat();
        let params = hard_guard_params(0, 300_000, 80_000).with_turn_hook_seat(seat);
        let budget_params = params.clone();
        let original_prefix = params.messages[..params.messages.len() - 1].to_vec();
        let target =
            hard_context_guard_target(params.prune_level.as_ref(), params.max_tokens).unwrap();
        let mut rx = run_query(
            Arc::new(Engine::new()),
            SessionHandle::new(Arc::new(mock.clone())),
            params,
            ToolContext::new(),
            CancelToken::new(),
        );
        let events = drain(&mut rx).await;
        assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, QueryEvent::CompactingStarted { .. }))
                .count(),
            1
        );
        assert!(events.iter().any(|event| matches!(event, QueryEvent::CompactingFinished { used_model, .. } if *used_model == summary_succeeds)));
        let captured = mock.captured_requests();
        assert_eq!(captured.len(), 2);
        assert_eq!(
            &captured[0].messages[..original_prefix.len()],
            original_prefix.as_slice()
        );
        let manager = ContextManager::new(captured[1].system.clone(), captured[1].messages.clone());
        assert!(next_request_input_estimate(&manager, &budget_params) <= target);
        assert_eq!(
            text_message_texts(&captured[1].messages).last(),
            Some(&"current tail")
        );
        assert!(observations
            .lock()
            .expect("cache trace observations poisoned")
            .iter()
            .any(|observation| matches!(
                observation,
                CacheTraceObservation::Request {
                    cache_miss_reason: CacheMissReason::HardContextGuardTruncated,
                    ..
                }
            )));
    }
}

#[test]
fn hard_context_guard_cache_miss_reason_is_specific() {
    assert_eq!(
        CacheMissReason::HardContextGuardTruncated.as_str(),
        "hard_context_guard_truncated"
    );
}

#[test]
fn appends_permission_extra_text_to_text_tool_result() {
    let mut content = ToolResultContent::text("done");
    append_permission_extra_text_to_tool_result(&mut content, Some("only run unit tests"));
    assert!(content.contains("done"));
    assert!(content.contains("only run unit tests"));
}

#[test]
fn appends_permission_extra_text_to_block_tool_result() {
    let mut content = ToolResultContent::blocks(vec![ToolResultContentBlock::Text(TextBlock {
        text: "file content".into(),
    })]);
    append_permission_extra_text_to_tool_result(&mut content, Some("inspect this image"));
    assert!(content.contains("file content"));
    assert!(content.contains("inspect this image"));
}
