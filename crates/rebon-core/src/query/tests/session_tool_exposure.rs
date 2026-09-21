use super::*;

#[test]
fn normal_session_projection_hides_queue_and_escalation_tools() {
    let engine = Engine::with_builtin_tools();
    let filter = crate::coordinator_mode::normal_session_filter();
    let projection = runtime_tool_projection(
        &engine,
        true,
        Some(&filter),
        None,
        &engine.deferred_tool_names(),
        &[],
        None,
    );
    let available = projection.available_tool_names();
    let indexed = projection.tool_search_index.names();

    for tool in crate::coordinator_mode::QUEUE_SESSION_TOOLS
        .iter()
        .copied()
        .chain(
            crate::coordinator_mode::COORDINATOR_MODE_ONLY_TOOLS
                .iter()
                .copied(),
        )
    {
        assert!(
            !available.contains(&tool.to_string()),
            "normal projection exposed `{tool}`"
        );
        assert!(
            !indexed.contains(&tool),
            "normal search index exposed `{tool}`"
        );
    }
    assert!(available.contains(&"Read".to_string()));
}

#[test]
fn queue_session_projection_exposes_queue_tools_without_ceo_escalation_tools() {
    let engine = crate::engine_with_every_builtin_tool();
    let filter = crate::coordinator_mode::queue_session_filter();
    let projection = runtime_tool_projection(
        &engine,
        true,
        Some(&filter),
        None,
        &engine.deferred_tool_names(),
        &[],
        None,
    );
    let available = projection.available_tool_names();

    for tool in crate::coordinator_mode::QUEUE_SESSION_TOOLS {
        assert!(
            available.contains(&tool.to_string()),
            "queue projection hid `{tool}`"
        );
    }
    assert!(!available.contains(&"EscalateQuestion".to_string()));
    assert!(!available.contains(&"ResolveEscalation".to_string()));
    assert!(available.contains(&"Bash".to_string()));
}

#[test]
fn coordinator_session_projection_keeps_agent_and_deferred_gateway_visible() {
    let engine = crate::engine_with_every_builtin_tool();
    let filter = crate::coordinator_mode::coordinator_session_filter();
    let projection = runtime_tool_projection(
        &engine,
        true,
        Some(&filter),
        None,
        &engine.deferred_tool_names(),
        &[],
        None,
    );
    let visible = projection.provider_visible_tool_names();
    let deferred = projection.deferred_tool_names;
    let indexed = projection.tool_search_index.names();

    assert!(visible.contains(&"Agent".to_string()));
    assert!(visible.contains(&"ResolveEscalation".to_string()));
    assert!(!visible.contains(&"EscalateQuestion".to_string()));
    for tool in crate::coordinator_mode::QUEUE_SESSION_TOOLS {
        assert!(!visible.contains(&tool.to_string()));
        assert!(!deferred.contains(&tool.to_string()));
        assert!(!indexed.contains(tool));
    }
    assert!(visible.contains(&rebon_tool::TOOL_SEARCH_TOOL_NAME.to_string()));
    assert!(visible.contains(&rebon_tool::INVOKE_DEFERRED_TOOL_NAME.to_string()));
    assert!(!visible.contains(&"Workflow".to_string()));
    assert!(!deferred.contains(&"Workflow".to_string()));
    assert!(!indexed.contains(&"Workflow"));
    assert!(!deferred.contains(&"Agent".to_string()));
    assert!(!indexed.contains(&"Agent"));
}

#[test]
fn async_agent_projection_keeps_deferred_worker_gateway_visible() {
    let engine = crate::engine_with_every_builtin_tool();
    let filter = crate::coordinator_mode::async_agent_filter();
    let projection = runtime_tool_projection(
        &engine,
        true,
        Some(&filter),
        None,
        &engine.deferred_tool_names(),
        &[],
        None,
    );
    let visible = projection.provider_visible_tool_names();
    let deferred = projection.deferred_tool_names;
    let indexed = projection.tool_search_index.names();

    assert!(visible.contains(&rebon_tool::TOOL_SEARCH_TOOL_NAME.to_string()));
    assert!(visible.contains(&rebon_tool::INVOKE_DEFERRED_TOOL_NAME.to_string()));
    assert!(visible.contains(&"EscalateQuestion".to_string()));
    assert!(!visible.contains(&"ResolveEscalation".to_string()));
    for tool in crate::coordinator_mode::QUEUE_SESSION_TOOLS {
        assert!(!visible.contains(&tool.to_string()));
        assert!(!deferred.contains(&tool.to_string()));
        assert!(!indexed.contains(tool));
    }
    if cfg!(windows) {
        // PowerShell is the primary shell on Windows and is exposed eagerly.
        assert!(visible.contains(&"PowerShell".to_string()));
        assert!(!deferred.contains(&"PowerShell".to_string()));
        assert!(!indexed.contains(&"PowerShell"));
    } else if rebon_tool::powershell::is_available() {
        assert!(deferred.contains(&"PowerShell".to_string()));
        assert!(indexed.contains(&"PowerShell"));
    } else {
        // Off Windows the tool disables itself when no `pwsh` is installed,
        // which the CI image may not have. Assert against that fact rather
        // than against the platform, the way
        // `shell_tool_preference_switches_registered_shell_tools` does.
        assert!(!visible.contains(&"PowerShell".to_string()));
        assert!(!deferred.contains(&"PowerShell".to_string()));
        assert!(!indexed.contains(&"PowerShell"));
    }
}

#[test]
fn runtime_tool_projection_removes_deferred_channel_when_tool_search_disabled() {
    let engine = crate::engine_with_every_builtin_tool();
    let projection = runtime_tool_projection(
        &engine,
        false,
        None,
        None,
        &engine.deferred_tool_names(),
        &[],
        None,
    );
    let visible = projection.provider_visible_tool_names();
    let available = projection.available_tool_names();

    assert!(projection.deferred_tool_names.is_empty());
    assert!(projection.tool_search_index.is_empty());
    assert!(!visible.contains(&rebon_tool::TOOL_SEARCH_TOOL_NAME.to_string()));
    assert!(visible.contains(&"SaveMemory".to_string()));
    assert!(available.contains(&"SaveMemory".to_string()));
}

/// The names-only listing is the full snapshot minus the schemas: same
/// tools, same order, same aliases — so a filter applied to one applies
/// identically to the other.
#[test]
fn eager_tool_name_snapshots_list_exactly_the_eager_snapshots() {
    let engine = Engine::with_builtin_tools();
    let full = engine.eager_tool_snapshots();
    let names = engine.eager_tool_name_snapshots();
    assert!(!names.is_empty());
    assert_eq!(names.len(), full.len());
    for (name, snapshot) in names.iter().zip(full.iter()) {
        assert_eq!(name.name, snapshot.name);
        assert_eq!(name.aliases, snapshot.aliases);
    }
}

#[test]
fn stable_builtin_exposure_policy_projects_expected_eager_and_deferred_sets() {
    let engine = crate::engine_with_every_builtin_tool();
    let registered: std::collections::HashSet<_> = engine.tool_names().into_iter().collect();
    let eager: std::collections::HashSet<_> = engine
        .eager_tool_snapshots()
        .into_iter()
        .map(|snap| snap.name)
        .collect();
    let deferred: std::collections::HashSet<_> = engine.deferred_tool_names().into_iter().collect();
    let index_names: std::collections::HashSet<_> = engine
        .build_tool_search_index()
        .names()
        .into_iter()
        .map(str::to_owned)
        .collect();

    for name in [
        "ToolSearch",
        "InvokeDeferredTool",
        "Read",
        "Glob",
        "Grep",
        "Edit",
        "Write",
        "Bash",
        "ShellOutput",
        "ShellStop",
        "Skill",
        "Agent",
        "TaskCreate",
        "TaskGet",
        "TaskList",
        "TaskUpdate",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
    ] {
        if registered.contains(name) {
            assert!(eager.contains(name), "{name} should be eager");
            assert!(!deferred.contains(name), "{name} should not be deferred");
            assert!(!index_names.contains(name), "{name} should not be indexed");
        }
    }

    for name in [
        "Mcp",
        "SaveMemory",
        "SendMessage",
        "CronCreate",
        "CronList",
        "CronDelete",
        "Sleep",
        "TeamCreate",
        "TeamDelete",
    ] {
        if registered.contains(name) {
            assert!(!eager.contains(name), "{name} should not be eager");
            assert!(deferred.contains(name), "{name} should be deferred");
            assert!(index_names.contains(name), "{name} should be indexed");
        }
    }

    // PowerShell is eager on Windows (primary shell) and deferred elsewhere.
    if registered.contains("PowerShell") {
        if cfg!(windows) {
            assert!(eager.contains("PowerShell"));
            assert!(!deferred.contains("PowerShell"));
            assert!(!index_names.contains("PowerShell"));
        } else {
            assert!(!eager.contains("PowerShell"));
            assert!(deferred.contains("PowerShell"));
            assert!(index_names.contains("PowerShell"));
        }
    }
}

#[test]
fn filtered_exposure_policy_applies_to_eager_deferred_and_search_index() {
    let engine = crate::engine_with_every_builtin_tool();
    let filter = ToolFilter::allow_only([
        "Read",
        "AskUserQuestion",
        rebon_tool::TOOL_SEARCH_TOOL_NAME,
        rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
    ]);

    let eager_names: Vec<_> = filtered_eager_tools_from_engine(&engine, &filter)
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    // Core tools (from the seat) lead the catalog in their own order; a
    // feature tool the host registered follows them.
    assert_eq!(
        eager_names,
        vec![
            "Read".to_string(),
            rebon_tool::TOOL_SEARCH_TOOL_NAME.to_string(),
            rebon_tool::INVOKE_DEFERRED_TOOL_NAME.to_string(),
            "AskUserQuestion".to_string(),
        ]
    );

    let deferred_names = engine.filtered_deferred_tool_names(&filter);
    assert!(deferred_names.is_empty());
    assert!(engine
        .build_filtered_tool_search_index(&filter)
        .names()
        .is_empty());
}

#[tokio::test]
async fn policy_deferred_builtin_gateway_auto_records_discovery_without_trait_defer() {
    let engine = crate::engine_with_every_builtin_tool();
    let index = Arc::new(engine.build_tool_search_index());
    assert!(index.names().contains(&"WebSearch"));
    let context = ToolContext::new().with_tool_search_index(index);

    // The gateway no longer requires a prior ToolSearch call: an indexed
    // target is auto-recorded as discovered and proceeds to the normal
    // validation pipeline (here: empty-arguments rejection). The target has
    // to be a tool this catalogue holds for real rather than one of the
    // metadata-only stand-ins, which refuse to execute.
    let undiscovered = engine
        .invoke_tool(
            rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
            json!({
                "tool_name": "WebSearch",
                "arguments": {}
            }),
            &context,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(!undiscovered.contains("has not been discovered through ToolSearch"));
    assert!(!undiscovered.contains("is not a deferred tool"));
    assert_eq!(
        context.discovered_deferred_tool_names(),
        vec!["WebSearch".to_string()]
    );
}

#[test]
fn no_tool_search_empty_index_fallback_keeps_all_enabled_tools_except_tool_search() {
    let engine = crate::engine_with_every_builtin_tool();
    let index = rebon_tool::ToolSearchIndex::default();
    let tools = tools_for_filter_and_index(&engine, None, None, &index);
    let names: std::collections::HashSet<_> = tools.iter().map(|tool| tool.name.as_str()).collect();

    assert!(!names.contains(rebon_tool::TOOL_SEARCH_TOOL_NAME));
    assert!(names.contains(rebon_tool::INVOKE_DEFERRED_TOOL_NAME));
    assert!(names.contains("Read"));
    assert!(names.contains("AskUserQuestion"));
}

#[tokio::test]
async fn gateway_auto_records_discovery_and_validates_target() {
    let mut engine = Engine::new();
    engine.register_tool(Arc::new(rebon_tool::ToolSearchTool));
    engine.register_tool(Arc::new(rebon_tool::InvokeDeferredTool));
    engine.register_tool(Arc::new(GatewayDeferredTool));
    let index = Arc::new(engine.build_tool_search_index());
    let context = ToolContext::new().with_tool_search_index(index);

    let missing_arguments = engine
        .invoke_tool(
            rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
            json!({ "tool_name": "GatewayDeferred" }),
            &context,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(missing_arguments.contains("invalid InvokeDeferredTool input"));
    assert!(missing_arguments.contains("arguments"));

    // No prior ToolSearch needed: an indexed target is auto-recorded as
    // discovered and dispatches straight through the gateway.
    let undiscovered_ok = engine
        .invoke_tool(
            rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
            json!({
                "tool_name": "GatewayDeferred",
                "arguments": { "message": "hello" }
            }),
            &context,
        )
        .await
        .unwrap();
    assert_eq!(
        undiscovered_ok,
        json!({ "called_with": { "message": "hello" } })
    );
    assert_eq!(
        context.discovered_deferred_tool_names(),
        vec!["GatewayDeferred".to_string()]
    );

    let unknown = engine
        .invoke_tool(
            rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
            json!({ "tool_name": "MissingDeferred", "arguments": {} }),
            &context,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(unknown.contains("unknown tool") || unknown.contains("MissingDeferred"));

    let non_deferred = engine
        .invoke_tool(
            rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
            json!({ "tool_name": rebon_tool::TOOL_SEARCH_TOOL_NAME, "arguments": {} }),
            &context,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(non_deferred.contains("is not a deferred tool"));

    let discovery = engine
        .invoke_tool(
            rebon_tool::TOOL_SEARCH_TOOL_NAME,
            json!({ "query": "select:GatewayDeferred" }),
            &context,
        )
        .await
        .unwrap();
    assert_eq!(discovery["matched_tools"], json!(["GatewayDeferred"]));
    assert_eq!(
        context.discovered_deferred_tool_names(),
        vec!["GatewayDeferred".to_string()]
    );

    let invalid = engine
        .invoke_tool(
            rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
            json!({ "tool_name": "GatewayDeferred", "arguments": {} }),
            &context,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(invalid.contains("Empty arguments for `GatewayDeferred`"));
    assert!(invalid.contains("message"));

    let out = engine
        .invoke_tool(
            rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
            json!({
                "tool_name": "GatewayDeferred",
                "arguments": { "message": "hello" }
            }),
            &context,
        )
        .await
        .unwrap();
    assert_eq!(out, json!({ "called_with": { "message": "hello" } }));
}

#[tokio::test]
async fn dispatch_accepts_direct_calls_to_indexed_deferred_tools() {
    let mut engine = Engine::new().with_permission_broker(Arc::new(AllowAllBroker));
    engine.register_tool(Arc::new(rebon_tool::ToolSearchTool));
    engine.register_tool(Arc::new(rebon_tool::InvokeDeferredTool));
    engine.register_tool(Arc::new(GatewayDeferredTool));
    let engine = Arc::new(engine);
    let index = Arc::new(engine.build_tool_search_index());
    assert!(index.contains_name("GatewayDeferred"));

    // The announced provider-visible set stays eager-only: the deferred
    // tool is absent, but a direct call to its name must still dispatch
    // because the name is in the (filter-respecting) ToolSearch index.
    let announced = tools_for_filter_and_index(&engine, None, None, &index);
    assert!(!announced.iter().any(|tool| tool.name == "GatewayDeferred"));

    let mock = MockModelClient::new();
    mock.push_turn(tool_turn(
        "msg_1",
        "GatewayDeferred",
        "toolu_1",
        r#"{"message":"hello"}"#,
    ));
    mock.push_turn(text_turn("msg_2", "done"));

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("go")])
        .with_tools(announced)
        .with_max_iterations(3);

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(mock)),
        params,
        ToolContext::new().with_tool_search_index(index),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let outcome = events.iter().find_map(|event| match event {
        QueryEvent::ToolDispatchResult { name, outcome, .. } if name == "GatewayDeferred" => {
            Some(outcome.clone())
        }
        _ => None,
    });
    let value = outcome
        .expect("direct deferred dispatch should reach the tool")
        .expect("direct deferred dispatch should succeed");
    assert_eq!(value, json!({ "called_with": { "message": "hello" } }));
    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
}

#[test]
fn eager_tools_include_gateway_but_not_deferred_tool() {
    let mut engine = Engine::new();
    engine.register_tool(Arc::new(rebon_tool::ToolSearchTool));
    engine.register_tool(Arc::new(rebon_tool::InvokeDeferredTool));
    engine.register_tool(Arc::new(GatewayDeferredTool));

    let index = engine.build_tool_search_index();
    let tools = tools_for_filter_and_index(&engine, None, None, &index);
    let names: Vec<_> = tools.iter().map(|tool| tool.name.as_str()).collect();

    assert!(names.contains(&rebon_tool::TOOL_SEARCH_TOOL_NAME));
    assert!(names.contains(&rebon_tool::INVOKE_DEFERRED_TOOL_NAME));
    assert!(!names.contains(&"GatewayDeferred"));
    assert_eq!(
        tools
            .iter()
            .find(|tool| tool.name == rebon_tool::INVOKE_DEFERRED_TOOL_NAME)
            .unwrap()
            .input_schema,
        rebon_tool::InvokeDeferredTool.input_schema()
    );
}

#[tokio::test]
async fn tool_search_discovery_does_not_change_provider_visible_tools() {
    let mut engine = crate::engine_with_every_builtin_tool();
    engine.register_tool(Arc::new(GatewayDeferredTool));

    let index = Arc::new(engine.build_tool_search_index());
    let context = ToolContext::new().with_tool_search_index(index.clone());
    let before = tools_for_filter_and_index(&engine, None, None, &index);
    engine
        .invoke_tool(
            rebon_tool::TOOL_SEARCH_TOOL_NAME,
            json!({ "query": "select:GatewayDeferred,SaveMemory" }),
            &context,
        )
        .await
        .unwrap();
    let after = tools_for_filter_and_index(&engine, None, None, &index);

    let before_names: Vec<_> = before.iter().map(|tool| tool.name.as_str()).collect();
    let after_names: Vec<_> = after.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(before_names, after_names);
    assert!(!after_names.contains(&"GatewayDeferred"));
    assert!(after_names.contains(&"AskUserQuestion"));
    assert!(!after_names.contains(&"SaveMemory"));
    assert!(after_names.contains(&rebon_tool::INVOKE_DEFERRED_TOOL_NAME));
}
