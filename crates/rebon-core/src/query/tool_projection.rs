use super::*;

pub(super) fn filter_for_execution_policy(policy: Option<&ExecutionPolicy>) -> Option<ToolFilter> {
    let ultraplan = policy.and_then(|p| p.ultraplan.as_ref())?;
    Some(
        ToolFilter::allow_only(ultraplan.allowed_tools.clone())
            .with_deny(ultraplan.denied_tools.clone()),
    )
}

pub(super) fn combine_filters(
    active_tool_filter: Option<&ToolFilter>,
    execution_policy: Option<&ExecutionPolicy>,
) -> Option<ToolFilter> {
    match (
        active_tool_filter,
        filter_for_execution_policy(execution_policy),
    ) {
        (Some(active), Some(policy_filter)) => Some(active.intersect(&policy_filter)),
        (Some(active), None) => Some(active.clone()),
        (None, Some(policy_filter)) => Some(policy_filter),
        (None, None) => None,
    }
}

#[derive(Clone)]
pub(super) struct RuntimeToolProjection {
    pub(super) provider_visible_tools: Vec<ApiTool>,
    pub(super) deferred_tool_names: Vec<String>,
    pub(super) tool_search_index: Arc<rebon_tool::ToolSearchIndex>,
}

impl RuntimeToolProjection {
    pub(super) fn provider_visible_tool_names(&self) -> Vec<String> {
        self.provider_visible_tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    }

    /// The union of eager and deferred names — every tool the turn offers
    /// the model, one way or the other. Used by the tests that assert a
    /// projection's whole surface; the prompt seat gets the same set as two
    /// separate lists on [`crate::prompt_seat::PromptSubject`].
    #[cfg(test)]
    pub(super) fn available_tool_names(&self) -> Vec<String> {
        let mut names = self.provider_visible_tool_names();
        for name in &self.deferred_tool_names {
            if !names.iter().any(|visible| visible == name) {
                names.push(name.clone());
            }
        }
        names
    }
}

pub(super) fn runtime_tool_projection(
    engine: &Engine,
    use_tool_search: bool,
    filter: Option<&ToolFilter>,
    execution_policy: Option<&ExecutionPolicy>,
    base_deferred_names: &[String],
    mcp_tool_definitions: &[(String, McpToolDefinition)],
    plugin_tools: Option<&Arc<dyn rebon_tool::PluginToolProvider>>,
) -> RuntimeToolProjection {
    runtime_tool_projection_for_mode(
        engine,
        use_tool_search,
        filter,
        execution_policy,
        base_deferred_names,
        mcp_tool_definitions,
        AgentCapabilityMode::Normal,
        plugin_tools,
    )
}

pub(super) fn runtime_tool_projection_for_mode(
    engine: &Engine,
    use_tool_search: bool,
    filter: Option<&ToolFilter>,
    execution_policy: Option<&ExecutionPolicy>,
    base_deferred_names: &[String],
    mcp_tool_definitions: &[(String, McpToolDefinition)],
    capability_mode: AgentCapabilityMode,
    plugin_tools: Option<&Arc<dyn rebon_tool::PluginToolProvider>>,
) -> RuntimeToolProjection {
    // Chat is the whole capability: no tools reach the model, so nothing can
    // read or write the workspace. Returned empty rather than filtered down,
    // because "no tools" has to hold against every later contributor — MCP
    // servers, deferred discovery, and plugin compositions alike.
    if capability_mode.is_chat() {
        return RuntimeToolProjection {
            provider_visible_tools: Vec::new(),
            deferred_tool_names: Vec::new(),
            tool_search_index: Arc::new(rebon_tool::ToolSearchIndex::default()),
        };
    }

    // Minimal leaves before the plugin leg on purpose: the capability is a
    // fixed, named tool surface, and an anchored session's first request
    // depends on that surface being byte-identical to the upstream preset.
    // A composition contributing tools into it would break exactly the
    // identity the anchoring measures.
    if capability_mode.is_minimal() {
        return minimal_runtime_tool_projection(
            engine,
            filter,
            execution_policy,
            mcp_tool_definitions,
        );
    }

    let deferred_tool_names = if use_tool_search {
        deferred_tool_names_for_prompt(
            engine,
            base_deferred_names,
            filter,
            execution_policy,
            mcp_tool_definitions,
        )
    } else {
        Vec::new()
    };
    let tool_search_index = tool_search_index_for_filter(
        engine,
        use_tool_search,
        filter,
        execution_policy,
        mcp_tool_definitions,
    );
    let mut provider_visible_tools =
        tools_for_filter_and_index(engine, filter, execution_policy, &tool_search_index);
    provider_visible_tools.extend(mcp_proxy_api_tools(mcp_tool_definitions, filter));
    // Plugin tools last, with same-name entries dropped: dispatch precedence
    // is builtin > MCP > plugin, so a shadowed plugin tool must not appear
    // in the model-visible list under a name that routes elsewhere.
    let visible: std::collections::HashSet<String> = provider_visible_tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    provider_visible_tools.extend(
        plugin_api_tools(plugin_tools, filter)
            .into_iter()
            .filter(|tool| !visible.contains(&tool.name)),
    );

    RuntimeToolProjection {
        provider_visible_tools,
        deferred_tool_names,
        tool_search_index,
    }
}

/// The Anchored Minimal bootstrap projection: the upstream Minimal pair and
/// nothing else — no `ToolSearch`, no deferred-name list. Discovery returns
/// with the promoted projection on the next request.
pub(super) fn anchored_bootstrap_tool_projection() -> RuntimeToolProjection {
    RuntimeToolProjection {
        provider_visible_tools: crate::anchored_minimal::anchored_bootstrap_tools(),
        deferred_tool_names: Vec::new(),
        tool_search_index: Arc::new(rebon_tool::ToolSearchIndex::default()),
    }
}

fn minimal_runtime_tool_projection(
    engine: &Engine,
    filter: Option<&ToolFilter>,
    execution_policy: Option<&ExecutionPolicy>,
    mcp_tool_definitions: &[(String, McpToolDefinition)],
) -> RuntimeToolProjection {
    let effective_filter = combine_filters(filter, execution_policy);
    let snapshots = match effective_filter.as_ref() {
        Some(filter) => engine.filtered_tool_snapshots(filter),
        None => engine.tool_snapshots(),
    };

    let tool_entries = snapshots
        .iter()
        .filter(|snapshot| snapshot.name != rebon_tool::TOOL_SEARCH_TOOL_NAME)
        .map(|snapshot| {
            rebon_tool::ToolSearchIndex::entry_from_parts(
                snapshot.name.clone(),
                snapshot.description.clone(),
                snapshot.input_schema.clone(),
                None,
            )
        });
    let mcp_entries = mcp_tool_definitions
        .iter()
        .filter(|(server, definition)| {
            mcp_tool_allowed(effective_filter.as_ref(), server, definition)
        })
        .map(|(server, definition)| {
            rebon_tool::ToolSearchIndex::entry_from_parts(
                rebon_tool::build_mcp_tool_name(server, &definition.name),
                definition.description.clone(),
                definition.input_schema.clone(),
                definition.search_hint.clone(),
            )
        });
    let tool_search_index = Arc::new(
        rebon_tool::ToolSearchIndex::default().with_entries(tool_entries.chain(mcp_entries)),
    );

    let provider_visible_tools = ["Bash", "Read", rebon_tool::TOOL_SEARCH_TOOL_NAME]
        .into_iter()
        .filter(|name| snapshots.iter().any(|snapshot| snapshot.name == *name))
        .filter_map(minimal_bootstrap_tool)
        .collect::<Vec<_>>();
    let provider_visible_names = provider_visible_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let deferred_tool_names = tool_search_index
        .names()
        .into_iter()
        .filter(|name| !provider_visible_names.contains(name))
        .map(str::to_string)
        .collect();

    RuntimeToolProjection {
        provider_visible_tools,
        deferred_tool_names,
        tool_search_index,
    }
}

fn minimal_bootstrap_tool(name: &str) -> Option<ApiTool> {
    let tool = match name {
        "Bash" => ApiTool {
            name: name.to_string(),
            description: "Run a shell command in the session workspace.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        "Read" => ApiTool {
            name: name.to_string(),
            description: "Read a file by absolute path.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "file_path": { "type": "string" } },
                "required": ["file_path"],
                "additionalProperties": false
            }),
        },
        rebon_tool::TOOL_SEARCH_TOOL_NAME => ApiTool {
            name: name.to_string(),
            description: "Load full schemas for other tools by name or keyword.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        _ => return None,
    };
    Some(tool)
}

/// Model-visible tool entries for session plugin tools (plugin-tool exposure leg).
pub(super) fn plugin_api_tools(
    provider: Option<&Arc<dyn rebon_tool::PluginToolProvider>>,
    filter: Option<&ToolFilter>,
) -> Vec<ApiTool> {
    let Some(provider) = provider else {
        return Vec::new();
    };
    provider
        .tool_names()
        .into_iter()
        .filter(|name| filter.is_none_or(|filter| filter.allows(name, &[])))
        .filter_map(|name| provider.tool(&name))
        .map(|tool| ApiTool {
            name: tool.id().as_str().to_string(),
            description: tool.model_description().to_string(),
            input_schema: tool.input_schema(),
        })
        .collect()
}

pub(super) fn deferred_tool_names_for_prompt(
    engine: &Engine,
    base_names: &[String],
    effective_tool_filter: Option<&ToolFilter>,
    execution_policy: Option<&ExecutionPolicy>,
    mcp_tool_definitions: &[(String, McpToolDefinition)],
) -> Vec<String> {
    let mut names = match (effective_tool_filter, execution_policy) {
        (Some(filter), policy) => engine.filtered_deferred_tool_names_for_policy(filter, policy),
        (None, Some(policy)) => engine.deferred_tool_names_for_policy(Some(policy)),
        (None, None) => base_names.to_vec(),
    };
    names.extend(
        mcp_tool_definitions
            .iter()
            .filter(|(server, definition)| {
                mcp_tool_allowed(effective_tool_filter, server, definition)
            })
            .filter(|(_, definition)| definition.should_defer())
            .map(|(server, definition)| rebon_tool::build_mcp_tool_name(server, &definition.name)),
    );
    names
}

pub(super) fn mcp_tool_allowed(
    filter: Option<&ToolFilter>,
    server: &str,
    definition: &McpToolDefinition,
) -> bool {
    let name = rebon_tool::build_mcp_tool_name(server, &definition.name);
    filter.is_none_or(|filter| filter.allows(&name, &[]))
}

pub(super) fn tool_search_index_for_filter(
    engine: &Engine,
    use_tool_search: bool,
    filter: Option<&ToolFilter>,
    execution_policy: Option<&ExecutionPolicy>,
    mcp_tool_definitions: &[(String, McpToolDefinition)],
) -> Arc<rebon_tool::ToolSearchIndex> {
    if use_tool_search {
        let index = match filter {
            Some(filter) => {
                engine.build_filtered_tool_search_index_for_policy(filter, execution_policy)
            }
            None => engine.build_tool_search_index_for_policy(execution_policy),
        };
        let mcp_entries = mcp_tool_definitions
            .iter()
            .filter(|(server, definition)| mcp_tool_allowed(filter, server, definition))
            .filter(|(_, definition)| definition.should_defer())
            .map(|(server, definition)| {
                rebon_tool::ToolSearchIndex::entry_from_parts(
                    rebon_tool::build_mcp_tool_name(server, &definition.name),
                    definition.description.clone(),
                    definition.input_schema.clone(),
                    definition.search_hint.clone(),
                )
            });
        Arc::new(index.with_entries(mcp_entries))
    } else {
        Arc::new(rebon_tool::ToolSearchIndex::default())
    }
}

pub(super) fn tools_for_filter_and_index(
    engine: &Engine,
    filter: Option<&ToolFilter>,
    execution_policy: Option<&ExecutionPolicy>,
    tool_search_index: &rebon_tool::ToolSearchIndex,
) -> Vec<ApiTool> {
    if tool_search_index.is_empty() {
        let mut all = match filter {
            Some(filter) => filtered_tools_from_engine(engine, filter),
            None => tools_from_engine(engine),
        };
        all.retain(|t| t.name != rebon_tool::TOOL_SEARCH_TOOL_NAME);
        all
    } else {
        match filter {
            Some(filter) => {
                filtered_eager_tools_from_engine_for_policy(engine, filter, execution_policy)
            }
            None => eager_tools_from_engine_for_policy(engine, execution_policy),
        }
    }
}

pub(super) async fn collect_mcp_tool_definitions(
    client: &dyn McpClient,
) -> Vec<(String, McpToolDefinition)> {
    let mut definitions = Vec::new();
    for server in client.server_names() {
        if let Some(mut tools) = client.list_tool_definitions(&server).await {
            tools.sort_by(|a, b| a.name.cmp(&b.name));
            definitions.extend(tools.into_iter().map(|tool| (server.clone(), tool)));
        }
    }
    definitions
}

pub(super) fn mcp_proxy_api_tools(
    definitions: &[(String, McpToolDefinition)],
    filter: Option<&ToolFilter>,
) -> Vec<ApiTool> {
    definitions
        .iter()
        .filter(|(server, definition)| mcp_tool_allowed(filter, server, definition))
        .filter(|(_, definition)| !definition.should_defer())
        .map(|(server, definition)| ApiTool {
            name: rebon_tool::build_mcp_tool_name(server, &definition.name),
            description: definition.description.clone(),
            input_schema: definition.input_schema.clone(),
        })
        .collect()
}

fn api_tool_from_snapshot(snap: ToolSnapshot) -> Vec<ApiTool> {
    let mut tools = vec![ApiTool {
        name: snap.name.clone(),
        description: snap.description.clone(),
        input_schema: snap.input_schema.clone(),
    }];
    if snap.name == rebon_tool::WORKFLOW_TOOL_NAME
        && snap.aliases.contains(&rebon_tool::RUN_WORKFLOW_ALIAS)
    {
        let alias_description = snap.description.clone();
        let alias_schema = snap.input_schema.clone();
        tools.push(ApiTool {
            name: rebon_tool::RUN_WORKFLOW_ALIAS.to_string(),
            description: alias_description,
            input_schema: alias_schema,
        });
    }
    tools
}

/// Build an [`ApiTool`] list reflecting the tools currently
/// registered on [`Engine`].
pub fn tools_from_engine(engine: &Engine) -> Vec<ApiTool> {
    engine
        .tool_snapshots()
        .into_iter()
        .flat_map(api_tool_from_snapshot)
        .collect()
}

/// Build an [`ApiTool`] list containing only **non-deferred** (eager)
/// tools. Deferred tools are excluded — the model discovers them via
/// ToolSearchTool.
pub fn eager_tools_from_engine(engine: &Engine) -> Vec<ApiTool> {
    eager_tools_from_engine_for_policy(engine, None)
}

pub fn eager_tools_from_engine_for_policy(
    engine: &Engine,
    execution_policy: Option<&ExecutionPolicy>,
) -> Vec<ApiTool> {
    engine
        .eager_tool_snapshots_for_policy(execution_policy)
        .into_iter()
        .flat_map(api_tool_from_snapshot)
        .collect()
}

/// Like [`eager_tools_from_engine`] but also applying a
/// [`ToolFilter`].
pub fn filtered_eager_tools_from_engine(engine: &Engine, filter: &ToolFilter) -> Vec<ApiTool> {
    filtered_eager_tools_from_engine_for_policy(engine, filter, None)
}

pub fn filtered_eager_tools_from_engine_for_policy(
    engine: &Engine,
    filter: &ToolFilter,
    execution_policy: Option<&ExecutionPolicy>,
) -> Vec<ApiTool> {
    engine
        .eager_tool_snapshots_for_policy(execution_policy)
        .into_iter()
        .filter(|snap| filter.allows(&snap.name, snap.aliases))
        .flat_map(api_tool_from_snapshot)
        .collect()
}

/// Build an [`ApiTool`] list reflecting only the tools on
/// [`Engine`] that pass the supplied [`ToolFilter`]. Used by the
/// query loop to project the filtered view the model should see.
pub fn filtered_tools_from_engine(engine: &Engine, filter: &ToolFilter) -> Vec<ApiTool> {
    engine
        .filtered_tool_snapshots(filter)
        .into_iter()
        .flat_map(api_tool_from_snapshot)
        .collect()
}

/// Snapshot of a registered tool — name, description, input
/// schema, and alias list. Used by [`tools_from_engine`] to
/// project the engine's tool registry into the API layer, and by
/// [`Engine::filtered_tool_snapshots`] to apply tool visibility
/// filters.
#[derive(Debug, Clone)]
pub struct ToolSnapshot {
    /// Registered tool name.
    pub name: String,
    /// Tool description (forwarded to the model).
    pub description: String,
    /// Tool input schema.
    pub input_schema: Value,
    /// Legacy / alternate names for this tool. Used by
    /// [`rebon_tool::ToolFilter`] to match filter entries that
    /// reference older names (e.g. `FileReadTool` → `Read`).
    pub aliases: &'static [&'static str],
}

/// A tool's name and aliases, and nothing else — for callers that only
/// filter or list tools. A full [`ToolSnapshot`] also carries the input
/// schema, and building 50 of those is what a session startup pays for
/// when all it wanted was the names (`Engine::eager_tool_name_snapshots`).
#[derive(Debug, Clone)]
pub struct ToolNameSnapshot {
    pub name: String,
    pub aliases: &'static [&'static str],
}
