use super::*;

#[test]
fn stable_base_system_env_defaults_on_and_accepts_false_values() {
    let _guard = env_lock();
    let _stable_guard = EnvVarGuard::set_absent("REBON_STABLE_BASE_SYSTEM");
    assert!(stable_base_system_enabled());
    std::env::set_var("REBON_STABLE_BASE_SYSTEM", "");
    assert!(stable_base_system_enabled());
    std::env::set_var("REBON_STABLE_BASE_SYSTEM", "1");
    assert!(stable_base_system_enabled());
    std::env::set_var("REBON_STABLE_BASE_SYSTEM", "true");
    assert!(stable_base_system_enabled());
    std::env::set_var("REBON_STABLE_BASE_SYSTEM", "yes");
    assert!(stable_base_system_enabled());
    std::env::set_var("REBON_STABLE_BASE_SYSTEM", "0");
    assert!(!stable_base_system_enabled());
    std::env::set_var("REBON_STABLE_BASE_SYSTEM", "false");
    assert!(!stable_base_system_enabled());
    std::env::set_var("REBON_STABLE_BASE_SYSTEM", "no");
    assert!(!stable_base_system_enabled());
    std::env::set_var("REBON_STABLE_BASE_SYSTEM", "off");
    assert!(!stable_base_system_enabled());
    std::env::remove_var("REBON_STABLE_BASE_SYSTEM");
}

#[test]
fn session_prompt_cache_key_ignores_dynamic_tool_lists_but_covers_base_prompt_inputs() {
    let guard = env_lock();
    let _simple_guard = EnvVarGuard::set_absent("REBON_SIMPLE");
    let _sub_agents_guard = SubAgentsEnabledGuard::set(true);

    let config = test_system_prompt_config();
    let ctx = DynamicPromptContext {
        coordinator_mode: false,
        coordinator_use_worktree: false,
        ..Default::default()
    };

    let key = SessionPromptCacheKey::from_config(&config, &ctx);
    assert_eq!(key, SessionPromptCacheKey::from_config(&config, &ctx));

    let mut changed = config.clone();
    changed.model = "other-model".to_string();
    assert_ne!(key, SessionPromptCacheKey::from_config(&changed, &ctx));

    let mut changed = config.clone();
    changed.model_marketing_name = Some("Other Marketing Name".to_string());
    assert_ne!(key, SessionPromptCacheKey::from_config(&changed, &ctx));

    let mut changed = config.clone();
    changed.knowledge_cutoff = Some("June 2026".to_string());
    assert_ne!(key, SessionPromptCacheKey::from_config(&changed, &ctx));

    let mut changed = config.clone();
    changed.tool_names.push("Write".to_string());
    assert_eq!(key, SessionPromptCacheKey::from_config(&changed, &ctx));

    let mut changed = config.clone();
    changed.deferred_tool_names.push("Deferred".to_string());
    assert_eq!(key, SessionPromptCacheKey::from_config(&changed, &ctx));

    let mut changed = config.clone();
    changed.platform = "windows".to_string();
    assert_ne!(key, SessionPromptCacheKey::from_config(&changed, &ctx));

    let mut changed = config.clone();
    changed.shell = "powershell".to_string();
    assert_ne!(key, SessionPromptCacheKey::from_config(&changed, &ctx));

    let mut changed = config.clone();
    changed.os_version = "other-os".to_string();
    assert_ne!(key, SessionPromptCacheKey::from_config(&changed, &ctx));

    let mut changed = config.clone();
    changed.normal_system_prompt_override = Some("custom normal persona".to_string());
    assert_ne!(key, SessionPromptCacheKey::from_config(&changed, &ctx));

    let mut changed = config.clone();
    changed.minimal_system_prompt_override = Some("custom minimal persona".to_string());
    assert_eq!(key, SessionPromptCacheKey::from_config(&changed, &ctx));

    let coordinator_ctx = DynamicPromptContext {
        coordinator_mode: true,
        coordinator_use_worktree: false,
        ..Default::default()
    };
    assert_ne!(
        key,
        SessionPromptCacheKey::from_config(&config, &coordinator_ctx)
    );

    let worktree_ctx = DynamicPromptContext {
        coordinator_mode: false,
        coordinator_use_worktree: true,
        ..Default::default()
    };
    assert_ne!(
        key,
        SessionPromptCacheKey::from_config(&config, &worktree_ctx)
    );

    std::env::set_var("REBON_SIMPLE", "1");
    assert_ne!(key, SessionPromptCacheKey::from_config(&config, &ctx));
    std::env::remove_var("REBON_SIMPLE");

    rebon_tool::set_sub_agents_enabled(false);
    assert_ne!(key, SessionPromptCacheKey::from_config(&config, &ctx));
    drop(guard);
}

#[test]
fn session_prompt_state_misses_when_sub_agents_toggle_changes() {
    let _guard = env_lock();
    let _sub_agents_guard = SubAgentsEnabledGuard::set(rebon_tool::sub_agents_enabled());
    let state = SessionPromptState::default();
    let config = test_system_prompt_config();
    let ctx = DynamicPromptContext::default();
    let calls = AtomicUsize::new(0);

    rebon_tool::set_sub_agents_enabled(false);
    let disabled_key = SessionPromptCacheKey::from_config(&config, &ctx);
    let (disabled_base, disabled_hit) = state.get_or_insert_base_system(disabled_key, || {
        calls.fetch_add(1, Ordering::SeqCst);
        "base-with-sub-agents-disabled".to_string()
    });

    rebon_tool::set_sub_agents_enabled(true);
    let enabled_key = SessionPromptCacheKey::from_config(&config, &ctx);
    let (enabled_base, enabled_hit) = state.get_or_insert_base_system(enabled_key, || {
        calls.fetch_add(1, Ordering::SeqCst);
        "base-with-sub-agents-enabled".to_string()
    });

    assert!(!disabled_hit);
    assert!(!enabled_hit);
    assert_eq!(&*disabled_base, "base-with-sub-agents-disabled");
    assert_eq!(&*enabled_base, "base-with-sub-agents-enabled");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn session_prompt_state_builds_once_per_key() {
    let state = SessionPromptState::default();
    let key = SessionPromptCacheKey::from_config(
        &test_system_prompt_config(),
        &DynamicPromptContext::default(),
    );
    let calls = AtomicUsize::new(0);

    let (first, first_hit) = state.get_or_insert_base_system(key.clone(), || {
        calls.fetch_add(1, Ordering::SeqCst);
        "base".to_string()
    });
    let (second, second_hit) = state.get_or_insert_base_system(key, || {
        calls.fetch_add(1, Ordering::SeqCst);
        "rebuilt".to_string()
    });

    assert!(!first_hit);
    assert!(second_hit);
    assert_eq!(&*first, "base");
    assert_eq!(&*second, "base");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let mut other_config = test_system_prompt_config();
    other_config.model = "other".to_string();
    let other_key =
        SessionPromptCacheKey::from_config(&other_config, &DynamicPromptContext::default());
    let (_, other_hit) = state.get_or_insert_base_system(other_key, || {
        calls.fetch_add(1, Ordering::SeqCst);
        "other-base".to_string()
    });
    assert!(!other_hit);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn stable_prompt_helper_uses_cached_base_but_rebuilds_runtime_context() {
    let state = SessionPromptState::default();
    let config = test_system_prompt_config();
    let ctx_one = DynamicPromptContext {
        cwd: "/tmp/one".to_string(),
        session_date: Some("2026-01-01".to_string()),
        ..Default::default()
    };
    let ctx_two = DynamicPromptContext {
        cwd: "/tmp/two".to_string(),
        session_date: Some("2026-01-02".to_string()),
        ..Default::default()
    };

    let (base_one, runtime_one, transient_one, hit_one) =
        build_prompt_parts_with_session_cache(&config, &ctx_one, &state);
    let (base_two, runtime_two, transient_two, hit_two) =
        build_prompt_parts_with_session_cache(&config, &ctx_two, &state);

    assert!(!hit_one);
    assert!(hit_two);
    assert_eq!(base_one, base_two);
    assert_ne!(runtime_one, runtime_two);
    assert_eq!(transient_one, transient_two);
    assert!(runtime_one.unwrap().contains("/tmp/one"));
    assert!(runtime_two.unwrap().contains("/tmp/two"));
}

#[test]
fn doc_update_triggers_fire_once_per_change() {
    let state = SessionPromptState::default();
    let baseline = AnnouncedDocSnapshot {
        rebon_md_content: Some("initial docs".to_string()),
        memory_prompt: None,
    };
    // First call records the baseline: the frozen runtime context
    // already carries this content, so nothing is announced.
    assert!(state.doc_update_triggers(baseline.clone()).is_empty());
    // Unchanged inputs stay silent.
    assert!(state.doc_update_triggers(baseline).is_empty());

    let edited = AnnouncedDocSnapshot {
        rebon_md_content: Some("edited docs".to_string()),
        memory_prompt: Some("new memory".to_string()),
    };
    let triggers = state.doc_update_triggers(edited.clone());
    assert_eq!(triggers.len(), 2);
    assert!(triggers[0].display_path.contains("REBON.md"));
    assert_eq!(triggers[0].content, "edited docs");
    assert!(triggers[1].display_path.contains("auto-memory"));
    assert_eq!(triggers[1].content, "new memory");

    // Announced content is recorded — no re-announcement.
    assert!(state.doc_update_triggers(edited.clone()).is_empty());

    // An input that disappears updates the baseline silently.
    let removed = AnnouncedDocSnapshot {
        rebon_md_content: None,
        memory_prompt: edited.memory_prompt.clone(),
    };
    assert!(state.doc_update_triggers(removed).is_empty());
}

#[test]
fn legacy_and_explicit_system_paths_do_not_need_session_cache() {
    let state = SessionPromptState::default();
    let config = test_system_prompt_config();
    let ctx = DynamicPromptContext {
        cwd: "/tmp/legacy".to_string(),
        ..Default::default()
    };

    let legacy_full = crate::system_prompt::build_system_prompt(&config, &ctx);
    assert!(legacy_full.contains("/tmp/legacy"));
    assert_eq!(state.base_system_by_key.lock().unwrap().len(), 0);

    let explicit_system = Some("explicit".to_string());
    assert_eq!(explicit_system.as_deref(), Some("explicit"));
    assert_eq!(state.base_system_by_key.lock().unwrap().len(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn stable_base_system_provider_requests_survive_rebon_md_mutation() {
    let _guard = env_lock();
    let _stable_guard = EnvVarGuard::set("REBON_STABLE_BASE_SYSTEM", "1");

    let engine = build_engine_with_tools(vec![
        Arc::new(RecordingTool::new("Read", json!({"ok": true}))) as Arc<dyn Tool>,
        Arc::new(RecordingTool::new("AskUserQuestion", json!({"ok": true}))) as Arc<dyn Tool>,
        Arc::new(RecordingTool::deferred("Write", json!({"ok": true}))) as Arc<dyn Tool>,
    ]);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "first stable response"));
    client.push_turn(text_turn("msg_2", "second stable response"));
    client.push_turn(text_turn("msg_3", "third stable response"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("stable_base_rebon_md_mutation");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    std::fs::write(
        projects_root.join("REBON.md"),
        "initial project instruction",
    )
    .unwrap();
    let session = state.create_session(cwd.clone(), Vec::new());
    let snapshot = Arc::new(RwLock::new(None));

    // Rendering a document find is `rebon-plugin-memory`'s now; supplying it
    // is still the executor's. Stand in for the plugin with a producer that
    // drains the binding's triggers and renders them the way the plugin does,
    // so this test keeps checking the engine's half: the find reaches the
    // model once, appended, without rewriting the frozen block at index 0.
    struct DocsProducer;
    struct DocsPoller(Arc<dyn crate::attachment_seat::TurnDocumentTriggers>);

    impl AttachmentPoller for DocsPoller {
        fn poll(&self, _request: AttachmentPollRequest<'_>) -> Vec<rebon_api::Message> {
            self.0
                .drain_document_triggers()
                .iter()
                .map(|trigger| {
                    rebon_api::Message::user_text(format!(
                        "<system-reminder>\nContents of {}:\n\n{}\n</system-reminder>",
                        trigger.display_path, trigger.content
                    ))
                })
                .collect()
        }
    }

    impl crate::attachment_seat::SeatAttachmentProducer for DocsProducer {
        fn poller_for_session(
            &self,
            binding: &crate::attachment_seat::SessionAttachmentBinding,
        ) -> Option<Arc<dyn AttachmentPoller>> {
            binding
                .documents
                .clone()
                .map(|documents| Arc::new(DocsPoller(documents)) as Arc<dyn AttachmentPoller>)
        }
    }

    let kernel = rebon_kernel::Kernel::new();
    let seat = crate::attachment_seat::AttachmentSeat::new();
    kernel
        .context()
        .provide::<crate::attachment_seat::AttachmentSeatService>(seat.clone())
        .unwrap();
    seat.register(
        kernel.context(),
        "memory",
        crate::attachment_seat::Order::Context,
        Arc::new(DocsProducer),
    )
    .unwrap();
    let lease = crate::permission::KernelContextLease::unmanaged(kernel.context().clone());

    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system_prompt_config(test_system_prompt_config())
        .with_system_prompt_snapshot(snapshot.clone())
        .with_kernel_context_resolver(Arc::new(move |_session_id| Some(lease.clone())))
        .with_server_state(state.clone());

    let make_request = |text: &str| rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: text.into(),
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

    let outcome = executor.execute(make_request("first turn")).await;
    assert!(outcome.is_ok(), "first execute failed: {:?}", outcome.err());
    std::fs::write(
        projects_root.join("REBON.md"),
        "updated project instruction",
    )
    .unwrap();
    let outcome = executor.execute(make_request("second turn")).await;
    assert!(
        outcome.is_ok(),
        "second execute failed: {:?}",
        outcome.err()
    );
    let outcome = executor.execute(make_request("third turn unchanged")).await;
    assert!(outcome.is_ok(), "third execute failed: {:?}", outcome.err());

    let captured = client_handle.captured_requests();
    assert_eq!(captured.len(), 3);
    let systems = captured
        .iter()
        .map(|request| {
            request
                .system
                .as_deref()
                .expect("stable base system present")
        })
        .collect::<Vec<_>>();
    assert_eq!(systems[0], systems[1]);
    assert_eq!(systems[1], systems[2]);
    for system in &systems {
        assert!(!system.contains("initial project instruction"));
        assert!(!system.contains("updated project instruction"));
        assert!(!system.contains("Read"));
        assert!(!system.contains("AskUserQuestion"));
        assert!(!system.contains("Write"));
        assert!(system.contains("ToolSearch may expose additional capabilities"));
        assert!(system.contains("InvokeDeferredTool gateway"));
    }
    assert_eq!(snapshot.read().unwrap().as_deref(), Some(systems[2]));
    assert_eq!(
        executor
            .session_prompt_state
            .base_system_by_key
            .lock()
            .unwrap()
            .len(),
        1
    );

    let runtime_contexts = captured
        .iter()
        .map(|request| captured_runtime_context_text(request))
        .collect::<Vec<_>>();
    for runtime_context in &runtime_contexts {
        assert!(runtime_context.contains("<system-reminder>"));
        assert!(runtime_context.contains("<runtime_context>"));
        assert!(runtime_context.contains("</runtime_context>"));
        assert!(runtime_context.contains("</system-reminder>"));
        assert!(runtime_context.contains("Provider-visible tools loaded in advance for this turn"));
        assert!(runtime_context.contains("Read"));
        assert!(runtime_context.contains("AskUserQuestion"));
        assert!(runtime_context.contains("Write"));
        if runtime_context.contains(
            "The deferred tools below can be used this turn once discovered through ToolSearch",
        ) {
            assert!(runtime_context.contains("not exhaustive"));
            assert!(
                runtime_context.contains("Never describe a listed deferred tool as unavailable")
            );
            assert!(runtime_context.contains("retrieve each schema before first invoking its tool"));
        }
    }
    assert!(captured
        .iter()
        .all(|request| request.transient_context.is_none()));
    // The stable runtime-context block is frozen for the session: it
    // rides at message index 0, so re-rendering it after a mid-session
    // REBON.md edit would rewrite the first message and re-prefill the
    // whole conversation on the next request. The edit becomes visible
    // on the next session (or when the announced tool shape changes,
    // which busts the provider prefix cache anyway).
    assert!(runtime_contexts[0].contains("initial project instruction"));
    assert!(!runtime_contexts[0].contains("updated project instruction"));
    assert!(runtime_contexts[1].contains("initial project instruction"));
    assert!(!runtime_contexts[1].contains("updated project instruction"));
    assert!(runtime_contexts[2].contains("initial project instruction"));
    assert_eq!(runtime_contexts[0], runtime_contexts[1]);
    assert_eq!(runtime_contexts[1], runtime_contexts[2]);

    // The edit still reaches the model — once, as an appended
    // nested-memory reminder, not by rewriting the frozen block.
    let count_update_reminders = |request: &rebon_api::CreateMessageRequest| {
        request
            .messages
            .iter()
            .filter(|message| {
                message.role == Role::User
                    && message.content.iter().any(|block| {
                        matches!(
                            block,
                            ApiContentBlock::Text(text)
                                if text.text.contains("updated mid-session")
                                    && text.text.contains("updated project instruction")
                        )
                    })
            })
            .count()
    };
    assert_eq!(count_update_reminders(&captured[0]), 0);
    assert_eq!(count_update_reminders(&captured[1]), 1);
    // Turn 3 replays the turn-2 reminder in history but must not add a
    // second one — the diff already recorded the announced content.
    assert_eq!(count_update_reminders(&captured[2]), 1);
}

/// Prompt surfaces that neither inject a file-history tracker nor pass a
/// `user_message_uuid` (background worker, ACP server, headless runs) must
/// still produce a rewindable snapshot: the executor arms a session-scoped
/// tracker keyed by the persisted transcript user-row uuid and records the
/// post-turn head when the turn completes.
#[tokio::test(flavor = "current_thread")]
async fn prompt_turn_without_injected_tracker_records_file_history_snapshot() {
    let _guard = env_lock();

    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "done"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("file-history-fallback");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());

    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_server_state(state.clone());

    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.id.clone(),
        cwd: cwd.clone(),
        prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: "make an edit".into(),
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

    let record = state.get_session(&session.id).expect("session record");
    let user_uuid = record
        .loaded_transcript
        .iter()
        .find(|entry| entry.uuid.starts_with("u-user-"))
        .map(|entry| entry.uuid.clone())
        .expect("persisted transcript user row");

    let store = rebon_session::FileHistoryStore::new(projects_root, &cwd, &session.id);
    let manifest = store
        .load_manifest()
        .expect("executor-armed turn creates the file-history manifest");
    assert!(
        manifest
            .snapshots
            .iter()
            .any(|snapshot| snapshot.message_id == user_uuid),
        "snapshot must be keyed by the transcript user uuid"
    );
    assert_eq!(
        manifest
            .current_head
            .as_ref()
            .map(|head| head.message_id.as_str()),
        Some(user_uuid.as_str()),
        "post-turn head must be recorded when the turn completes"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_system_override_with_stable_flag_skips_cache_and_runtime_context() {
    let _guard = env_lock();
    let _stable_guard = EnvVarGuard::set("REBON_STABLE_BASE_SYSTEM", "1");

    let tool = Arc::new(RecordingTool::new("Bash", json!({"ok": true})));
    let engine = build_engine_with(tool as Arc<dyn Tool>);
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "explicit response"));
    let client_handle = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let projects_root_dir = temp_projects_root("explicit_system_stable_base");
    let projects_root = projects_root_dir.path();
    let state = Arc::new(rebon_session_state::ServerState::new());
    let cwd = projects_root.to_string_lossy().to_string();
    let session = state.create_session(cwd.clone(), Vec::new());
    let snapshot = Arc::new(RwLock::new(None));

    let executor = EngineQueryExecutor::new(engine, client, projects_root, "mock-model")
        .with_system("explicit system override")
        .with_system_prompt_config(test_system_prompt_config())
        .with_system_prompt_snapshot(snapshot.clone())
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
    assert_eq!(
        executor
            .session_prompt_state
            .base_system_by_key
            .lock()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        snapshot.read().unwrap().as_deref(),
        Some("explicit system override")
    );

    let captured = client_handle.captured_requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].system.as_deref(),
        Some("explicit system override")
    );
    assert_eq!(captured[0].messages.len(), 1);
    let request_text = match &captured[0].messages[0].content[0] {
        ApiContentBlock::Text(text) => text.text.as_str(),
        other => panic!("expected text prompt, got {other:?}"),
    };
    assert!(!request_text.contains("<runtime_context>"));
}
