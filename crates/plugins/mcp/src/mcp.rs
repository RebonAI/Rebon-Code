//! MCP tools and concrete clients. Shared DTOs and the client contract are
//! owned by `rebon-tool`; this plugin owns executable behavior and transports.

use async_trait::async_trait;
use rebon_tool::mcp::*;
use rebon_tool::{Tool, ToolContext, ToolFilter};
use rebon_tools_core::{
    PermissionDecision, PermissionRequest, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const INVALID_INPUT_CODE: i64 = 400;

pub fn parse_mcp_tool_definitions(result: &Value) -> Vec<McpToolDefinition> {
    result
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(parse_mcp_tool_definition)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn parse_mcp_tool_definition(tool: &Value) -> Option<McpToolDefinition> {
    let name = tool.get("name")?.as_str()?.to_string();
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let input_schema = tool
        .get("inputSchema")
        .filter(|schema| schema.is_object())
        .cloned()
        .unwrap_or_else(default_mcp_input_schema);
    let annotations = tool.get("annotations");
    let meta = tool.get("_meta");
    let search_hint = meta
        .and_then(|meta| meta.get("anthropic/searchHint"))
        .and_then(Value::as_str)
        .map(|hint| hint.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|hint| !hint.is_empty());
    let always_load = meta
        .and_then(|meta| meta.get("anthropic/alwaysLoad"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    Some(McpToolDefinition {
        name,
        description,
        input_schema,
        read_only: annotations
            .and_then(|annotations| annotations.get("readOnlyHint"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        destructive: annotations
            .and_then(|annotations| annotations.get("destructiveHint"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        open_world: annotations
            .and_then(|annotations| annotations.get("openWorldHint"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        search_hint,
        always_load,
    })
}

/// In-memory [`McpClient`] useful for tests: pre-seed per-server
/// tool maps and observe every call.
#[derive(Debug, Clone, Default)]
pub struct InMemoryMcpClient {
    inner: Arc<Mutex<InMemoryInner>>,
}

#[derive(Debug, Default)]
struct InMemoryInner {
    servers: HashMap<String, HashMap<String, Value>>,
    errors: HashMap<(String, String), McpClientError>,
    definitions: HashMap<String, HashMap<String, McpToolDefinition>>,
    calls: Vec<McpToolCall>,
}

impl InMemoryMcpClient {
    /// Construct an empty client.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `(server, tool)` with a canned response payload.
    pub fn register_tool(
        &self,
        server: impl Into<String>,
        name: impl Into<String>,
        response: Value,
    ) {
        let server = server.into();
        let name = name.into();
        let mut guard = self.inner.lock().expect("in-memory mcp client poisoned");
        guard
            .servers
            .entry(server.clone())
            .or_default()
            .insert(name.clone(), response);
        guard
            .definitions
            .entry(server)
            .or_default()
            .entry(name.clone())
            .or_insert_with(|| McpToolDefinition::new(name));
    }

    pub fn register_tool_definition(
        &self,
        server: impl Into<String>,
        definition: McpToolDefinition,
        response: Value,
    ) {
        let server = server.into();
        let mut guard = self.inner.lock().expect("in-memory mcp client poisoned");
        guard
            .servers
            .entry(server.clone())
            .or_default()
            .insert(definition.name.clone(), response);
        guard
            .definitions
            .entry(server)
            .or_default()
            .insert(definition.name.clone(), definition);
    }

    /// Inject an error for the next call to `(server, tool)`.
    pub fn inject_error(
        &self,
        server: impl Into<String>,
        name: impl Into<String>,
        error: McpClientError,
    ) {
        let mut guard = self.inner.lock().expect("in-memory mcp client poisoned");
        guard.errors.insert((server.into(), name.into()), error);
    }

    /// Snapshot of every call recorded so far.
    pub fn calls(&self) -> Vec<McpToolCall> {
        let guard = self.inner.lock().expect("in-memory mcp client poisoned");
        guard.calls.clone()
    }
}

#[async_trait]
impl McpClient for InMemoryMcpClient {
    async fn call_tool(&self, call: McpToolCall) -> Result<McpToolResult, McpClientError> {
        let mut guard = self.inner.lock().expect("in-memory mcp client poisoned");
        guard.calls.push(call.clone());
        if let Some(err) = guard
            .errors
            .remove(&(call.server.clone(), call.name.clone()))
        {
            return Err(err);
        }
        let server = guard
            .servers
            .get(&call.server)
            .ok_or_else(|| McpClientError::UnknownServer(call.server.clone()))?;
        let content =
            server
                .get(&call.name)
                .cloned()
                .ok_or_else(|| McpClientError::UnknownTool {
                    server: call.server.clone(),
                    tool: call.name.clone(),
                })?;
        Ok(McpToolResult {
            server: call.server,
            name: call.name,
            content,
            is_error: false,
        })
    }

    async fn list_tools(&self, server: &str) -> Option<Vec<String>> {
        let guard = self.inner.lock().expect("in-memory mcp client poisoned");
        guard.servers.get(server).map(|tools| {
            let mut names = tools.keys().cloned().collect::<Vec<_>>();
            names.sort();
            names
        })
    }

    async fn list_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        let guard = self.inner.lock().expect("in-memory mcp client poisoned");
        let mut definitions = guard
            .definitions
            .get(server)
            .map(|tools| tools.values().cloned().collect::<Vec<_>>())?;
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        Some(definitions)
    }

    fn cached_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        let guard = self.inner.lock().expect("in-memory mcp client poisoned");
        let mut definitions = guard
            .definitions
            .get(server)
            .map(|tools| tools.values().cloned().collect::<Vec<_>>())?;
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        Some(definitions)
    }

    fn server_names(&self) -> Vec<String> {
        let guard = self.inner.lock().expect("in-memory mcp client poisoned");
        let mut names: Vec<String> = guard.servers.keys().cloned().collect();
        names.sort();
        names
    }

    /// Forgetting a seeded server is the in-memory equivalent of a teardown.
    /// Reconnecting is not: there is no configuration here to rebuild from, so
    /// the default `Ok(false)` — "not mine" — is the truthful answer.
    async fn disconnect_server(&self, name: &str) -> bool {
        let mut guard = self.inner.lock().expect("in-memory mcp client poisoned");
        guard.definitions.remove(name);
        guard.servers.remove(name).is_some()
    }
}

/// Aggregate [`McpClient`] that routes calls by logical server name.
pub struct AggregateMcpClient {
    clients: Vec<Arc<dyn McpClient>>,
}

impl std::fmt::Debug for AggregateMcpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AggregateMcpClient")
            .field("client_count", &self.clients.len())
            .finish()
    }
}

impl AggregateMcpClient {
    pub fn new(clients: Vec<Arc<dyn McpClient>>) -> Self {
        Self { clients }
    }
}

#[async_trait]
impl McpClient for AggregateMcpClient {
    async fn call_tool(&self, call: McpToolCall) -> Result<McpToolResult, McpClientError> {
        for client in &self.clients {
            if client
                .server_names()
                .iter()
                .any(|name| name == &call.server)
            {
                return client.call_tool(call).await;
            }
        }
        Err(McpClientError::UnknownServer(call.server))
    }

    async fn list_tools(&self, server: &str) -> Option<Vec<String>> {
        for client in &self.clients {
            if client.server_names().iter().any(|name| name == server) {
                return client.list_tools(server).await;
            }
        }
        None
    }

    async fn list_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        for client in &self.clients {
            if client.server_names().iter().any(|name| name == server) {
                return client.list_tool_definitions(server).await;
            }
        }
        None
    }

    fn cached_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        for client in &self.clients {
            if client.server_names().iter().any(|name| name == server) {
                return client.cached_tool_definitions(server);
            }
        }
        None
    }

    fn server_names(&self) -> Vec<String> {
        let mut names = self
            .clients
            .iter()
            .flat_map(|client| client.server_names())
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        names
    }

    fn drain_channel_notifications(&self) -> Vec<String> {
        self.clients
            .iter()
            .flat_map(|client| client.drain_channel_notifications())
            .collect()
    }

    async fn shutdown_transport(&self) {
        for client in &self.clients {
            client.shutdown_transport().await;
        }
    }

    /// Close every transport at once, each under the full budget.
    ///
    /// Deliberately not the sequential loop above with a shrinking deadline:
    /// transports are independent, so a stdio client whose servers are slow to
    /// reap has no business eating the budget an SSE client needed. Running
    /// them together keeps the caller's total wait at the budget while giving
    /// each transport all of it.
    async fn close_with_timeout(&self, budget: std::time::Duration) -> McpShutdownReport {
        let reports = futures_util::future::join_all(
            self.clients
                .iter()
                .map(|client| client.close_with_timeout(budget)),
        )
        .await;
        let mut merged = McpShutdownReport::default();
        for report in reports {
            merged.absorb(report);
        }
        merged
    }

    /// The first client that has the name answers for it. A server name is
    /// unique across transports — the config is one map — so there is nothing
    /// to fan out to, and stopping at the first owner keeps a reconnect from
    /// being attempted twice.
    async fn disconnect_server(&self, name: &str) -> bool {
        for client in &self.clients {
            if client.disconnect_server(name).await {
                return true;
            }
        }
        false
    }

    async fn reconnect_server(&self, name: &str) -> Result<bool, McpClientError> {
        for client in &self.clients {
            match client.reconnect_server(name).await {
                // "not mine" — keep looking. An error is this server's error
                // and stops the search, because the name was found.
                Ok(false) => continue,
                answered => return answered,
            }
        }
        Ok(false)
    }
}

/// Tool that routes calls to an [`McpClient`] via [`ToolContext`].
#[derive(Debug, Clone, Default)]
pub struct McpTool;

#[derive(Debug, Clone)]
pub struct McpProxyTool {
    id: String,
    server: String,
    name: String,
    description: String,
    input_schema: Value,
    read_only: bool,
    destructive: bool,
    should_defer: bool,
    search_hint: Option<String>,
    active: Arc<std::sync::atomic::AtomicBool>,
}

impl McpProxyTool {
    pub fn new(server: impl Into<String>, definition: McpToolDefinition) -> Self {
        let server = server.into();
        let id = build_mcp_tool_name(&server, &definition.name);
        let should_defer = definition.should_defer();
        Self {
            id,
            server,
            name: definition.name,
            description: definition.description,
            input_schema: definition.input_schema,
            read_only: definition.read_only,
            destructive: definition.destructive,
            should_defer,
            search_hint: definition.search_hint,
            active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    pub(super) fn with_liveness(mut self, active: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.active = active;
        self
    }

    fn ensure_live(&self) -> ToolResult<()> {
        if self.is_enabled() {
            Ok(())
        } else {
            Err(ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!("[STALE_PROVIDER] MCP plugin is unloaded"),
            })
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn mcp_name(&self) -> &str {
        &self.name
    }
}

#[async_trait]
impl Tool for McpProxyTool {
    fn is_enabled(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Acquire)
    }

    fn id(&self) -> ToolId {
        ToolId::new(self.id.clone())
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> ToolInputSchema {
        self.input_schema.clone()
    }

    fn should_defer(&self) -> bool {
        self.should_defer
    }

    fn search_hint(&self) -> Option<&str> {
        self.search_hint.as_deref()
    }

    fn is_concurrency_safe(&self, input: &Value) -> bool {
        self.is_read_only(input)
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        self.read_only
    }

    fn is_destructive(&self, _input: &Value) -> bool {
        self.destructive
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        self.ensure_live()?;
        if input.is_object() {
            Ok(ValidationOutcome::valid())
        } else {
            Ok(ValidationOutcome::invalid(
                "MCP tool input must be a JSON object",
                INVALID_INPUT_CODE,
            ))
        }
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        self.ensure_live()?;
        Ok(PermissionDecision::ask(
            PermissionRequest::new(
                "Call MCP tool",
                format!("Call {} on MCP server {}", self.name, self.server),
            )
            .with_options(["allow_once", "allow_always", "reject_once"]),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        self.ensure_live()?;
        let client = context
            .mcp_client()
            .ok_or_else(|| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!("MCP proxy tool requires an McpClient on the ToolContext"),
            })?
            .clone();
        let result = client
            .call_tool(McpToolCall {
                server: self.server.clone(),
                name: self.name.clone(),
                arguments: input,
            })
            .await
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            })?;
        Ok(json!({
            "server": result.server,
            "name": result.name,
            "content": result.content,
            "isError": result.is_error,
        }))
    }
}

pub fn mcp_proxy_tool_from_filter(
    definitions: &[(String, McpToolDefinition)],
    filter: Option<&ToolFilter>,
    tool_name: &str,
) -> Option<McpProxyTool> {
    definitions
        .iter()
        .find(|(server, definition)| {
            build_mcp_tool_name(server, &definition.name) == tool_name
                && filter.is_none_or(|filter| filter.allows(tool_name, &[]))
        })
        .map(|(server, definition)| McpProxyTool::new(server.clone(), definition.clone()))
}

#[async_trait]
impl Tool for McpTool {
    fn id(&self) -> ToolId {
        ToolId::new(MCP_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["McpTool", "MCPTool"]
    }

    fn description(&self) -> &str {
        "Call a tool exposed by a configured MCP (Model Context Protocol) server."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Logical MCP server name."
                },
                "name": {
                    "type": "string",
                    "description": "Tool name exposed by the server."
                },
                "arguments": {
                    "type": "object",
                    "description": "JSON arguments forwarded to the server."
                }
            },
            "required": ["server", "name"],
            "additionalProperties": false
        })
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        match parse_input(input) {
            Ok(_) => Ok(ValidationOutcome::valid()),
            Err(message) => Ok(ValidationOutcome::invalid(message, INVALID_INPUT_CODE)),
        }
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let server = input
            .get("server")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        Ok(PermissionDecision::ask(
            PermissionRequest::new(
                "Call MCP tool",
                format!("Call {name} on MCP server {server}"),
            )
            .with_options(["allow_once", "allow_always", "reject_once"]),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let call = parse_input(&input).map_err(|reason| ToolError::InvalidInput {
            tool: self.id(),
            reason,
            error_code: Some(INVALID_INPUT_CODE),
        })?;

        let client = context
            .mcp_client()
            .ok_or_else(|| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!("McpTool requires an McpClient on the ToolContext"),
            })?
            .clone();

        let result = client
            .call_tool(call)
            .await
            .map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(err),
            })?;

        Ok(json!({
            "server": result.server,
            "name": result.name,
            "content": result.content,
            "isError": result.is_error,
        }))
    }
}

fn parse_input(input: &Value) -> Result<McpToolCall, String> {
    let object = input
        .as_object()
        .ok_or_else(|| "McpTool input must be a JSON object".to_string())?;

    let server = object
        .get("server")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "`server` is required".to_string())?
        .trim()
        .to_string();
    if server.is_empty() {
        return Err("`server` must not be empty".to_string());
    }

    let name = object
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "`name` is required".to_string())?
        .trim()
        .to_string();
    if name.is_empty() {
        return Err("`name` must not be empty".to_string());
    }

    let arguments = object
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    if !arguments.is_object() {
        return Err("`arguments` must be an object".to_string());
    }

    Ok(McpToolCall {
        server,
        name,
        arguments,
    })
}

#[cfg(test)]
mod tests {

    /// An aggregate has to route a lifecycle call to the one client that owns
    /// the name, and stop there — a reconnect attempted twice would dial a
    /// second copy of the same server.
    #[tokio::test]
    async fn an_aggregate_routes_a_disconnect_to_the_client_that_owns_the_name() {
        let first = InMemoryMcpClient::new();
        first.register_tool("alpha", "t", json!({}));
        let second = InMemoryMcpClient::new();
        second.register_tool("beta", "t", json!({}));

        let aggregate = AggregateMcpClient::new(vec![
            Arc::new(first.clone()) as Arc<dyn McpClient>,
            Arc::new(second.clone()) as Arc<dyn McpClient>,
        ]);

        assert!(aggregate.disconnect_server("beta").await);
        assert_eq!(first.server_names(), vec!["alpha".to_string()]);
        assert!(second.server_names().is_empty());

        assert!(
            !aggregate.disconnect_server("ghost").await,
            "a name no client owns is nobody's to disconnect"
        );
    }

    /// In-memory clients have no configuration to dial again, and say so
    /// rather than claiming a reconnect that did not happen.
    #[tokio::test]
    async fn an_in_memory_client_reports_that_it_cannot_reconnect() {
        let client = InMemoryMcpClient::new();
        client.register_tool("alpha", "t", json!({}));
        assert!(!client.reconnect_server("alpha").await.unwrap());
    }

    use super::*;

    #[tokio::test]
    async fn mcp_tool_routes_call_to_client_and_echoes_result() {
        let client = Arc::new(InMemoryMcpClient::new());
        client.register_tool("notes", "fetch", json!({ "title": "hello" }));

        let tool = McpTool;
        let context = ToolContext::new().with_mcp_client(client.clone() as Arc<dyn McpClient>);
        let out = tool
            .call(
                json!({
                    "server": "notes",
                    "name": "fetch",
                    "arguments": { "id": "42" }
                }),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["server"], "notes");
        assert_eq!(out["name"], "fetch");
        assert_eq!(out["content"]["title"], "hello");

        let recorded = client.calls();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].server, "notes");
        assert_eq!(recorded[0].arguments["id"], "42");
    }

    #[tokio::test]
    async fn mcp_tool_errors_when_no_client_is_injected() {
        let tool = McpTool;
        let context = ToolContext::new();
        let err = tool
            .call(json!({ "server": "notes", "name": "fetch" }), &context)
            .await
            .unwrap_err();
        match err {
            ToolError::Execution { source, .. } => {
                assert!(source.to_string().contains("McpClient"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_tool_surfaces_unknown_server_error_from_client() {
        let client = Arc::new(InMemoryMcpClient::new());
        let tool = McpTool;
        let context = ToolContext::new().with_mcp_client(client as Arc<dyn McpClient>);
        let err = tool
            .call(json!({ "server": "ghost", "name": "x" }), &context)
            .await
            .unwrap_err();
        match err {
            ToolError::Execution { source, .. } => {
                assert!(source.to_string().contains("unknown MCP server"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_tool_validation_rejects_missing_fields() {
        let tool = McpTool;
        let context = ToolContext::new();
        let out = tool.validate_input(&json!({}), &context).await.unwrap();
        assert!(!out.is_valid());
        let out = tool
            .validate_input(&json!({ "server": "notes" }), &context)
            .await
            .unwrap();
        assert!(!out.is_valid());
    }

    #[tokio::test]
    async fn mcp_tool_validation_rejects_non_object_arguments() {
        let tool = McpTool;
        let context = ToolContext::new();
        let out = tool
            .validate_input(
                &json!({ "server": "s", "name": "t", "arguments": "nope" }),
                &context,
            )
            .await
            .unwrap();
        assert!(!out.is_valid());
    }

    #[tokio::test]
    async fn in_memory_client_list_tools_returns_registered_names() {
        let client = InMemoryMcpClient::new();
        client.register_tool("notes", "fetch", json!(null));
        client.register_tool("notes", "create", json!(null));
        let mut listed = client.list_tools("notes").await.unwrap();
        listed.sort();
        assert_eq!(listed, vec!["create".to_string(), "fetch".to_string()]);
        assert!(client.list_tools("ghost").await.is_none());
    }

    #[test]
    fn mcp_tool_definitions_default_to_eager() {
        let definition = McpToolDefinition::new("search_patents");
        assert!(!definition.should_defer());
    }

    #[test]
    fn parses_mcp_tool_metadata_from_tools_list_result() {
        let definitions = parse_mcp_tool_definitions(&json!({
            "tools": [{
                "name": "search_patents",
                "description": "Search patent data",
                "inputSchema": {
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                },
                "annotations": {
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "openWorldHint": true
                },
                "_meta": {
                    "anthropic/searchHint": "patent\nsearch",
                    "anthropic/alwaysLoad": true
                }
            }]
        }));

        assert_eq!(definitions.len(), 1);
        let definition = &definitions[0];
        assert_eq!(definition.name, "search_patents");
        assert_eq!(definition.input_schema["required"][0], "query");
        assert!(definition.read_only);
        assert!(definition.open_world);
        assert_eq!(definition.search_hint.as_deref(), Some("patent search"));
        assert!(!definition.should_defer());
        assert_eq!(
            build_mcp_tool_name("patent-search", "search_patents"),
            "mcp__patent-search__search_patents"
        );
    }

    #[tokio::test]
    async fn in_memory_client_inject_error_fires_once() {
        let client = InMemoryMcpClient::new();
        client.register_tool("notes", "fetch", json!({}));
        client.inject_error("notes", "fetch", McpClientError::Transport("down".into()));
        let err = client
            .call_tool(McpToolCall {
                server: "notes".into(),
                name: "fetch".into(),
                arguments: json!({}),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, McpClientError::Transport(_)));
        // Second call succeeds.
        let ok = client
            .call_tool(McpToolCall {
                server: "notes".into(),
                name: "fetch".into(),
                arguments: json!({}),
            })
            .await
            .unwrap();
        assert_eq!(ok.name, "fetch");
    }
}
