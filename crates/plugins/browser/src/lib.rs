mod bridge;
mod tools;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use bridge::{BrowserBridge, BrowserBridgeOptions};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

pub use bridge::{BROWSER_PROTOCOL_VERSION, BROWSER_WEBSOCKET_PROTOCOL, DEFAULT_BROWSER_PORT};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_SERVER_NAME: &str = "rebon-browser";
const MAX_TOOL_OUTPUT_CHARS: usize = 128_000;

#[derive(Debug, Clone)]
pub struct BrowserServerOptions {
    pub config_home: PathBuf,
    pub extension_dir: PathBuf,
    pub screenshot_root: PathBuf,
    pub port: u16,
    pub request_timeout: Duration,
}

impl BrowserServerOptions {
    pub fn new(config_home: impl Into<PathBuf>, extension_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_home: config_home.into(),
            extension_dir: extension_dir.into(),
            screenshot_root: screenshot_session_root(),
            port: DEFAULT_BROWSER_PORT,
            request_timeout: Duration::from_secs(30),
        }
    }

    fn bridge_options(&self) -> BrowserBridgeOptions {
        BrowserBridgeOptions {
            config_home: self.config_home.clone(),
            extension_dir: self.extension_dir.clone(),
            port: self.port,
            request_timeout: self.request_timeout,
        }
    }
}

fn screenshot_session_root() -> PathBuf {
    let mut random = [0u8; 8];
    let suffix = if getrandom::getrandom(&mut random).is_ok() {
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string()
    };
    std::env::temp_dir()
        .join("rebon")
        .join("browser")
        .join(format!("session-{suffix}"))
}

fn is_screenshot_session_root(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("session-"))
        && path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("browser")
        && path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("rebon")
}

fn cleanup_stale_screenshot_sessions(root: &Path) {
    let Some(parent) = root.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == root || !is_screenshot_session_root(&path) {
            continue;
        }
        let is_stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .and_then(|modified| modified.elapsed().map_err(io::Error::other))
            .is_ok_and(|age| age >= Duration::from_secs(24 * 60 * 60));
        if is_stale {
            let _ = fs::remove_dir_all(path);
        }
    }
}

pub async fn run_server(options: BrowserServerOptions) -> anyhow::Result<()> {
    run_mcp_server(tokio::io::stdin(), tokio::io::stdout(), options).await
}

struct McpServer {
    options: BrowserServerOptions,
    bridge: Option<BrowserBridge>,
    cleanup_screenshot_root: bool,
}

impl McpServer {
    fn new(options: BrowserServerOptions) -> Self {
        let cleanup_screenshot_root = is_screenshot_session_root(&options.screenshot_root);
        if cleanup_screenshot_root {
            cleanup_stale_screenshot_sessions(&options.screenshot_root);
        }
        Self {
            options,
            bridge: None,
            cleanup_screenshot_root,
        }
    }

    async fn handle_message(&mut self, message: Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = match message.get("method").and_then(Value::as_str) {
            Some(method) => method,
            None => return id.map(|id| jsonrpc_error(id, -32600, "invalid JSON-RPC request")),
        };
        match method {
            "initialize" => id.map(|id| jsonrpc_result(id, initialize_result())),
            "notifications/initialized" | "notifications/cancelled" => None,
            "tools/list" => id.map(|id| jsonrpc_result(id, tools::tool_definitions())),
            "tools/call" => match id {
                Some(id) => Some(
                    self.handle_tool_call(id, message.get("params").cloned())
                        .await,
                ),
                None => None,
            },
            _ => id.map(|id| jsonrpc_error(id, -32601, format!("unknown method `{method}`"))),
        }
    }

    async fn handle_tool_call(&mut self, id: Value, params: Option<Value>) -> Value {
        let params = params.unwrap_or_else(|| json!({}));
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return jsonrpc_error(id, -32602, "tools/call requires string `name`");
        };
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if let Err(err) = tools::validate_arguments(name, &arguments) {
            return jsonrpc_result(id, tool_result(json!({ "error": err.to_string() }), true));
        }

        let result = self.call_tool(name, arguments).await;
        match result {
            Ok(value) => jsonrpc_result(id, tool_result(value, false)),
            Err(err) => jsonrpc_result(
                id,
                tool_result(json!({ "error": format!("{err:#}") }), true),
            ),
        }
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> anyhow::Result<Value> {
        let screenshot_root = self.options.screenshot_root.clone();
        let bridge = self.ensure_bridge().await?;
        if name == "status" {
            let mut status = bridge.status().await?;
            let authenticated = status
                .get("authenticated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if authenticated {
                match bridge.request("status", json!({})).await {
                    Ok(extension) => {
                        status["extension"] = extension;
                    }
                    Err(err) => {
                        status["extension_error"] = Value::String(err.to_string());
                    }
                }
            }
            return Ok(status);
        }

        let mut result = bridge.request(name, arguments).await?;
        tools::materialize_screenshots(name, &mut result, &screenshot_root)?;
        Ok(result)
    }

    async fn ensure_bridge(&mut self) -> anyhow::Result<&BrowserBridge> {
        if self.bridge.is_none() {
            self.bridge = Some(BrowserBridge::start(self.options.bridge_options()).await?);
        }
        Ok(self
            .bridge
            .as_ref()
            .expect("browser bridge was initialized"))
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        if self.cleanup_screenshot_root {
            let _ = fs::remove_dir_all(&self.options.screenshot_root);
        }
    }
}

async fn run_mcp_server<R, W>(
    input: R,
    output: W,
    options: BrowserServerOptions,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut server = McpServer::new(options);
    let output = Arc::new(Mutex::new(output));
    let mut lines = BufReader::new(input).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let response = match serde_json::from_str::<Value>(trimmed) {
                    Ok(message) => server.handle_message(message).await,
                    Err(err) => Some(jsonrpc_error(
                        Value::Null,
                        -32700,
                        format!("parse error: {err}"),
                    )),
                };
                if let Some(response) = response {
                    write_ndjson(&output, &response).await?;
                }
            }
            Ok(None) => break,
            Err(err) => return Err(err).context("failed to read browser MCP request"),
        }
    }
    Ok(())
}

async fn write_ndjson<W>(output: &Arc<Mutex<W>>, message: &Value) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let mut output = output.lock().await;
    output.write_all(&bytes).await?;
    output.flush().await
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": MCP_SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let mut text =
        serde_json::to_string_pretty(&value).expect("serde_json::Value always serializes");
    if text.chars().count() > MAX_TOOL_OUTPUT_CHARS {
        text = text.chars().take(MAX_TOOL_OUTPUT_CHARS).collect();
        text.push_str("\n… browser tool output truncated");
    }
    json!({
        "content": [{
            "type": "text",
            "text": text
        }],
        "isError": is_error
    })
}

fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn jsonrpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> BrowserServerOptions {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.keep();
        BrowserServerOptions {
            config_home: path.join("config"),
            extension_dir: path.join("extension"),
            screenshot_root: path.join("screenshots"),
            port: 0,
            request_timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn default_screenshot_roots_are_private_per_server_session() {
        let first = BrowserServerOptions::new("config", "extension");
        let second = BrowserServerOptions::new("config", "extension");
        assert_ne!(first.screenshot_root, second.screenshot_root);
        assert!(is_screenshot_session_root(&first.screenshot_root));
    }

    #[test]
    fn dropping_server_removes_its_screenshot_session() {
        let options = BrowserServerOptions::new("config", "extension");
        fs::create_dir_all(&options.screenshot_root).unwrap();
        fs::write(options.screenshot_root.join("capture.png"), b"png").unwrap();
        let root = options.screenshot_root.clone();
        drop(McpServer::new(options));
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn initialize_and_tool_listing_do_not_start_the_browser_bridge() {
        let mut server = McpServer::new(options());
        let initialized = server
            .handle_message(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }))
            .await
            .unwrap();
        assert_eq!(initialized["result"]["serverInfo"]["name"], MCP_SERVER_NAME);
        let tools = server
            .handle_message(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .await
            .unwrap();
        assert!(tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "observe"));
        assert!(server.bridge.is_none());
    }

    #[tokio::test]
    async fn invalid_tool_arguments_return_an_mcp_tool_error() {
        let mut server = McpServer::new(options());
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "navigate",
                    "arguments": { "url": "file:///secret" }
                }
            }))
            .await
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("http or https"));
        assert!(server.bridge.is_none());
    }

    #[tokio::test]
    async fn ndjson_server_answers_multiple_requests() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
        let mut output = Vec::new();
        run_mcp_server(&input[..], &mut output, options())
            .await
            .unwrap();
        let lines = String::from_utf8(output).unwrap();
        assert_eq!(lines.lines().count(), 2);
    }
}
