use super::*;

#[test]
fn context_reset_releases_request_policy_and_restores_base_tools() {
    let engine = build_engine_with_tools(vec![
        Arc::new(RecordingTool::new("Read", json!({}))) as Arc<dyn Tool>,
        Arc::new(RecordingTool::new("Write", json!({}))) as Arc<dyn Tool>,
    ]);
    let policy = ExecutionPolicy::ultraplan(rebon_types::UltraplanContext::planning_turn(
        "run-1",
        "plan_mode_active",
        rebon_types::PolicyMode::Enforce,
    ));
    let effective_tool_filter = combine_filters(None, Some(&policy)).expect("planning filter");
    let planning_index = tool_search_index_for_filter(
        &engine,
        true,
        Some(&effective_tool_filter),
        Some(&policy),
        &[],
    );
    let planning_tools = tools_for_filter_and_index(
        &engine,
        Some(&effective_tool_filter),
        Some(&policy),
        &planning_index,
    );
    let mut params = QueryParams {
        model: "mock-model".into(),
        system: Some("planning-system".into()),
        runtime_context_message: None,
        transient_context_message: None,
        messages: Vec::new(),
        tools: planning_tools,
        max_tokens: 4096,
        max_iterations: 1,
        capability_mode: AgentCapabilityMode::Normal,
        anchored_minimal_promotion: None,
        attachment_poller: None,
        turn_hooks: crate::turn_hook::TurnHooks::default(),
        next_tool_choice: None,
        extensions: rebon_tool::Extensions::default(),
        thinking: None,
        reasoning_effort: None,
        reasoning_mode: None,
        web_search: None,
        context_management: None,
        prune_level: None,
        compact_provider: None,
        compact_fallback_provider: None,
        compact_custom_instructions: None,
        compact_summary_options: rebon_api::CompactSummaryOptions::default(),
        file_state_cache: None,
        execution_policy: Some(policy.clone()),
        invariant_execution_policy: None,
        base_tool_filter: None,
        effective_tool_filter: Some(effective_tool_filter),
        post_context_reset_system: Some("base-system".into()),
        post_context_reset_runtime_context_message: None,
        post_context_reset_transient_context_message: None,
        policy: crate::policy_seat::PolicySources::default(),
        cache_trace_context: None,
        mcp_tool_definitions: Vec::new(),
    };
    let mut context = ToolContext::new().with_execution_policy(policy);

    release_request_policy_after_context_reset(&engine, &mut params, &mut context);

    let restored_tools: Vec<_> = params.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(params.execution_policy.is_none());
    assert!(params.effective_tool_filter.is_none());
    assert!(context.execution_policy().is_none());
    assert_eq!(params.system.as_deref(), Some("base-system"));
    assert!(restored_tools.contains(&"Read"));
    assert!(restored_tools.contains(&"Write"));
}

#[test]
fn context_reset_preserves_invariant_eager_promotion() {
    let engine = build_engine_with_tools(vec![
        Arc::new(RecordingTool::new("Read", json!({}))),
        Arc::new(RecordingTool::deferred("StructuredOutput", json!({}))),
    ]);
    let invariant = ExecutionPolicy::default().with_eager_promotions(["StructuredOutput"]);
    let mut params = QueryParams::new("mock-model", Vec::new());
    params.invariant_execution_policy = Some(invariant.clone());
    params.tools = vec![];
    let mut context = ToolContext::new().with_execution_policy(invariant);

    release_request_policy_after_context_reset(&engine, &mut params, &mut context);

    let restored_tools: Vec<_> = params.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(restored_tools.contains(&"StructuredOutput"));
    assert!(context
        .execution_policy()
        .is_some_and(|policy| policy.eager_promotions.contains("StructuredOutput")));
}

#[test]
fn context_reset_post_prompt_and_runtime_drop_request_scoped_eager_promotion() {
    let engine = build_engine_with_tools(vec![
        Arc::new(RecordingTool::new("Read", json!({}))),
        Arc::new(RecordingTool::deferred("StructuredOutput", json!({}))),
    ]);
    let request_policy = ExecutionPolicy::default().with_eager_promotions(["StructuredOutput"]);
    let mut params = QueryParams::new("mock-model", Vec::new());
    params.execution_policy = Some(request_policy.clone());
    params.effective_tool_filter = combine_filters(None, params.execution_policy.as_ref());
    let post_reset_policy = params.invariant_execution_policy.clone();
    let post_reset_filter = combine_filters(None, post_reset_policy.as_ref());
    let post_projection = runtime_tool_projection(
        &engine,
        true,
        post_reset_filter.as_ref(),
        post_reset_policy.as_ref(),
        &engine.deferred_tool_names(),
        &[],
        None,
    );
    params.post_context_reset_system = Some(format!(
        "visible: {} deferred: {}",
        post_projection.provider_visible_tool_names().join(","),
        post_projection.deferred_tool_names.join(",")
    ));
    let mut context = ToolContext::new().with_execution_policy(request_policy);

    release_request_policy_after_context_reset(&engine, &mut params, &mut context);

    let restored_tools: Vec<_> = params.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(!restored_tools.contains(&"StructuredOutput"));
    assert!(params
        .system
        .as_deref()
        .unwrap()
        .contains("deferred: StructuredOutput"));
    assert!(!params
        .system
        .as_deref()
        .unwrap()
        .contains("visible: StructuredOutput"));
    assert!(context.execution_policy().is_none());
}

#[test]
fn context_reset_post_prompt_and_runtime_preserve_invariant_structured_output() {
    let engine = build_engine_with_tools(vec![
        Arc::new(RecordingTool::new("Read", json!({}))),
        Arc::new(RecordingTool::deferred("StructuredOutput", json!({}))),
    ]);
    let invariant = ExecutionPolicy::default().with_eager_promotions(["StructuredOutput"]);
    let mut params = QueryParams::new("mock-model", Vec::new());
    params.invariant_execution_policy = Some(invariant.clone());
    let post_reset_policy = params.invariant_execution_policy.clone();
    let post_reset_filter = combine_filters(None, post_reset_policy.as_ref());
    let post_projection = runtime_tool_projection(
        &engine,
        true,
        post_reset_filter.as_ref(),
        post_reset_policy.as_ref(),
        &engine.deferred_tool_names(),
        &[],
        None,
    );
    params.post_context_reset_system = Some(format!(
        "visible: {} deferred: {}",
        post_projection.provider_visible_tool_names().join(","),
        post_projection.deferred_tool_names.join(",")
    ));
    let mut context = ToolContext::new().with_execution_policy(invariant);

    release_request_policy_after_context_reset(&engine, &mut params, &mut context);

    let restored_tools: Vec<_> = params.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(restored_tools.contains(&"StructuredOutput"));
    assert!(params
        .system
        .as_deref()
        .unwrap()
        .contains("visible: Read,StructuredOutput"));
    assert!(params.system.as_deref().unwrap().contains("deferred: "));
    assert!(context
        .execution_policy()
        .is_some_and(|policy| policy.eager_promotions.contains("StructuredOutput")));
}

#[test]
fn parallel_agent_write_batch_requires_multiple_potential_writers() {
    let implementation_a = ToolUseBlock {
        id: "agent-a".into(),
        name: rebon_tool::AGENT_TOOL_NAME.into(),
        input: json!({ "prompt": "implement a", "task_kind": "implementation" }),
    };
    let implementation_b = ToolUseBlock {
        id: "agent-b".into(),
        name: rebon_tool::AGENT_TOOL_NAME.into(),
        input: json!({ "prompt": "implement b" }),
    };
    let explore_a = ToolUseBlock {
        id: "explore-a".into(),
        name: rebon_tool::AGENT_TOOL_NAME.into(),
        input: json!({ "prompt": "inspect a", "subagent_type": "Explore" }),
    };
    let explore_b = ToolUseBlock {
        id: "explore-b".into(),
        name: rebon_tool::AGENT_TOOL_NAME.into(),
        input: json!({ "prompt": "inspect b", "task_kind": "research" }),
    };

    assert!(super::turn_control::has_parallel_agent_write_batch(&[
        &implementation_a,
        &implementation_b,
    ]));
    assert!(!super::turn_control::has_parallel_agent_write_batch(&[
        &implementation_a,
    ]));
    assert!(!super::turn_control::has_parallel_agent_write_batch(&[
        &explore_a, &explore_b,
    ]));
    assert!(!super::turn_control::has_parallel_agent_write_batch(&[
        &implementation_a,
        &explore_a,
    ]));
}

#[tokio::test]
async fn cache_trace_disabled_emits_no_subscriber_observations() {
    let _env = env_lock();
    let _trace = EnvVarGuard::set_absent("REBON_CACHE_TRACE");
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_done", "done"));
    let (seat, observations) = cache_trace_recording_seat();
    let params =
        QueryParams::new("mock", vec![ApiMessage::user_text("hello")]).with_turn_hook_seat(seat);

    let mut rx = run_query(
        build_engine_with_tools(Vec::new()),
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert!(observations
        .lock()
        .expect("cache trace observations poisoned")
        .is_empty());
}

#[tokio::test]
async fn cache_trace_subscriber_observes_first_and_tool_followup_requests_with_usage() {
    let _env = env_lock();
    let _trace_env = EnvVarGuard::set("REBON_CACHE_TRACE", "1");
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "file body"})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_cache",
        "{\"path\":\"a.rs\"}",
    ));
    client.push_turn(text_turn("msg_done", "done"));
    let trace = CacheTraceContext {
        context_policy: Some("stable-prefix".into()),
        prompt_cache_key: Some("session-key".into()),
        prompt_cache_retention: Some("session".into()),
        api_path: Some("/responses".into()),
        previous_response_id_present: Some(true),
        tools_hash: Some("tools".into()),
        schema_hash: Some("schema".into()),
        shared_preamble_hash: Some("shared".into()),
        profile_preamble_hash: Some("profile".into()),
        capsule_hash: Some("capsule".into()),
        task_hash: Some("task".into()),
        tokens_before_capsule: Some(11),
        tokens_before_task: Some(22),
        cross_run_cache_eligible: Some(true),
        same_dispatch_cache_eligible: Some(false),
    };
    let (seat, observations) = cache_trace_recording_seat();
    let mut params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine))
        .with_turn_hook_seat(seat)
        .with_max_iterations(3);
    params.runtime_context_message = Some("runtime".into());
    params.transient_context_message = Some("transient".into());
    params.cache_trace_context = Some(trace.clone());

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert_eq!(
        *observations
            .lock()
            .expect("cache trace observations poisoned"),
        [
            CacheTraceObservation::Request {
                model: "mock".into(),
                message_count: 2,
                runtime_context_message: Some("runtime".into()),
                transient_context_message: Some("transient".into()),
                cache_miss_reason: CacheMissReason::None,
                cache_trace_context: Some(trace.clone()),
            },
            CacheTraceObservation::Usage {
                model: "mock".into(),
                input_tokens: 4,
                output_tokens: 6,
                cache_trace_context: Some(trace.clone()),
            },
            CacheTraceObservation::Request {
                model: "mock".into(),
                message_count: 4,
                runtime_context_message: Some("runtime".into()),
                transient_context_message: Some("transient".into()),
                cache_miss_reason: CacheMissReason::None,
                cache_trace_context: Some(trace.clone()),
            },
            CacheTraceObservation::Usage {
                model: "mock".into(),
                input_tokens: 4,
                output_tokens: 4,
                cache_trace_context: Some(trace),
            },
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn cache_trace_retry_emits_each_request_once_and_usage_only_after_success() {
    let _env = env_lock();
    let _trace_env = EnvVarGuard::set("REBON_CACHE_TRACE", "1");
    let client = MockModelClient::new();
    client.push_error(ModelError::Http("temporary failure".into()));
    client.push_turn(text_turn("msg_done", "done"));
    let (seat, observations) = cache_trace_recording_seat();
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("hello")])
        .with_turn_hook_seat(seat)
        .with_max_iterations(3);

    let mut rx = run_query(
        build_engine_with_tools(Vec::new()),
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert_eq!(
        *observations
            .lock()
            .expect("cache trace observations poisoned"),
        [
            CacheTraceObservation::Request {
                model: "mock".into(),
                message_count: 1,
                runtime_context_message: None,
                transient_context_message: None,
                cache_miss_reason: CacheMissReason::None,
                cache_trace_context: None,
            },
            CacheTraceObservation::Request {
                model: "mock".into(),
                message_count: 1,
                runtime_context_message: None,
                transient_context_message: None,
                cache_miss_reason: CacheMissReason::RetryWithoutPreviousResponseId,
                cache_trace_context: None,
            },
            CacheTraceObservation::Usage {
                model: "mock".into(),
                input_tokens: 4,
                output_tokens: 4,
                cache_trace_context: None,
            },
        ]
    );
}

#[tokio::test]
async fn cache_trace_hard_error_emits_request_without_usage() {
    let _env = env_lock();
    let _trace_env = EnvVarGuard::set("REBON_CACHE_TRACE", "1");
    let client = MockModelClient::new();
    client.push_error(ModelError::other("hard failure"));
    let (seat, observations) = cache_trace_recording_seat();
    let params =
        QueryParams::new("mock", vec![ApiMessage::user_text("hello")]).with_turn_hook_seat(seat);

    let mut rx = run_query(
        build_engine_with_tools(Vec::new()),
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Error(_))));
    assert_eq!(
        *observations
            .lock()
            .expect("cache trace observations poisoned"),
        [CacheTraceObservation::Request {
            model: "mock".into(),
            message_count: 1,
            runtime_context_message: None,
            transient_context_message: None,
            cache_miss_reason: CacheMissReason::None,
            cache_trace_context: None,
        }]
    );
}

#[tokio::test]
async fn turn_hook_observes_a_complete_tool_round_in_query_event_order() {
    let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "file body"})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_tool",
        "Read",
        "toolu_hook",
        "{\"path\":\"a.rs\"}",
    ));
    client.push_turn(text_turn("msg_done", "done"));

    let observed = Arc::new(Mutex::new(Vec::new()));
    let seat = crate::turn_hook::TurnHookSeat::new();
    let _subscription = seat
        .subscribe(
            "observer",
            crate::turn_hook::Order::NORMAL,
            Arc::new({
                let observed = observed.clone();
                move |event: &QueryEvent, _context: &mut crate::turn_hook::TurnHookContext| {
                    observed
                        .lock()
                        .expect("observed turn events poisoned")
                        .push(query_event_kind(event));
                }
            }),
        )
        .unwrap();
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("read a.rs")])
        .with_tools(tools_from_engine(&engine))
        .with_turn_hook_seat(seat)
        .with_max_iterations(3);

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;
    let received: Vec<_> = events.iter().map(query_event_kind).collect();
    let observed = observed
        .lock()
        .expect("observed turn events poisoned")
        .clone();

    assert_eq!(observed, received, "the hook must see every public event");
    assert_eq!(
        received
            .into_iter()
            .filter(|kind| *kind != "stream")
            .collect::<Vec<_>>(),
        [
            "iteration",
            "tool-start",
            "tool-result",
            "iteration",
            "done"
        ]
    );
}

#[tokio::test]
async fn turn_hook_history_and_optional_params_reach_the_next_request() {
    let engine = build_engine_with_tools(Vec::new());
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_first", "first response"));
    client.push_turn(text_turn("msg_second", "second response"));
    let captured_client = client.clone();

    let seat = crate::turn_hook::TurnHookSeat::new();
    let _subscription = seat
        .subscribe(
            "writer",
            crate::turn_hook::Order::NORMAL,
            Arc::new(
                |event: &QueryEvent, context: &mut crate::turn_hook::TurnHookContext| {
                    let QueryEvent::IterationComplete {
                        iteration: 0,
                        message,
                    } = event
                    else {
                        return;
                    };
                    context.append_history(ApiMessage {
                        role: Role::Assistant,
                        content: message.content.clone(),
                    });
                    context.append_history(ApiMessage::user_text("hook history"));
                    context.update_params(|params| {
                        params.system = Some("hook system".into());
                    });
                    context.request_continue();
                },
            ),
        )
        .unwrap();
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("start")])
        .with_turn_hook_seat(seat)
        .with_max_iterations(1);

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
    assert_eq!(requests[1].system.as_deref(), Some("hook system"));
    let second_history = text_message_texts(&requests[1].messages);
    assert!(second_history.contains(&"first response"));
    assert!(second_history.contains(&"hook history"));
}

#[tokio::test]
async fn executor_resolves_the_turn_hook_seat_from_the_kernel_scope() {
    let _env = env_lock();
    let _trace_env = EnvVarGuard::set("REBON_CACHE_TRACE", "1");
    let engine = build_engine_with_tools(Vec::new());
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_done", "done"));
    let projects_root = temp_projects_root("kernel_turn_hook_seat");
    let cwd = projects_root.path().to_string_lossy().to_string();

    let kernel = rebon_kernel::Kernel::new();
    let seat = crate::turn_hook::TurnHookSeat::new();
    kernel
        .context()
        .provide::<crate::turn_hook::TurnHookSeatService>(seat.clone())
        .unwrap();
    let cache_observations = Arc::new(Mutex::new(Vec::new()));
    let cache_registration = seat
        .subscribe(
            "tests/executor-cache-trace",
            crate::turn_hook::Order::LAST,
            Arc::new(RecordingCacheTraceHook {
                observations: cache_observations.clone(),
            }),
        )
        .unwrap();
    drop(cache_registration);
    let observed_done = Arc::new(AtomicBool::new(false));
    let _subscription = seat
        .subscribe(
            "kernel-observer",
            crate::turn_hook::Order::NORMAL,
            Arc::new({
                let observed_done = observed_done.clone();
                move |event: &QueryEvent, _context: &mut crate::turn_hook::TurnHookContext| {
                    if matches!(event, QueryEvent::Done { .. }) {
                        observed_done.store(true, Ordering::Release);
                    }
                }
            }),
        )
        .unwrap();
    let lease = crate::permission::KernelContextLease::unmanaged(kernel.context().clone());
    let executor = EngineQueryExecutor::new(engine, Arc::new(client), projects_root.path(), "mock")
        .with_kernel_context_resolver(Arc::new(move |_session_id| Some(lease.clone())))
        .with_system_prompt_config(test_system_prompt_config());

    executor
        .execute(basic_prompt_request("turn-hook-session", &cwd))
        .await
        .unwrap();

    assert!(observed_done.load(Ordering::Acquire));
    let observations = cache_observations
        .lock()
        .expect("cache trace observations poisoned");
    assert!(matches!(
        observations.first(),
        Some(CacheTraceObservation::SessionBasePrompt {
            stable_base_system: true,
            cache_hits,
        }) if !cache_hits.is_empty()
    ));
}
