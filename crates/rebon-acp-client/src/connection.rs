//! The JSON-RPC connection to an agent, in both directions.
//!
//! ACP is symmetric once the pipe is open: we send requests to the
//! agent (`session/prompt`), the agent sends requests to us
//! (`session/request_permission`, `fs/write_text_file`), and either
//! side may send notifications. So this is not a request/response
//! client — it is a peer.
//!
//! [`Connection::spawn`] takes the two halves of a pipe and returns
//! immediately with a handle. A background task owns the read half:
//! it classifies each inbound message, resolves responses against
//! [`PendingRequests`], and hands requests and notifications to a
//! [`ClientDelegate`] the caller supplies. When the read half ends —
//! agent exited, stdout closed, wire garbage — that task fails every
//! in-flight request on the way out, which is what turns a dead agent
//! into an error rather than a hang.

use std::sync::Arc;

use rebon_proto::types::{
    error_code, JsonRpcError, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    JsonRpcVersion, ReadTextFileParams, ReadTextFileResult, RequestId, RequestPermissionParams,
    RequestPermissionResult, SessionUpdateParams, WriteTextFileParams,
};
use rebon_proto::{FramingMode, StdioReader, StdioWriter};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;

use crate::pending::{PendingError, PendingRequests};

/// Method names this client speaks. Kept in one place so a typo is a
/// compile error rather than a silent method-not-found.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const SESSION_NEW: &str = "session/new";
    pub const SESSION_LOAD: &str = "session/load";
    pub const SESSION_PROMPT: &str = "session/prompt";
    pub const SESSION_CANCEL: &str = "session/cancel";
    pub const SESSION_UPDATE: &str = "session/update";
    pub const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
    pub const FS_READ_TEXT_FILE: &str = "fs/read_text_file";
    pub const FS_WRITE_TEXT_FILE: &str = "fs/write_text_file";
    /// Extension request: inject a message into the running turn.
    /// Advertised by the agent via `InitializeResult._meta.steering`.
    pub const SESSION_STEERING: &str = "_session/steering";
}

/// What went wrong talking to the agent.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error(transparent)]
    Pending(#[from] PendingError),
    #[error("failed to write to the agent: {0}")]
    Write(String),
    #[error("failed to encode {method}: {source}")]
    Encode {
        method: &'static str,
        source: serde_json::Error,
    },
    #[error("failed to decode the response to {method}: {source}")]
    Decode {
        method: &'static str,
        source: serde_json::Error,
    },
    #[error("agent rejected {method} (code {code}): {message}")]
    Rpc {
        method: &'static str,
        code: i32,
        message: String,
    },
    #[error("agent returned neither a result nor an error for {method}")]
    EmptyResponse { method: &'static str },
}

/// The client side of the conversation: what Rebon does when the
/// agent asks *it* for something.
///
/// Every method here is a request the agent makes of us mid-turn, so
/// answering slowly stalls the agent's turn. Implementations should
/// resolve or reject promptly rather than blocking on anything
/// unbounded.
#[async_trait::async_trait]
pub trait ClientDelegate: Send + Sync {
    /// A streaming update for a session: message chunks, tool calls,
    /// plan entries.
    async fn session_update(&self, params: SessionUpdateParams);

    /// The agent wants permission to run a tool.
    async fn request_permission(
        &self,
        params: RequestPermissionParams,
    ) -> anyhow::Result<RequestPermissionResult>;

    /// The agent wants to read a file through us rather than off the
    /// disk itself.
    async fn read_text_file(&self, params: ReadTextFileParams) -> anyhow::Result<String>;

    /// The agent wants to write a file through us.
    ///
    /// This is the call that keeps `/rewind` honest: a write that
    /// arrives here can be snapshotted before it lands. An agent that
    /// never calls it is writing behind Rebon's back.
    async fn write_text_file(&self, params: WriteTextFileParams) -> anyhow::Result<()>;

    /// Any other request the agent makes. The default refuses with
    /// method-not-found, which is the correct answer for a capability
    /// we never advertised.
    async fn unknown_request(&self, method: &str) -> anyhow::Result<serde_json::Value> {
        Err(anyhow::anyhow!("unsupported client method: {method}"))
    }
}

/// A live connection to an agent.
pub struct Connection {
    writer: Arc<dyn OutboundWriter>,
    pending: Arc<PendingRequests>,
    reader_task: tokio::task::JoinHandle<()>,
}

/// Erases the writer's stream type so [`Connection`] is not generic.
#[async_trait::async_trait]
trait OutboundWriter: Send + Sync {
    async fn write(&self, body: Vec<u8>) -> Result<(), String>;
}

struct FramedWriter<W> {
    writer: Mutex<StdioWriter<W>>,
    framing: FramingMode,
}

#[async_trait::async_trait]
impl<W: AsyncWrite + Unpin + Send + 'static> OutboundWriter for FramedWriter<W> {
    async fn write(&self, body: Vec<u8>) -> Result<(), String> {
        let mut writer = self.writer.lock().await;
        let result = match self.framing {
            FramingMode::ContentLength => writer.write_content_length(&body).await,
            // `Ndjson` and "not yet known" both mean newline-delimited:
            // it is what the ACP CLIs in the wild speak, and the
            // decoder on the other side auto-detects anyway.
            _ => writer.write_ndjson(&body).await,
        };
        result.map_err(|err| err.to_string())
    }
}

impl Connection {
    /// Take ownership of a pipe and start serving it.
    ///
    /// `reader` and `writer` are the agent's stdout and stdin. The
    /// returned connection stays usable until the agent stops writing.
    pub fn spawn<R, W>(
        reader: R,
        writer: W,
        framing: FramingMode,
        delegate: Arc<dyn ClientDelegate>,
    ) -> Arc<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let outbound: Arc<dyn OutboundWriter> = Arc::new(FramedWriter {
            writer: Mutex::new(StdioWriter::new(writer)),
            framing,
        });
        let pending = Arc::new(PendingRequests::new());
        let reader_task = tokio::spawn(read_loop(
            StdioReader::new(reader),
            pending.clone(),
            outbound.clone(),
            delegate,
        ));
        Arc::new(Self {
            writer: outbound,
            pending,
            reader_task,
        })
    }

    /// Send a request and wait for its response.
    pub async fn request<P, R>(&self, method: &'static str, params: P) -> Result<R, ConnectionError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let params = serde_json::to_value(params)
            .map_err(|source| ConnectionError::Encode { method, source })?;
        let (id, rx) = self.pending.register()?;
        let request = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: id.clone(),
            method: method.to_string(),
            params: Some(params),
        };
        let body = serde_json::to_vec(&request)
            .map_err(|source| ConnectionError::Encode { method, source })?;
        if let Err(err) = self.writer.write(body).await {
            self.pending.forget(&id);
            return Err(ConnectionError::Write(err));
        }

        // The sender lives in the pending table, and the read loop
        // fails every entry on the way out, so a dropped sender here
        // means the table was emptied without a reason — treat it the
        // same as a closed connection rather than panicking.
        let response = rx.await.map_err(|_| {
            ConnectionError::Pending(PendingError::ConnectionClosed(
                "agent connection dropped".into(),
            ))
        })?;
        decode_response(method, response)
    }

    /// Send a notification. Nothing comes back, including errors.
    pub async fn notify<P: Serialize>(
        &self,
        method: &'static str,
        params: P,
    ) -> Result<(), ConnectionError> {
        let params = serde_json::to_value(params)
            .map_err(|source| ConnectionError::Encode { method, source })?;
        let notification = JsonRpcNotification {
            jsonrpc: JsonRpcVersion,
            method: method.to_string(),
            params: Some(params),
        };
        let body = serde_json::to_vec(&notification)
            .map_err(|source| ConnectionError::Encode { method, source })?;
        self.writer
            .write(body)
            .await
            .map_err(ConnectionError::Write)
    }

    /// Whether the connection has been declared dead.
    pub fn is_closed(&self) -> bool {
        self.pending.is_closed()
    }

    /// Fail everything in flight and stop reading. Idempotent.
    pub fn shutdown(&self, reason: impl Into<String>) {
        self.pending.fail_all(reason);
        self.reader_task.abort();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.pending.fail_all("agent connection dropped");
        self.reader_task.abort();
    }
}

fn decode_response<R: DeserializeOwned>(
    method: &'static str,
    response: JsonRpcResponse,
) -> Result<R, ConnectionError> {
    if let Some(error) = response.error {
        return Err(ConnectionError::Rpc {
            method,
            code: error.code,
            message: error.message,
        });
    }
    let result = response
        .result
        .ok_or(ConnectionError::EmptyResponse { method })?;
    serde_json::from_value(result).map_err(|source| ConnectionError::Decode { method, source })
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: StdioReader<R>,
    pending: Arc<PendingRequests>,
    writer: Arc<dyn OutboundWriter>,
    delegate: Arc<dyn ClientDelegate>,
) {
    let reason = loop {
        let body = match reader.read_message().await {
            Ok(Some(body)) => body,
            Ok(None) => break "agent closed its output stream".to_string(),
            Err(err) => break format!("agent output stream failed: {err}"),
        };
        let message = match JsonRpcMessage::from_bytes(&body) {
            Ok(message) => message,
            Err(err) => {
                // One unparseable frame is the agent's bug, not a
                // reason to tear down a working session.
                tracing::warn!(error = %err, "acp-client: dropping unparseable message");
                continue;
            }
        };
        match message {
            JsonRpcMessage::Response(response) => {
                if !pending.resolve(response) {
                    tracing::debug!("acp-client: response for an unknown request id");
                }
            }
            JsonRpcMessage::Notification(notification) => {
                dispatch_notification(notification, delegate.as_ref()).await;
            }
            JsonRpcMessage::Request(request) => {
                // Served on its own task: the delegate may need to ask
                // the user, and blocking the read loop on that would
                // stall every other message from the agent, including
                // the updates that render the question.
                let delegate = delegate.clone();
                let writer = writer.clone();
                tokio::spawn(async move {
                    let (id, response) = serve_request(request, delegate.as_ref()).await;
                    let body = match serde_json::to_vec(&response) {
                        Ok(body) => body,
                        Err(err) => {
                            tracing::warn!(?id, error = %err, "acp-client: failed to encode reply");
                            return;
                        }
                    };
                    if let Err(err) = writer.write(body).await {
                        tracing::warn!(?id, error = %err, "acp-client: failed to reply");
                    }
                });
            }
        }
    };
    tracing::info!(reason = %reason, "acp-client: connection closed");
    pending.fail_all(reason);
}

async fn dispatch_notification(notification: JsonRpcNotification, delegate: &dyn ClientDelegate) {
    if notification.method != method::SESSION_UPDATE {
        tracing::debug!(
            method = %notification.method,
            "acp-client: ignoring unknown notification"
        );
        return;
    }
    let Some(params) = notification.params else {
        tracing::warn!("acp-client: session/update without params");
        return;
    };
    match serde_json::from_value::<SessionUpdateParams>(params) {
        Ok(params) => delegate.session_update(params).await,
        Err(err) => tracing::warn!(error = %err, "acp-client: invalid session/update"),
    }
}

async fn serve_request(
    request: JsonRpcRequest,
    delegate: &dyn ClientDelegate,
) -> (RequestId, JsonRpcResponse) {
    let id = request.id.clone();
    let params = request.params.unwrap_or(serde_json::Value::Null);
    let outcome = match request.method.as_str() {
        method::SESSION_REQUEST_PERMISSION => {
            match serde_json::from_value::<RequestPermissionParams>(params) {
                Ok(params) => delegate
                    .request_permission(params)
                    .await
                    .and_then(|result| Ok(serde_json::to_value(result)?)),
                Err(err) => Err(anyhow::anyhow!("invalid params: {err}")),
            }
        }
        method::FS_READ_TEXT_FILE => match serde_json::from_value::<ReadTextFileParams>(params) {
            Ok(params) => delegate
                .read_text_file(params)
                .await
                .and_then(|content| Ok(serde_json::to_value(ReadTextFileResult { content })?)),
            Err(err) => Err(anyhow::anyhow!("invalid params: {err}")),
        },
        method::FS_WRITE_TEXT_FILE => match serde_json::from_value::<WriteTextFileParams>(params) {
            Ok(params) => delegate
                .write_text_file(params)
                .await
                .map(|()| serde_json::json!({})),
            Err(err) => Err(anyhow::anyhow!("invalid params: {err}")),
        },
        other => delegate.unknown_request(other).await,
    };

    let response = match outcome {
        Ok(result) => JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: Some(id.clone()),
            result: Some(result),
            error: None,
        },
        Err(err) => JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: Some(id.clone()),
            result: None,
            error: Some(JsonRpcError {
                code: error_code::INTERNAL_ERROR,
                message: err.to_string(),
                data: None,
            }),
        },
    };
    (id, response)
}
