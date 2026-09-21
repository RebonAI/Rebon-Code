use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use serde_json::{json, Value};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

use crate::mcp::parse_mcp_tool_definitions;
use rebon_tool::{
    McpClient, McpClientError, McpShutdownReport, McpToolCall, McpToolDefinition, McpToolResult,
};

#[derive(Debug, Clone)]
pub struct SseServerConfig {
    pub name: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub request_timeout: Option<Duration>,
}

impl SseServerConfig {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            headers: Vec::new(),
            request_timeout: None,
        }
    }

    fn effective_timeout(&self) -> Duration {
        self.request_timeout
            .unwrap_or_else(|| Duration::from_secs(30))
    }
}

#[derive(Debug)]
struct SseServer {
    config: SseServerConfig,
    headers: HeaderMap,
    post_url: Mutex<Option<String>>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<JsonRpcResponse, McpClientError>>>>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    tools_cache: Mutex<Option<Vec<McpToolDefinition>>>,
}

/// Own the reader until the connection is published. Cancellation during the
/// endpoint/initialize handshake must not detach a task holding the server alive.
struct PendingReader(Option<tokio::task::AbortHandle>);

impl Drop for PendingReader {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SseMcpClient {
    http: reqwest::Client,
    servers: Arc<Mutex<HashMap<String, Arc<SseServer>>>>,
    /// Configs parked by `disconnect_server`, so `/mcp reconnect` can bring a
    /// disconnected server back. `remove_server` stays a true removal.
    disconnected: Arc<Mutex<HashMap<String, SseServerConfig>>>,
}

#[derive(Debug, Clone)]
struct JsonRpcResponse {
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Clone)]
struct JsonRpcError {
    code: i64,
    message: String,
}

impl SseMcpClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            servers: Arc::new(Mutex::new(HashMap::new())),
            disconnected: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn add_server(&self, config: SseServerConfig) -> Result<(), McpClientError> {
        self.add_server_cancellable(config, std::future::pending())
            .await
    }

    pub(super) async fn add_server_cancellable(
        &self,
        config: SseServerConfig,
        cancel: impl std::future::Future<Output = ()> + Send,
    ) -> Result<(), McpClientError> {
        tokio::pin!(cancel);
        // The guard drops on the `;` rather than at the end of the block, so a
        // teardown does not hold the table every concurrent call has to read.
        let existing = self.servers.lock().await.remove(&config.name);
        if let Some(existing) = existing {
            shutdown_server(existing).await;
        }

        let headers = build_headers(&config.headers)?;
        let server = Arc::new(SseServer {
            config: config.clone(),
            headers,
            post_url: Mutex::new(None),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            reader_handle: Mutex::new(None),
            tools_cache: Mutex::new(None),
        });

        let connect = self
            .http
            .get(&config.url)
            .headers(sse_get_headers(&server.headers))
            .timeout(config.effective_timeout())
            .send();
        let response = tokio::select! {
            biased;
            _ = &mut cancel => return Err(McpClientError::Transport("[STALE_PROVIDER] MCP construction cancelled".into())),
            response = connect => response,
        }
        .map_err(|err| McpClientError::Transport(format!("SSE connect failed: {err}")))?;
        let status = response.status();
        if !status.is_success() {
            let body = tokio::select! {
                biased;
                _ = &mut cancel => return Err(McpClientError::Transport("[STALE_PROVIDER] MCP construction cancelled".into())),
                body = response.text() => body.unwrap_or_default(),
            };
            return Err(McpClientError::Transport(format!(
                "SSE MCP server returned {status}: {body}"
            )));
        }

        let (endpoint_tx, endpoint_rx) = oneshot::channel();
        let reader_server = Arc::clone(&server);
        let reader_handle = tokio::spawn(async move {
            run_sse_reader(reader_server, response, Some(endpoint_tx)).await;
        });
        let mut pending_reader = PendingReader(Some(reader_handle.abort_handle()));
        *server.reader_handle.lock().await = Some(reader_handle);

        let handshake = async {
            let post_url = match tokio::time::timeout(config.effective_timeout(), endpoint_rx).await
            {
                Ok(Ok(post_url)) => post_url,
                Ok(Err(_)) => {
                    return Err(McpClientError::Transport(
                        "SSE stream closed before endpoint event".into(),
                    ))
                }
                Err(_) => {
                    return Err(McpClientError::Transport(
                        "timed out waiting for SSE endpoint event".into(),
                    ))
                }
            };
            *server.post_url.lock().await = Some(post_url);
            initialize_server(&self.http, &server).await
        };
        let initialized = tokio::select! {
            biased;
            _ = &mut cancel => Err(McpClientError::Transport("[STALE_PROVIDER] MCP construction cancelled".into())),
            result = handshake => result,
        };
        if let Err(err) = initialized {
            // The task owning the unpublished reader stays alive to join it.
            shutdown_server(Arc::clone(&server)).await;
            return Err(err);
        }

        self.disconnected.lock().await.remove(&config.name);
        self.servers.lock().await.insert(config.name, server);
        pending_reader.0.take();
        Ok(())
    }

    pub async fn remove_server(&self, name: &str) -> bool {
        self.disconnected.lock().await.remove(name);
        // A `match` scrutinee's temporary lives until the match ends, so the
        // removal is bound first and the teardown runs with the table free.
        let server = self.servers.lock().await.remove(name);
        match server {
            Some(server) => {
                shutdown_server(server).await;
                true
            }
            None => false,
        }
    }

    pub async fn shutdown(&self) {
        self.disconnected.lock().await.clear();
        let map = std::mem::take(&mut *self.servers.lock().await);
        for (_, server) in map {
            shutdown_server(server).await;
        }
    }

    /// [`Self::shutdown`] under a deadline. Every server is cancelled before
    /// any is joined, so the budget can only shorten the wait, never skip a
    /// server the loop had not reached.
    pub async fn close_within(&self, budget: Duration) -> McpShutdownReport {
        self.disconnected.lock().await.clear();
        let map = std::mem::take(&mut *self.servers.lock().await);
        let mut joins = Vec::with_capacity(map.len());
        for (name, server) in map {
            joins.push((name, cancel_server(server).await));
        }
        rebon_tool::join_closed_servers(
            joins
                .into_iter()
                .map(|(name, reader)| {
                    (name, async move {
                        if let Some(handle) = reader {
                            let _ = handle.await;
                        }
                    })
                })
                .collect(),
            budget,
        )
        .await
    }

    pub async fn server_count(&self) -> usize {
        self.servers.lock().await.len()
    }

    async fn get_server(&self, name: &str) -> Option<Arc<SseServer>> {
        self.servers.lock().await.get(name).cloned()
    }
}

impl Drop for SseMcpClient {
    fn drop(&mut self) {
        if let Ok(servers) = self.servers.try_lock() {
            for server in servers.values() {
                if let Ok(mut handle) = server.reader_handle.try_lock() {
                    if let Some(handle) = handle.take() {
                        handle.abort();
                    }
                }
            }
        }
    }
}

#[async_trait]
impl McpClient for SseMcpClient {
    async fn call_tool(&self, call: McpToolCall) -> Result<McpToolResult, McpClientError> {
        let server = self
            .get_server(&call.server)
            .await
            .ok_or_else(|| McpClientError::UnknownServer(call.server.clone()))?;
        let response = send_request(
            &self.http,
            &server,
            "tools/call",
            json!({"name": call.name, "arguments": call.arguments}),
        )
        .await?;
        if let Some(err) = response.error {
            return Err(McpClientError::CallFailed(format!(
                "{} ({})",
                err.message, err.code
            )));
        }
        let result = response
            .result
            .ok_or_else(|| McpClientError::Transport("empty result".into()))?;
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let content = result.get("content").cloned().unwrap_or(result);
        Ok(McpToolResult {
            server: call.server,
            name: call.name,
            content,
            is_error,
        })
    }

    async fn list_tools(&self, server: &str) -> Option<Vec<String>> {
        self.list_tool_definitions(server)
            .await
            .map(|tools| tools.into_iter().map(|tool| tool.name).collect())
    }

    async fn list_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        let state = self.get_server(server).await?;
        if let Some(cached) = state.tools_cache.lock().await.clone() {
            return Some(cached);
        }
        let response = send_request(&self.http, &state, "tools/list", json!({}))
            .await
            .ok()?;
        let result = response.result?;
        let tools = parse_mcp_tool_definitions(&result);
        *state.tools_cache.lock().await = Some(tools.clone());
        Some(tools)
    }

    fn cached_tool_definitions(&self, server: &str) -> Option<Vec<McpToolDefinition>> {
        let state = if let Ok(guard) = self.servers.try_lock() {
            guard.get(server).cloned()
        } else {
            None
        }?;
        let cached = if let Ok(guard) = state.tools_cache.try_lock() {
            guard.clone()
        } else {
            None
        };
        cached
    }

    fn server_names(&self) -> Vec<String> {
        let mut names = if let Ok(guard) = self.servers.try_lock() {
            guard.keys().cloned().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        names.sort();
        names
    }

    fn drain_channel_notifications(&self) -> Vec<String> {
        Vec::new()
    }

    async fn shutdown_transport(&self) {
        self.shutdown().await;
    }

    async fn close_with_timeout(&self, budget: Duration) -> McpShutdownReport {
        self.close_within(budget).await
    }

    async fn disconnect_server(&self, name: &str) -> bool {
        // Parks the config rather than delegating to `remove_server`: the
        // receipt the front ends print says `/mcp reconnect` brings the server
        // back, and a reconnect needs the config to do that.
        let server = self.servers.lock().await.remove(name);
        match server {
            Some(server) => {
                self.disconnected
                    .lock()
                    .await
                    .insert(name.to_string(), server.config.clone());
                shutdown_server(server).await;
                true
            }
            None => false,
        }
    }

    /// Takes the old connection down first, so a reconnect never leaves two of
    /// the same server running. If bringing it back fails, the name stays
    /// parked and the error says why, and another `/mcp reconnect` may retry —
    /// better than a half-attached server that answers some calls and not
    /// others.
    async fn reconnect_server(&self, name: &str) -> Result<bool, McpClientError> {
        let live = self
            .servers
            .lock()
            .await
            .get(name)
            .map(|server| server.config.clone());
        let config = match live {
            Some(config) => config,
            None => match self.disconnected.lock().await.remove(name) {
                Some(config) => config,
                None => return Ok(false),
            },
        };
        self.remove_server(name).await;
        if let Err(error) = self.add_server(config.clone()).await {
            self.disconnected
                .lock()
                .await
                .insert(name.to_string(), config);
            return Err(error);
        }
        Ok(true)
    }
}

fn build_headers(configured: &[(String, String)]) -> Result<HeaderMap, McpClientError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    for (key, value) in configured {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|err| {
            McpClientError::Transport(format!("invalid HTTP header `{key}`: {err}"))
        })?;
        let value = HeaderValue::from_str(value).map_err(|err| {
            McpClientError::Transport(format!("invalid value for HTTP header `{key}`: {err}"))
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn sse_get_headers(base: &HeaderMap) -> HeaderMap {
    let mut headers = base.clone();
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.remove(CONTENT_TYPE);
    headers
}

async fn initialize_server(
    http: &reqwest::Client,
    server: &Arc<SseServer>,
) -> Result<(), McpClientError> {
    let init_params = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {"tools": {}},
        "clientInfo": {"name": "rebon", "version": env!("CARGO_PKG_VERSION")}
    });
    let response = send_request(http, server, "initialize", init_params).await?;
    if let Some(err) = response.error {
        return Err(McpClientError::Transport(format!(
            "initialize failed: {} ({})",
            err.message, err.code
        )));
    }
    send_notification(http, server, "notifications/initialized", json!({})).await
}

async fn send_request(
    http: &reqwest::Client,
    server: &Arc<SseServer>,
    method: &str,
    params: Value,
) -> Result<JsonRpcResponse, McpClientError> {
    let id = server.next_id.fetch_add(1, Ordering::Relaxed);
    let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let (tx, rx) = oneshot::channel();
    server.pending.lock().await.insert(id, tx);

    if let Err(err) = post_jsonrpc(http, server, &frame).await {
        server.pending.lock().await.remove(&id);
        return Err(err);
    }

    match tokio::time::timeout(server.config.effective_timeout(), rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(McpClientError::Transport(
            "SSE response channel closed".into(),
        )),
        Err(_) => {
            server.pending.lock().await.remove(&id);
            Err(McpClientError::Transport(format!(
                "timed out waiting for JSON-RPC response to `{method}`"
            )))
        }
    }
}

async fn send_notification(
    http: &reqwest::Client,
    server: &Arc<SseServer>,
    method: &str,
    params: Value,
) -> Result<(), McpClientError> {
    let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
    post_jsonrpc(http, server, &frame).await
}

async fn post_jsonrpc(
    http: &reqwest::Client,
    server: &Arc<SseServer>,
    frame: &Value,
) -> Result<(), McpClientError> {
    let post_url =
        server.post_url.lock().await.clone().ok_or_else(|| {
            McpClientError::Transport("SSE post endpoint is not established".into())
        })?;
    let response = http
        .post(post_url)
        .headers(server.headers.clone())
        .timeout(server.config.effective_timeout())
        .json(frame)
        .send()
        .await
        .map_err(|err| McpClientError::Transport(format!("SSE JSON-RPC post failed: {err}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(McpClientError::Transport(format!(
            "SSE MCP post returned {status}: {body}"
        )));
    }
    Ok(())
}

async fn run_sse_reader(
    server: Arc<SseServer>,
    mut response: reqwest::Response,
    mut endpoint_tx: Option<oneshot::Sender<String>>,
) {
    let base_url = server.config.url.clone();
    let mut parser = SseParser::default();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let text = String::from_utf8_lossy(&chunk);
                for event in parser.push_str(&text) {
                    handle_sse_event(&server, &base_url, event, &mut endpoint_tx).await;
                }
            }
            Ok(None) => {
                fail_all_pending(&server, "SSE stream closed").await;
                return;
            }
            Err(err) => {
                fail_all_pending(&server, &format!("SSE stream read failed: {err}")).await;
                return;
            }
        }
    }
}

async fn handle_sse_event(
    server: &Arc<SseServer>,
    base_url: &str,
    event: SseEvent,
    endpoint_tx: &mut Option<oneshot::Sender<String>>,
) {
    if event.event.as_deref() == Some("endpoint") {
        if let Some(tx) = endpoint_tx.take() {
            let _ = tx.send(resolve_endpoint(base_url, event.data.trim()));
        }
        return;
    }

    if event.data.trim().is_empty() {
        return;
    }
    let value = match serde_json::from_str::<Value>(&event.data) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(server = %server.config.name, error = %err, data = %event.data, "invalid MCP SSE JSON-RPC event");
            return;
        }
    };
    if value.get("method").is_some() && value.get("id").is_none() {
        tracing::debug!(server = %server.config.name, event = %value, "MCP SSE notification ignored");
        return;
    }
    let id = match value.get("id").and_then(|id| id.as_i64()) {
        Some(id) => id,
        None => return,
    };
    let parsed = parse_jsonrpc_response(value);
    if let Some(tx) = server.pending.lock().await.remove(&id) {
        let _ = tx.send(parsed);
    }
}

fn resolve_endpoint(base_url: &str, endpoint: &str) -> String {
    if let Ok(url) = reqwest::Url::parse(endpoint) {
        return url.to_string();
    }
    reqwest::Url::parse(base_url)
        .and_then(|base| base.join(endpoint))
        .map(|url| url.to_string())
        .unwrap_or_else(|_| endpoint.to_string())
}

/// Cancel one server: stop its reader and release every waiter.
///
/// The same split the stdio transport makes, for the same reason: nothing
/// here waits on the server, so a deadline cannot catch it half-done. What it
/// returns is the aborted reader, whose unwinding a caller out of budget may
/// stop waiting for.
async fn cancel_server(server: Arc<SseServer>) -> Option<JoinHandle<()>> {
    let handle = server.reader_handle.lock().await.take();
    if let Some(handle) = handle.as_ref() {
        handle.abort();
    }
    fail_all_pending(&server, "SSE MCP server shut down").await;
    handle
}

async fn shutdown_server(server: Arc<SseServer>) {
    if let Some(handle) = cancel_server(server).await {
        let _ = handle.await;
    }
}

async fn fail_all_pending(server: &Arc<SseServer>, message: &str) {
    let pending = std::mem::take(&mut *server.pending.lock().await);
    for (_, tx) in pending {
        let _ = tx.send(Err(McpClientError::Transport(message.to_string())));
    }
}

#[derive(Debug, Default)]
struct SseParser {
    buffer: String,
}

#[derive(Debug, PartialEq, Eq)]
struct SseEvent {
    event: Option<String>,
    data: String,
}

impl SseParser {
    fn push_str(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();
        while let Some((idx, sep_len)) = find_event_separator(&self.buffer) {
            let raw = self.buffer[..idx].to_string();
            self.buffer.drain(..idx + sep_len);
            if let Some(event) = parse_sse_event(&raw) {
                events.push(event);
            }
        }
        events
    }
}

fn find_event_separator(buffer: &str) -> Option<(usize, usize)> {
    [("\r\n\r\n", 4), ("\n\n", 2), ("\r\r", 2)]
        .iter()
        .filter_map(|(needle, len)| buffer.find(needle).map(|idx| (idx, *len)))
        .min_by_key(|(idx, _)| *idx)
}

fn parse_sse_event(raw: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data_lines = Vec::new();
    for raw_line in raw.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data_lines.push(value.to_string()),
            _ => {}
        }
    }
    if event.is_none() && data_lines.is_empty() {
        None
    } else {
        Some(SseEvent {
            event,
            data: data_lines.join("\n"),
        })
    }
}

fn parse_jsonrpc_response(value: Value) -> Result<JsonRpcResponse, McpClientError> {
    let object = value.as_object().ok_or_else(|| {
        McpClientError::Transport(format!("JSON-RPC response must be an object: {value}"))
    })?;
    match object.get("jsonrpc").and_then(|v| v.as_str()) {
        Some("2.0") => {}
        Some(other) => {
            return Err(McpClientError::Transport(format!(
                "unsupported JSON-RPC version `{other}`"
            )))
        }
        None => return Err(McpClientError::Transport("missing JSON-RPC version".into())),
    }
    let error = object
        .get("error")
        .map(|err| {
            let err_object = err.as_object().ok_or_else(|| {
                McpClientError::Transport(format!("JSON-RPC error must be an object: {err}"))
            })?;
            let code = err_object
                .get("code")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| {
                    McpClientError::Transport(format!("JSON-RPC error missing numeric code: {err}"))
                })?;
            let message = err_object
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    McpClientError::Transport(format!(
                        "JSON-RPC error missing string message: {err}"
                    ))
                })?
                .to_string();
            Ok(JsonRpcError { code, message })
        })
        .transpose()?;
    Ok(JsonRpcResponse {
        result: object.get("result").cloned(),
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{mpsc, oneshot};

    #[test]
    fn mcp_sse_parser_handles_endpoint_and_multiline_data() {
        let mut parser = SseParser::default();
        let events = parser.push_str(
            ": ping\n\nevent: endpoint\ndata: /message\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\n\
             data: \"id\":1,\"result\":{}}\n\n",
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("endpoint"));
        assert_eq!(events[0].data, "/message");
        assert_eq!(
            events[1].data,
            "{\"jsonrpc\":\"2.0\",\n\"id\":1,\"result\":{}}"
        );
    }

    #[test]
    fn mcp_sse_resolves_relative_endpoint_against_sse_url() {
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:1234/sse", "/message"),
            "http://127.0.0.1:1234/message"
        );
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:1234/api/sse", "message"),
            "http://127.0.0.1:1234/api/message"
        );
    }

    #[tokio::test]
    async fn mcp_sse_client_initializes_lists_and_calls_tools() {
        let (url, seen_rx) = start_legacy_sse_server().await;
        let client = SseMcpClient::new();
        client
            .add_server(SseServerConfig::new("legacy", url))
            .await
            .unwrap();

        assert_eq!(client.list_tools("legacy").await.unwrap(), vec!["search"]);
        let result = client
            .call_tool(McpToolCall {
                server: "legacy".to_string(),
                name: "search".to_string(),
                arguments: json!({"q":"abc"}),
            })
            .await
            .unwrap();
        assert_eq!(result.content[0]["text"], "ok");

        client.shutdown().await;
        let mut seen = Vec::new();
        let mut seen_rx = seen_rx;
        while let Ok(method) = seen_rx.try_recv() {
            seen.push(method);
        }
        assert_eq!(
            seen,
            vec![
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn erased_shutdown_closes_sse_reader_and_forgets_reconnect_config() {
        let (url, _seen) = start_legacy_sse_server().await;
        let client = SseMcpClient::new();
        client
            .add_server(SseServerConfig::new("legacy", url))
            .await
            .unwrap();
        assert!(client.list_tool_definitions("legacy").await.is_some());
        let server = client.get_server("legacy").await.unwrap();
        let reader = server
            .reader_handle
            .lock()
            .await
            .as_ref()
            .unwrap()
            .abort_handle();
        let erased: &dyn McpClient = &client;
        erased.shutdown_transport().await;
        assert!(reader.is_finished());
        assert_eq!(client.server_count().await, 0);
        assert!(erased.cached_tool_definitions("legacy").is_none());
        client
            .disconnected
            .lock()
            .await
            .insert("legacy".into(), server.config.clone());
        erased.shutdown_transport().await;
        assert!(!erased.reconnect_server("legacy").await.unwrap());
    }

    #[tokio::test]
    async fn mcp_sse_add_server_cleans_up_reader_when_endpoint_timeout() {
        let (url, disconnected_rx) = start_sse_server_without_endpoint().await;
        let client = SseMcpClient::new();
        let mut config = SseServerConfig::new("no-endpoint", url);
        config.request_timeout = Some(Duration::from_millis(50));

        let err = client.add_server(config).await.unwrap_err();

        assert!(matches!(
            err,
            McpClientError::Transport(message)
                if message.contains("timed out waiting for SSE endpoint event")
                    || message.contains("SSE stream closed before endpoint event")
        ));
        assert!(!client.server_names().contains(&"no-endpoint".to_string()));
        assert_eq!(client.server_count().await, 0);
        tokio::time::timeout(Duration::from_secs(2), disconnected_rx)
            .await
            .expect("server did not observe SSE disconnect")
            .expect("disconnect notification dropped");
    }

    async fn start_sse_server_without_endpoint() -> (String, oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (disconnected_tx, disconnected_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            let headers =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
            stream.write_all(headers.as_bytes()).await.unwrap();
            let mut buffer = [0_u8; 1];
            let read = stream.read(&mut buffer).await.unwrap();
            assert_eq!(read, 0);
            let _ = disconnected_tx.send(());
        });
        (format!("http://{addr}/sse"), disconnected_rx)
    }

    async fn start_legacy_sse_server() -> (String, mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (sse_stream, _) = listener.accept().await.unwrap();
            let (post_tx, post_rx) = mpsc::unbounded_channel::<Value>();
            let sse_task = tokio::spawn(write_sse_stream(sse_stream, addr, post_rx));
            for _ in 0..4 {
                let (post_stream, _) = listener.accept().await.unwrap();
                let tx = post_tx.clone();
                let seen_tx = seen_tx.clone();
                tokio::spawn(async move {
                    let request = read_request(post_stream).await;
                    let method = request["method"].as_str().unwrap().to_string();
                    seen_tx.send(method).unwrap();
                    tx.send(request).unwrap();
                });
            }
            sse_task.await.unwrap();
        });
        (format!("http://{addr}/sse"), seen_rx)
    }

    async fn write_sse_stream(
        mut stream: TcpStream,
        addr: SocketAddr,
        mut post_rx: mpsc::UnboundedReceiver<Value>,
    ) {
        read_http_headers(&mut stream).await;
        let headers =
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
        stream.write_all(headers.as_bytes()).await.unwrap();
        stream
            .write_all(format!("event: endpoint\ndata: http://{addr}/message\n\n").as_bytes())
            .await
            .unwrap();
        while let Some(request) = post_rx.recv().await {
            let method = request["method"].as_str().unwrap();
            let response = match method {
                "initialize" => Some(
                    json!({"jsonrpc":"2.0","id":request["id"],"result":{"protocolVersion":"2024-11-05","capabilities":{}}}),
                ),
                "notifications/initialized" => None,
                "tools/list" => Some(
                    json!({"jsonrpc":"2.0","id":request["id"],"result":{"tools":[{"name":"search"}]}}),
                ),
                "tools/call" => Some(
                    json!({"jsonrpc":"2.0","id":request["id"],"result":{"content":[{"type":"text","text":"ok"}]}}),
                ),
                other => panic!("unexpected method {other}"),
            };
            if let Some(response) = response {
                stream
                    .write_all(format!("event: message\ndata: {response}\n\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
    }

    async fn read_http_headers(stream: &mut TcpStream) {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0);
            bytes.extend_from_slice(&buffer[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                return;
            }
        }
    }

    async fn read_request(mut stream: TcpStream) -> Value {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end;
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0);
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(pos) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = pos + 4;
                break;
            }
        }
        let header_text = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let content_length = header_text
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0);
            bytes.extend_from_slice(&buffer[..read]);
        }
        stream
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap()
    }
}
