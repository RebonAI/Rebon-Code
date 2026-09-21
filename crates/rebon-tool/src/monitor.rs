//! Shared monitor ownership, cancellation, and WebSocket transport runtime.
//! Input parsing, network policy, and the model-facing tool belong to the monitor plugin.

use crate::shell_process::random_monitor_id;
use crate::{
    MonitorEventDisposition, MonitorTaskCompletion, MonitorTaskCompletionStatus, MonitorTaskSource,
    MonitorTaskSpec, TaskRuntimeController, ToolContext,
};
use futures_util::{SinkExt, StreamExt};
use rebon_tools_core::{ToolError, ToolId, ToolResult};
use rebon_types::PromptCancel;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderValue},
    Message,
};
use url::Url;

/// Monitor registry stored in the shared tool context.
#[derive(Clone, Default)]
pub struct MonitorContext {
    pub registry: Option<Arc<MonitorRegistry>>,
}

pub const MONITOR_TOOL_NAME: &str = "Monitor";
const INVALID_INPUT_CODE: i64 = 400;
const MAX_RUNNING_WEBSOCKETS_PER_OWNER: usize = 16;

/// A parsed target passed by the monitor plugin after its network-policy checks.
#[derive(Debug, Clone)]
pub struct WebSocketTarget {
    pub url: Url,
    pub host: String,
    pub port: u16,
    pub subprotocols: Vec<String>,
    pub redacted_target: String,
}

#[derive(Clone)]
pub struct MonitorRegistry {
    inner: Arc<MonitorRegistryInner>,
}

impl Default for MonitorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MonitorRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MonitorRegistry")
            .field("entries", &lock(&self.inner.entries).len())
            .finish()
    }
}

impl MonitorRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MonitorRegistryInner::default()),
        }
    }

    /// Start a monitor using the addresses already validated by the caller's network policy.
    pub fn spawn_websocket(
        &self,
        context: &ToolContext,
        controller: Arc<dyn TaskRuntimeController>,
        target: WebSocketTarget,
        addresses: Vec<SocketAddr>,
        description: String,
        timeout_ms: Option<u64>,
    ) -> ToolResult<String> {
        let owner = MonitorOwner::from_context(context)?;
        let session_id = owner
            .session_id
            .clone()
            .ok_or_else(|| invalid_input("WebSocket task runtime requires a session_id"))?;
        let mut entries = lock(&self.inner.entries);
        let running = entries
            .values()
            .filter(|entry| entry.owner == owner)
            .count();
        if running >= MAX_RUNNING_WEBSOCKETS_PER_OWNER {
            return Err(execution_error(format!(
                "WebSocket monitor limit reached ({MAX_RUNNING_WEBSOCKETS_PER_OWNER})"
            )));
        }
        let task_id = loop {
            let candidate =
                random_monitor_id().map_err(|error| execution_error(error.to_string()))?;
            if !entries.contains_key(&candidate) {
                break candidate;
            }
        };
        let cancel = PromptCancel::new();
        entries.insert(
            task_id.clone(),
            MonitorRegistryEntry {
                owner: owner.clone(),
                cancel: cancel.clone(),
            },
        );
        drop(entries);
        controller.monitor_started(
            &session_id,
            MonitorTaskSpec {
                task_id: task_id.clone(),
                description,
                source: MonitorTaskSource::WebSocket,
                redacted_target: target.redacted_target.clone(),
                session_id: Some(session_id.clone()),
                agent_id: owner.agent_id,
                started_at_ms: now_ms(),
            },
            cancel.clone(),
        );
        let weak = Arc::downgrade(&self.inner);
        let runtime_task_id = task_id.clone();
        tokio::spawn(async move {
            let completion = run_websocket_monitor(
                &session_id,
                runtime_task_id.clone(),
                target,
                addresses,
                timeout_ms,
                cancel,
                controller.clone(),
            )
            .await;
            if let Some(inner) = weak.upgrade() {
                lock(&inner.entries).remove(&runtime_task_id);
            }
            controller.monitor_finished(&session_id, completion);
        });
        Ok(task_id)
    }
}

#[derive(Default)]
struct MonitorRegistryInner {
    entries: Mutex<HashMap<String, MonitorRegistryEntry>>,
}

impl Drop for MonitorRegistryInner {
    fn drop(&mut self) {
        let entries = self
            .entries
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entry in entries.values() {
            entry.cancel.cancel();
        }
    }
}

struct MonitorRegistryEntry {
    owner: MonitorOwner,
    cancel: PromptCancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MonitorOwner {
    session_id: Option<String>,
    agent_id: Option<String>,
}

impl MonitorOwner {
    fn from_context(context: &ToolContext) -> ToolResult<Self> {
        let owner = Self {
            session_id: context.session_id().map(str::to_owned),
            agent_id: context.agent_id().map(str::to_owned),
        };
        if owner.session_id.is_none() && owner.agent_id.is_none() {
            Err(invalid_input(
                "Monitor requires a session_id or agent_id owner",
            ))
        } else {
            Ok(owner)
        }
    }
}

async fn run_websocket_monitor(
    session_id: &str,
    task_id: String,
    target: WebSocketTarget,
    addresses: Vec<SocketAddr>,
    timeout_ms: Option<u64>,
    cancel: PromptCancel,
    controller: Arc<dyn TaskRuntimeController>,
) -> MonitorTaskCompletion {
    let outcome = if let Some(timeout_ms) = timeout_ms {
        tokio::select! {
            outcome = run_websocket_stream(session_id, &task_id, &target, &addresses, &cancel, controller.as_ref()) => outcome,
            _ = sleep(Duration::from_millis(timeout_ms)) => WebSocketOutcome::TimedOut,
        }
    } else {
        run_websocket_stream(
            session_id,
            &task_id,
            &target,
            &addresses,
            &cancel,
            controller.as_ref(),
        )
        .await
    };
    let (status, error) = match outcome {
        WebSocketOutcome::Closed => (MonitorTaskCompletionStatus::Closed, None),
        WebSocketOutcome::Stopped => (MonitorTaskCompletionStatus::Stopped, None),
        WebSocketOutcome::TimedOut => (
            MonitorTaskCompletionStatus::TimedOut,
            Some("WebSocket monitor timed out".to_string()),
        ),
        WebSocketOutcome::AutoStopped => (
            MonitorTaskCompletionStatus::AutoStopped,
            Some("WebSocket monitor produced too many events".to_string()),
        ),
        WebSocketOutcome::Failed(error) => (MonitorTaskCompletionStatus::Failed, Some(error)),
    };
    MonitorTaskCompletion {
        task_id,
        status,
        completed_at_ms: now_ms(),
        exit_code: None,
        stderr: None,
        error,
    }
}

enum WebSocketOutcome {
    Closed,
    Stopped,
    TimedOut,
    AutoStopped,
    Failed(String),
}

async fn run_websocket_stream(
    session_id: &str,
    task_id: &str,
    target: &WebSocketTarget,
    addresses: &[SocketAddr],
    cancel: &PromptCancel,
    controller: &dyn TaskRuntimeController,
) -> WebSocketOutcome {
    let stream = tokio::select! {
        result = connect_validated_address(addresses) => match result {
            Ok(stream) => stream,
            Err(error) => return WebSocketOutcome::Failed(format!("WebSocket connection failed: {error}")),
        },
        _ = cancel.notified() => return WebSocketOutcome::Stopped,
    };
    let mut request = match target.url.as_str().into_client_request() {
        Ok(request) => request,
        Err(error) => {
            return WebSocketOutcome::Failed(format!("WebSocket request failed: {error}"))
        }
    };
    if !target.subprotocols.is_empty() {
        let value = target.subprotocols.join(", ");
        let header = match HeaderValue::from_str(&value) {
            Ok(header) => header,
            Err(_) => {
                return WebSocketOutcome::Failed("invalid WebSocket subprotocol header".to_string())
            }
        };
        request.headers_mut().insert(SEC_WEBSOCKET_PROTOCOL, header);
    }
    let (mut socket, _) = tokio::select! {
        result = tokio_tungstenite::client_async_tls(request, stream) => match result {
            Ok(connected) => connected,
            Err(error) => return WebSocketOutcome::Failed(format!("WebSocket handshake failed: {error}")),
        },
        _ = cancel.notified() => return WebSocketOutcome::Stopped,
    };
    loop {
        tokio::select! {
            _ = cancel.notified() => {
                let _ = socket.close(None).await;
                return WebSocketOutcome::Stopped;
            }
            message = socket.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if controller.monitor_event(session_id, task_id, text.to_string()) == MonitorEventDisposition::AutoStop {
                        let _ = socket.close(None).await;
                        return WebSocketOutcome::AutoStopped;
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    let event = format!("[{}-byte binary WebSocket frame]", bytes.len());
                    if controller.monitor_event(session_id, task_id, event) == MonitorEventDisposition::AutoStop {
                        let _ = socket.close(None).await;
                        return WebSocketOutcome::AutoStopped;
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    if let Err(error) = socket.send(Message::Pong(payload)).await {
                        return WebSocketOutcome::Failed(format!("WebSocket pong failed: {error}"));
                    }
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => return WebSocketOutcome::Closed,
                Some(Ok(Message::Frame(_))) => {}
                Some(Err(error)) => return WebSocketOutcome::Failed(format!("WebSocket transport failed: {error}")),
            }
        }
    }
}

async fn connect_validated_address(addresses: &[SocketAddr]) -> io::Result<TcpStream> {
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("no validated addresses")))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub fn invalid_input(reason: impl Into<String>) -> ToolError {
    ToolError::InvalidInput {
        tool: ToolId::new(MONITOR_TOOL_NAME),
        reason: reason.into(),
        error_code: Some(INVALID_INPUT_CODE),
    }
}

pub fn execution_error(message: impl Into<String>) -> ToolError {
    ToolError::Execution {
        tool: ToolId::new(MONITOR_TOOL_NAME),
        source: anyhow::anyhow!(message.into()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
