use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Response;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::mcp::parse_mcp_tool_definitions;
use rebon_tool::{McpClient, McpClientError, McpToolCall, McpToolDefinition, McpToolResult};

const MCP_SESSION_ID: HeaderName = HeaderName::from_static("mcp-session-id");

#[derive(Debug, Clone)]
pub struct HttpServerConfig {
    pub name: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub request_timeout: Option<Duration>,
}

impl HttpServerConfig {
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
struct HttpServer {
    config: HttpServerConfig,
    headers: HeaderMap,
    session_id: Mutex<Option<HeaderValue>>,
    next_id: AtomicI64,
    tools_cache: Mutex<Option<Vec<McpToolDefinition>>>,
}

#[derive(Debug, Clone, Default)]
pub struct HttpMcpClient {
    http: reqwest::Client,
    servers: Arc<Mutex<HashMap<String, Arc<HttpServer>>>>,
    /// Configs parked by `disconnect_server`, so `/mcp reconnect` can bring a
    /// disconnected server back. `remove_server` stays a true removal.
    disconnected: Arc<Mutex<HashMap<String, HttpServerConfig>>>,
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

impl HttpMcpClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            servers: Arc::new(Mutex::new(HashMap::new())),
            disconnected: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn add_server(&self, config: HttpServerConfig) -> Result<(), McpClientError> {
        let headers = build_headers(&config.headers)?;
        let server = Arc::new(HttpServer {
            config: config.clone(),
            headers,
            session_id: Mutex::new(None),
            next_id: AtomicI64::new(1),
            tools_cache: Mutex::new(None),
        });

        initialize_server(&self.http, &server).await?;
        self.disconnected.lock().await.remove(&config.name);
        self.servers.lock().await.insert(config.name, server);
        Ok(())
    }

    /// Forget one server. There is no child process and no reader task behind
    /// an HTTP server — each call is its own request — so dropping the entry is
    /// the whole teardown.
    pub async fn remove_server(&self, name: &str) -> bool {
        self.disconnected.lock().await.remove(name);
        self.servers.lock().await.remove(name).is_some()
    }

    pub async fn server_count(&self) -> usize {
        self.servers.lock().await.len()
    }

    async fn get_server(&self, name: &str) -> Option<Arc<HttpServer>> {
        self.servers.lock().await.get(name).cloned()
    }
}

#[async_trait]
impl McpClient for HttpMcpClient {
    async fn call_tool(&self, call: McpToolCall) -> Result<McpToolResult, McpClientError> {
        let server = self
            .get_server(&call.server)
            .await
            .ok_or_else(|| McpClientError::UnknownServer(call.server.clone()))?;
        let response = send_request(
            &self.http,
            &server,
            "tools/call",
            json!({
                "name": call.name,
                "arguments": call.arguments,
            }),
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

    async fn shutdown_transport(&self) {
        self.servers.lock().await.clear();
        self.disconnected.lock().await.clear();
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
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
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

async fn initialize_server(
    http: &reqwest::Client,
    server: &Arc<HttpServer>,
) -> Result<(), McpClientError> {
    let init_params = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {"tools": {}},
        "clientInfo": {
            "name": "rebon",
            "version": env!("CARGO_PKG_VERSION"),
        }
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
    server: &Arc<HttpServer>,
    method: &str,
    params: Value,
) -> Result<JsonRpcResponse, McpClientError> {
    let id = server.next_id.fetch_add(1, Ordering::Relaxed);
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let value = post_jsonrpc_request(http, server, &frame).await?;
    parse_jsonrpc_response(value)
}

async fn send_notification(
    http: &reqwest::Client,
    server: &Arc<HttpServer>,
    method: &str,
    params: Value,
) -> Result<(), McpClientError> {
    let frame = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    post_jsonrpc_notification(http, server, &frame).await
}

async fn post_jsonrpc_request(
    http: &reqwest::Client,
    server: &Arc<HttpServer>,
    frame: &Value,
) -> Result<Value, McpClientError> {
    let response = send_http_post(http, server, frame).await?;
    read_jsonrpc_body(response, false).await
}

async fn post_jsonrpc_notification(
    http: &reqwest::Client,
    server: &Arc<HttpServer>,
    frame: &Value,
) -> Result<(), McpClientError> {
    let response = send_http_post(http, server, frame).await?;
    let _ = read_jsonrpc_body(response, true).await?;
    Ok(())
}

async fn send_http_post(
    http: &reqwest::Client,
    server: &Arc<HttpServer>,
    frame: &Value,
) -> Result<Response, McpClientError> {
    let mut headers = server.headers.clone();
    if let Some(session_id) = server.session_id.lock().await.clone() {
        headers.insert(MCP_SESSION_ID, session_id);
    }

    let response = http
        .post(&server.config.url)
        .headers(headers)
        .timeout(server.config.effective_timeout())
        .json(frame)
        .send()
        .await
        .map_err(|err| McpClientError::Transport(format!("http post failed: {err}")))?;

    if let Some(session_id) = response.headers().get(MCP_SESSION_ID).cloned() {
        *server.session_id.lock().await = Some(session_id);
    }

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(McpClientError::Transport(format!(
            "HTTP MCP server returned {status}: {body}"
        )));
    }

    Ok(response)
}

async fn read_jsonrpc_body(response: Response, allow_empty: bool) -> Result<Value, McpClientError> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase());
    let body = response.text().await.map_err(|err| {
        McpClientError::Transport(format!("invalid JSON-RPC response body: {err}"))
    })?;

    if body.trim().is_empty() {
        if allow_empty {
            return Ok(Value::Null);
        }
        return Err(McpClientError::Transport("empty JSON-RPC response".into()));
    }

    if content_type
        .as_deref()
        .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        parse_sse_jsonrpc_response(&body)
    } else {
        serde_json::from_str::<Value>(&body).map_err(|err| {
            McpClientError::Transport(format!("invalid JSON-RPC response: {err}; body: {body}"))
        })
    }
}

fn parse_sse_jsonrpc_response(body: &str) -> Result<Value, McpClientError> {
    for event in body.split("\n\n") {
        let mut data_lines = Vec::new();
        for raw_line in event.lines() {
            let line = raw_line.trim_end_matches('\r');
            if line.starts_with(':') || line.is_empty() {
                continue;
            }
            if let Some(data) = line.strip_prefix("data:") {
                data_lines.push(data.strip_prefix(' ').unwrap_or(data));
            }
        }

        if data_lines.is_empty() {
            continue;
        }

        let data = data_lines.join("\n");
        let value = serde_json::from_str::<Value>(&data).map_err(|err| {
            McpClientError::Transport(format!("invalid JSON-RPC SSE data: {err}; data: {data}"))
        })?;
        if value.get("jsonrpc").is_some()
            || value.get("result").is_some()
            || value.get("error").is_some()
        {
            return Ok(value);
        }
    }

    Err(McpClientError::Transport(
        "text/event-stream response did not contain JSON-RPC data".into(),
    ))
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

    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    if has_result == has_error {
        return Err(McpClientError::Transport(
            "JSON-RPC response must contain exactly one of result or error".into(),
        ));
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
    use rebon_tool::McpClient;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    #[derive(Debug)]
    struct TestRequest {
        headers: HashMap<String, String>,
        body: Value,
    }

    fn read_http_request(stream: &mut TcpStream) -> TestRequest {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end;
        loop {
            let read = std::io::Read::read(stream, &mut buffer).unwrap();
            assert_ne!(read, 0, "connection closed before headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(pos) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = pos + 4;
                break;
            }
        }

        let header_text = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let mut headers = HashMap::new();
        for line in header_text.lines().skip(1) {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let content_length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let read = std::io::Read::read(stream, &mut buffer).unwrap();
            assert_ne!(read, 0, "connection closed before body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        let body = serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
        TestRequest { headers, body }
    }

    fn write_response(stream: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &str) {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (key, value) in headers {
            response.push_str(key);
            response.push_str(": ");
            response.push_str(value);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        response.push_str(body);
        stream.write_all(response.as_bytes()).unwrap();
    }

    fn start_test_server<F>(requests: usize, handler: F) -> String
    where
        F: Fn(TestRequest) -> (String, Vec<(String, String)>, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handler = Arc::new(handler);
        thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                let (status, headers, body) = handler(request);
                let header_refs = headers
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect::<Vec<_>>();
                write_response(&mut stream, &status, &header_refs, &body);
            }
        });
        url
    }

    #[test]
    fn parses_jsonrpc_tool_response_content_and_error_flag() {
        let response = parse_jsonrpc_response(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {"content": [{"type": "text", "text": "ok"}], "isError": true}
        }))
        .unwrap();

        assert!(response.error.is_none());
        let result = response.result.unwrap();
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "ok");
    }

    #[test]
    fn parses_jsonrpc_error_response() {
        let response = parse_jsonrpc_response(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": {"code": -32000, "message": "boom"}
        }))
        .unwrap();

        let err = response.error.unwrap();
        assert_eq!(err.code, -32000);
        assert_eq!(err.message, "boom");
    }

    #[test]
    fn parses_sse_jsonrpc_response_with_multiline_data_and_ignores_comments() {
        let value = parse_sse_jsonrpc_response(
            ": keepalive\n\
             event: message\n\
             data: {\"jsonrpc\":\"2.0\",\n\
             data: \"id\":4,\"result\":{\"tools\":[]}}\n\n",
        )
        .unwrap();

        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 4);
        assert_eq!(value["result"]["tools"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn rejects_malformed_jsonrpc_response() {
        let err = parse_jsonrpc_response(json!({"jsonrpc": "2.0", "result": {}, "error": {}}))
            .unwrap_err();
        assert!(err.to_string().contains("exactly one of result or error"));
    }

    #[tokio::test]
    async fn persists_session_id_for_initialized_list_and_call() {
        let seen_methods = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_methods_for_server = seen_methods.clone();
        let url = start_test_server(4, move |request| {
            let method = request.body["method"].as_str().unwrap().to_string();
            if method != "initialize" {
                assert_eq!(
                    request.headers.get("mcp-session-id").map(String::as_str),
                    Some("abc")
                );
            }
            seen_methods_for_server.lock().unwrap().push(method.clone());

            match method.as_str() {
                "initialize" => (
                    "200 OK".to_string(),
                    vec![
                        ("Content-Type".to_string(), "application/json".to_string()),
                        ("Mcp-Session-Id".to_string(), "abc".to_string()),
                    ],
                    json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mock","version":"1"}}}).to_string(),
                ),
                "notifications/initialized" => (
                    "202 Accepted".to_string(),
                    Vec::new(),
                    String::new(),
                ),
                "tools/list" => (
                    "200 OK".to_string(),
                    vec![("Content-Type".to_string(), "application/json".to_string())],
                    json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"tools":[{"name":"search"}]}}).to_string(),
                ),
                "tools/call" => (
                    "200 OK".to_string(),
                    vec![("Content-Type".to_string(), "application/json".to_string())],
                    json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"content":[{"type":"text","text":"ok"}]}}).to_string(),
                ),
                other => panic!("unexpected method {other}"),
            }
        });

        let client = HttpMcpClient::new();
        client
            .add_server(HttpServerConfig::new("patent-search", url))
            .await
            .unwrap();
        assert_eq!(
            client.list_tools("patent-search").await.unwrap(),
            vec!["search"]
        );
        let result = client
            .call_tool(McpToolCall {
                server: "patent-search".to_string(),
                name: "search".to_string(),
                arguments: json!({"q":"abc"}),
            })
            .await
            .unwrap();
        assert_eq!(result.content[0]["text"], "ok");
        assert_eq!(
            *seen_methods.lock().unwrap(),
            vec![
                "initialize".to_string(),
                "notifications/initialized".to_string(),
                "tools/list".to_string(),
                "tools/call".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn initialized_notification_accepts_empty_204_response() {
        let url = start_test_server(2, move |request| {
            match request.body["method"].as_str().unwrap() {
                "initialize" => (
                    "200 OK".to_string(),
                    vec![("Content-Type".to_string(), "application/json".to_string())],
                    json!({"jsonrpc":"2.0","id":request.body["id"],"result":{}}).to_string(),
                ),
                "notifications/initialized" => {
                    ("204 No Content".to_string(), Vec::new(), String::new())
                }
                other => panic!("unexpected method {other}"),
            }
        });

        let client = HttpMcpClient::new();
        client
            .add_server(HttpServerConfig::new("empty-notification", url))
            .await
            .unwrap();
        assert_eq!(client.server_count().await, 1);
    }

    #[tokio::test]
    async fn parses_sse_tools_list_response() {
        let url = start_test_server(3, move |request| {
            match request.body["method"].as_str().unwrap() {
                "initialize" => (
                    "200 OK".to_string(),
                    vec![("Content-Type".to_string(), "application/json".to_string())],
                    json!({"jsonrpc":"2.0","id":request.body["id"],"result":{}}).to_string(),
                ),
                "notifications/initialized" => {
                    ("202 Accepted".to_string(), Vec::new(), String::new())
                }
                "tools/list" => (
                    "200 OK".to_string(),
                    vec![("Content-Type".to_string(), "text/event-stream".to_string())],
                    format!(
                        ": ping\n\nevent: message\ndata: {}\n\n",
                        json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"tools":[{"name":"sse-tool"}]}})
                    ),
                ),
                other => panic!("unexpected method {other}"),
            }
        });

        let client = HttpMcpClient::new();
        client
            .add_server(HttpServerConfig::new("sse", url))
            .await
            .unwrap();
        assert_eq!(client.list_tools("sse").await.unwrap(), vec!["sse-tool"]);
    }
}
