use super::*;

#[test]
fn runtime_tool_projection_uses_one_projection_for_visible_deferred_and_capability_names() {
    let engine = crate::engine_with_every_builtin_tool();
    let projection = runtime_tool_projection(
        &engine,
        true,
        None,
        None,
        &engine.deferred_tool_names(),
        &[],
        None,
    );
    let visible = projection.provider_visible_tool_names();
    let available = projection.available_tool_names();

    assert!(visible.contains(&rebon_tool::TOOL_SEARCH_TOOL_NAME.to_string()));
    assert!(visible.contains(&rebon_tool::INVOKE_DEFERRED_TOOL_NAME.to_string()));
    assert!(!visible.contains(&"SaveMemory".to_string()));
    assert!(projection
        .deferred_tool_names
        .contains(&"SaveMemory".to_string()));
    assert!(projection.tool_search_index.names().contains(&"SaveMemory"));
    assert!(available.contains(&"SaveMemory".to_string()));
}

/// Chat's whole promise is that nothing can touch the workspace, so the
/// emptiness has to survive every later contributor — a tool-search index, an
/// MCP server, and a plugin composition all reach this projection.
#[test]
fn chat_projection_is_empty_against_every_tool_contributor() {
    let engine = Engine::with_builtin_tools();
    let mcp = vec![(
        "server".to_string(),
        McpToolDefinition {
            name: "remote_thing".into(),
            description: "does something remote".into(),
            input_schema: json!({ "type": "object" }),
            read_only: false,
            destructive: false,
            open_world: false,
            search_hint: None,
            // Eager, so it would land in the visible list rather than the
            // deferred one — the harder case for an empty projection.
            always_load: true,
        },
    )];
    let projection = runtime_tool_projection_for_mode(
        &engine,
        true,
        None,
        None,
        &engine.deferred_tool_names(),
        &mcp,
        AgentCapabilityMode::Chat,
        None,
    );

    assert!(
        projection.provider_visible_tools.is_empty(),
        "chat must expose no tools, got {:?}",
        projection.provider_visible_tool_names()
    );
    assert!(
        projection.deferred_tool_names.is_empty(),
        "a deferred name is still a tool the model can ask for"
    );
    assert!(
        projection.tool_search_index.names().is_empty(),
        "an empty tool list with a populated search index is still discoverable"
    );
}

#[test]
fn minimal_projection_starts_with_compact_bash_read_and_tool_search() {
    let engine = Engine::with_builtin_tools();
    let projection = runtime_tool_projection_for_mode(
        &engine,
        false,
        None,
        None,
        &[],
        &[],
        AgentCapabilityMode::Minimal,
        None,
    );

    assert_eq!(
        projection.provider_visible_tool_names(),
        vec!["Bash", "Read", "ToolSearch"]
    );
    assert_eq!(
        projection.provider_visible_tools[0].input_schema["required"],
        json!(["command"])
    );
    assert_eq!(
        projection.provider_visible_tools[1].input_schema["required"],
        json!(["file_path"])
    );
    assert!(projection.tool_search_index.contains_name("Edit"));
    assert!(projection.tool_search_index.contains_name("Bash"));
    assert!(projection.tool_search_index.contains_name("Read"));
    assert!(projection.deferred_tool_names.contains(&"Edit".to_string()));
    assert!(!projection.deferred_tool_names.contains(&"Bash".to_string()));
}

#[test]
fn normal_projection_keeps_full_tools_and_standard_deferred_behavior() {
    let engine = crate::engine_with_every_builtin_tool();
    let deferred = engine.deferred_tool_names();
    let projection = runtime_tool_projection_for_mode(
        &engine,
        true,
        None,
        None,
        &deferred,
        &[],
        AgentCapabilityMode::Normal,
        None,
    );

    assert!(projection
        .provider_visible_tool_names()
        .contains(&"Edit".to_string()));
    assert!(projection
        .provider_visible_tool_names()
        .contains(&rebon_tool::INVOKE_DEFERRED_TOOL_NAME.to_string()));
    assert!(projection
        .deferred_tool_names
        .contains(&"SaveMemory".to_string()));
    let bash = projection
        .provider_visible_tools
        .iter()
        .find(|tool| tool.name == "Bash")
        .expect("Normal mode should expose Bash's full schema");
    assert!(bash.input_schema["properties"].get("timeout").is_some());
    let read = projection
        .provider_visible_tools
        .iter()
        .find(|tool| tool.name == "Read")
        .expect("Normal mode should expose Read's full schema");
    assert!(read.input_schema["properties"].get("pages").is_some());
}

#[test]
fn runtime_tool_projection_exposes_plugin_tools_and_dedupes_shadowed_names() {
    struct StaticPluginTool {
        name: &'static str,
    }

    #[async_trait]
    impl rebon_tool::Tool for StaticPluginTool {
        fn id(&self) -> rebon_tools_core::ToolId {
            rebon_tools_core::ToolId::new(self.name)
        }
        fn description(&self) -> &str {
            "plugin-contributed"
        }
        fn input_schema(&self) -> rebon_tools_core::ToolInputSchema {
            json!({ "type": "object" })
        }
        async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(input)
        }
    }

    struct StaticProvider;

    impl rebon_tool::PluginToolProvider for StaticProvider {
        fn tool(&self, name: &str) -> Option<Arc<dyn rebon_tool::Tool>> {
            matches!(name, "JsEcho" | "Read").then(|| {
                Arc::new(StaticPluginTool {
                    name: if name == "Read" { "Read" } else { "JsEcho" },
                }) as Arc<dyn rebon_tool::Tool>
            })
        }
        fn tool_names(&self) -> Vec<String> {
            vec!["JsEcho".into(), "Read".into()]
        }
    }

    let engine = Engine::with_builtin_tools();
    let provider: Arc<dyn rebon_tool::PluginToolProvider> = Arc::new(StaticProvider);
    let projection = runtime_tool_projection(
        &engine,
        true,
        None,
        None,
        &engine.deferred_tool_names(),
        &[],
        Some(&provider),
    );
    let visible = projection.provider_visible_tool_names();

    // The plugin tool is model-visible; the builtin-shadowed name appears
    // exactly once (dispatch would route `Read` to the builtin).
    assert!(visible.contains(&"JsEcho".to_string()));
    assert_eq!(visible.iter().filter(|name| *name == "Read").count(), 1);
    let echo = projection
        .provider_visible_tools
        .iter()
        .find(|tool| tool.name == "JsEcho")
        .expect("plugin tool present");
    assert_eq!(echo.description, "plugin-contributed");
}

#[test]
fn minimal_bootstrap_static_context_stays_within_500_tokens() {
    let engine = Engine::with_builtin_tools();
    let projection = runtime_tool_projection_for_mode(
        &engine,
        true,
        None,
        None,
        &[],
        &[],
        AgentCapabilityMode::Minimal,
        None,
    );
    let prompt = crate::system_prompt::build_minimal_system_prompt(None);
    let report = PromptCostReport::new(vec![
        prompt_text_section_report("system", "system", &prompt),
        prompt_tool_metadata_report("tools", &projection.provider_visible_tools),
    ]);

    assert!(
        report.estimated_tokens <= 500,
        "minimal bootstrap used {} estimated tokens",
        report.estimated_tokens
    );
}
