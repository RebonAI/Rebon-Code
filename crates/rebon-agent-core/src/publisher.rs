//! Outbound ACP publishing helpers.
//!
//! This module owns the two agent → client transport surfaces the ACP runtime
//! currently needs:
//!
//! - best-effort `session/update` notifications
//! - `session/request_permission` reverse requests that wait for a response
//!
//! The notification path is intentionally infallible at the producer edge:
//! client disconnects should not fail the producer side of an agent turn.
//! The permission path is different: it must mint request ids, keep an
//! in-flight response map, and decode the nested JSON-RPC wire result back into
//! the flat host-facing `RequestPermissionResult` shape.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};

use rebon_proto::types::{
    JsonRpcRequest, JsonRpcResponse, JsonRpcVersion, RequestId, RequestPermissionParams,
    RequestPermissionResult, RequestPermissionWireResult, SessionId, SessionUpdate,
    SessionUpdateParams,
};

/// Trait implemented by sinks that accept agent → client `session/update`
/// notifications.
#[async_trait]
pub trait SessionUpdatePublisher: Send + Sync {
    /// Publish a fully-formed notification envelope.
    async fn publish_owned(&self, params: SessionUpdateParams);

    /// Convenience wrapper around [`publish_owned`](Self::publish_owned).
    async fn publish_to(&self, session_id: &SessionId, update: SessionUpdate) {
        self.publish_owned(SessionUpdateParams {
            session_id: session_id.clone(),
            update,
        })
        .await;
    }
}

/// In-process recorder used by tests.
#[derive(Debug, Clone, Default)]
pub struct MemorySessionUpdatePublisher {
    inner: Arc<Mutex<Vec<SessionUpdateParams>>>,
}

impl MemorySessionUpdatePublisher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<SessionUpdateParams> {
        self.inner
            .lock()
            .expect("MemorySessionUpdatePublisher mutex poisoned")
            .clone()
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("MemorySessionUpdatePublisher mutex poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        self.inner
            .lock()
            .expect("MemorySessionUpdatePublisher mutex poisoned")
            .clear();
    }
}

#[async_trait]
impl SessionUpdatePublisher for MemorySessionUpdatePublisher {
    async fn publish_owned(&self, params: SessionUpdateParams) {
        self.inner
            .lock()
            .expect("MemorySessionUpdatePublisher mutex poisoned")
            .push(params);
    }
}

/// Channel-backed notification publisher used by the ACP server loop.
#[derive(Debug, Clone)]
pub struct ChannelSessionUpdatePublisher {
    sender: UnboundedSender<SessionUpdateParams>,
}

impl ChannelSessionUpdatePublisher {
    pub fn new() -> (Self, UnboundedReceiver<SessionUpdateParams>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (Self { sender }, receiver)
    }
}

#[async_trait]
impl SessionUpdatePublisher for ChannelSessionUpdatePublisher {
    async fn publish_owned(&self, params: SessionUpdateParams) {
        if let Err(err) = self.sender.send(params) {
            tracing::debug!(
                "ChannelSessionUpdatePublisher: receiver gone, dropping notification: {err}"
            );
        }
    }
}

/// One outbound `session/request_permission` reverse request waiting to be sent.
#[derive(Debug)]
pub struct OutboundPermissionRequest {
    pub request_id: RequestId,
    pub params: RequestPermissionParams,
    pub response_tx: oneshot::Sender<JsonRpcResponse>,
}

/// Pending-response registry keyed by reverse request id.
pub type PendingPermissionRequests = HashMap<RequestId, oneshot::Sender<JsonRpcResponse>>;

/// Channel-backed reverse-request publisher for `session/request_permission`.
#[derive(Debug, Clone)]
pub struct ChannelPermissionRequestPublisher {
    sender: UnboundedSender<OutboundPermissionRequest>,
    next_request_id: Arc<AtomicU64>,
}

impl ChannelPermissionRequestPublisher {
    pub fn new() -> (Self, UnboundedReceiver<OutboundPermissionRequest>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                sender,
                next_request_id: Arc::new(AtomicU64::new(1)),
            },
            receiver,
        )
    }

    pub async fn request_permission(
        &self,
        params: RequestPermissionParams,
    ) -> anyhow::Result<RequestPermissionResult> {
        let request_id = next_permission_request_id(&self.next_request_id);
        let (response_tx, response_rx) = oneshot::channel();

        self.sender
            .send(OutboundPermissionRequest {
                request_id: request_id.clone(),
                params,
                response_tx,
            })
            .map_err(|_| anyhow::anyhow!("permission request receiver gone"))?;

        let response = response_rx
            .await
            .map_err(|_| anyhow::anyhow!("permission request response dropped"))?;

        parse_permission_response(response)
    }
}

pub fn permission_request_method() -> &'static str {
    "session/request_permission"
}

pub fn next_permission_request_id(counter: &AtomicU64) -> RequestId {
    RequestId::Number(counter.fetch_add(1, Ordering::Relaxed) as i64)
}

pub fn register_pending_permission_request(
    pending: &mut PendingPermissionRequests,
    request_id: RequestId,
    response_tx: oneshot::Sender<JsonRpcResponse>,
) {
    pending.insert(request_id, response_tx);
}

pub fn resolve_pending_permission_response(
    pending: &mut PendingPermissionRequests,
    response: JsonRpcResponse,
) -> bool {
    let Some(id) = response.id.clone() else {
        return false;
    };
    let Some(tx) = pending.remove(&id) else {
        return false;
    };
    if tx.send(response).is_err() {
        tracing::debug!(?id, "permission requester dropped before response arrived");
    }
    true
}

/// Test helper: how many reverse requests are still awaiting a
/// response. Public because the ACP server's tests live in another
/// crate.
pub fn pending_permission_request_count(pending: &PendingPermissionRequests) -> usize {
    pending.len()
}

pub fn serialize_permission_request(
    request_id: RequestId,
    params: &RequestPermissionParams,
) -> anyhow::Result<Vec<u8>> {
    let req = JsonRpcRequest {
        jsonrpc: JsonRpcVersion,
        id: request_id,
        method: permission_request_method().to_string(),
        params: Some(serde_json::to_value(params).map_err(|err| {
            anyhow::anyhow!("failed to serialize permission request params: {err}")
        })?),
    };

    serde_json::to_vec(&req)
        .map_err(|err| anyhow::anyhow!("failed to serialize permission JsonRpcRequest: {err}"))
}

pub fn parse_permission_response(
    response: JsonRpcResponse,
) -> anyhow::Result<RequestPermissionResult> {
    if let Some(error) = response.error {
        return Err(anyhow::anyhow!(
            "permission request failed (code {}): {}",
            error.code,
            error.message
        ));
    }

    let result = response
        .result
        .ok_or_else(|| anyhow::anyhow!("permission request response missing result"))?;
    let wire: RequestPermissionWireResult = serde_json::from_value(result)
        .map_err(|err| anyhow::anyhow!("invalid permission request response: {err}"))?;
    let mut outcome = wire.outcome;
    // Merge wire-level updated_input into the outcome so the broker
    // sees TUI-supplied answers (e.g. AskUserQuestion).
    if outcome.updated_input.is_none() {
        outcome.updated_input = wire.updated_input;
    }
    Ok(outcome)
}

/// Test helper: build the JSON-RPC response a client would send back
/// for a `session/request_permission`. Public for the same reason as
/// [`pending_permission_request_count`].
pub fn make_permission_result_response(
    request_id: RequestId,
    result: RequestPermissionResult,
) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: JsonRpcVersion,
        id: Some(request_id),
        result: Some(
            serde_json::to_value(RequestPermissionWireResult {
                outcome: result,
                updated_input: None,
            })
            .expect("permission result serializes"),
        ),
        error: None,
    }
}

#[cfg(test)]
pub fn make_permission_error_response(
    request_id: RequestId,
    code: i32,
    message: &str,
) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: JsonRpcVersion,
        id: Some(request_id),
        result: None,
        error: Some(rebon_proto::types::JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_proto::types::{
        ContentBlock, PermissionOption, PermissionOptionKind, PermissionOutcome, SessionUpdate,
        TextContent, ToolCallReference, ToolCallStatus, ToolCallStatus::Pending, ToolKind,
    };

    fn sample_permission_params() -> RequestPermissionParams {
        RequestPermissionParams {
            session_id: "sess-1".into(),
            tool_call: ToolCallReference {
                tool_call_id: "toolu_01".into(),
            },
            options: vec![PermissionOption {
                option_id: "allow_once".into(),
                name: "Allow once".into(),
                kind: PermissionOptionKind::AllowOnce,
            }],
            title: None,
            message: None,
            tool_name: None,
            tool_input: None,
            metadata: None,
        }
    }

    #[tokio::test]
    async fn memory_publisher_records_calls_in_order() {
        let pub_ = MemorySessionUpdatePublisher::new();
        assert!(pub_.is_empty());

        pub_.publish_to(
            &"sess-1".to_string(),
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(TextContent {
                    text: "hello".into(),
                    annotations: None,
                }),
            },
        )
        .await;

        pub_.publish_to(
            &"sess-1".to_string(),
            SessionUpdate::ToolCall {
                tool_call_id: "toolu_01".into(),
                title: "Read foo.rs".into(),
                kind: ToolKind::Read,
                status: Pending,
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            },
        )
        .await;

        let snap = pub_.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(pub_.len(), 2);

        match &snap[0].update {
            SessionUpdate::AgentMessageChunk { content } => match content {
                ContentBlock::Text(t) => assert_eq!(t.text, "hello"),
                other => panic!("expected text content, got {other:?}"),
            },
            other => panic!("expected agent_message_chunk, got {other:?}"),
        }
        assert_eq!(snap[0].session_id, "sess-1");

        match &snap[1].update {
            SessionUpdate::ToolCall {
                tool_call_id,
                title,
                kind,
                status,
                ..
            } => {
                assert_eq!(tool_call_id, "toolu_01");
                assert_eq!(title, "Read foo.rs");
                assert_eq!(*kind, ToolKind::Read);
                assert_eq!(*status, ToolCallStatus::Pending);
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_publisher_clear_resets_state() {
        let pub_ = MemorySessionUpdatePublisher::new();
        pub_.publish_owned(SessionUpdateParams {
            session_id: "s".into(),
            update: SessionUpdate::Plan { entries: vec![] },
        })
        .await;
        assert_eq!(pub_.len(), 1);
        pub_.clear();
        assert_eq!(pub_.len(), 0);
        assert!(pub_.is_empty());
    }

    #[tokio::test]
    async fn memory_publisher_is_clone_shareable() {
        let pub_a = MemorySessionUpdatePublisher::new();
        let pub_b = pub_a.clone();
        pub_a
            .publish_owned(SessionUpdateParams {
                session_id: "s".into(),
                update: SessionUpdate::Plan { entries: vec![] },
            })
            .await;
        pub_b
            .publish_owned(SessionUpdateParams {
                session_id: "s".into(),
                update: SessionUpdate::Plan { entries: vec![] },
            })
            .await;
        pub_a
            .publish_owned(SessionUpdateParams {
                session_id: "s".into(),
                update: SessionUpdate::Plan { entries: vec![] },
            })
            .await;
        assert_eq!(pub_a.len(), 3);
        assert_eq!(pub_b.len(), 3, "clones must share the same recording vec");
    }

    #[tokio::test]
    async fn channel_publisher_forwards_to_receiver() {
        let (pub_, mut rx) = ChannelSessionUpdatePublisher::new();

        pub_.publish_to(
            &"sess-1".to_string(),
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(TextContent {
                    text: "hi".into(),
                    annotations: None,
                }),
            },
        )
        .await;

        let envelope = rx.recv().await.expect("expected one notification");
        assert_eq!(envelope.session_id, "sess-1");
        assert!(matches!(
            envelope.update,
            SessionUpdate::AgentMessageChunk { .. }
        ));
    }

    #[tokio::test]
    async fn channel_publisher_drops_silently_when_receiver_gone() {
        let (pub_, rx) = ChannelSessionUpdatePublisher::new();
        drop(rx);

        pub_.publish_owned(SessionUpdateParams {
            session_id: "s".into(),
            update: SessionUpdate::Plan { entries: vec![] },
        })
        .await;
    }

    #[test]
    fn serialize_permission_request_uses_expected_wire_shape() {
        let bytes = serialize_permission_request(RequestId::Number(7), &sample_permission_params())
            .expect("request serializes");
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "session/request_permission");
        assert_eq!(value["params"]["sessionId"], "sess-1");
        assert_eq!(value["params"]["toolCall"]["toolCallId"], "toolu_01");
        assert_eq!(value["params"]["options"][0]["optionId"], "allow_once");
    }

    #[test]
    fn serialize_permission_request_preserves_non_null_metadata() {
        let mut params = sample_permission_params();
        params.metadata = Some(serde_json::json!({
            "kind": "workflowReview",
            "audit": {
                "title": "Workflow/RunWorkflow permission review",
                "phaseCount": 2
            }
        }));

        let bytes = serialize_permission_request(RequestId::Number(8), &params)
            .expect("request with metadata serializes");
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 8);
        assert_eq!(value["method"], "session/request_permission");
        assert_eq!(value["params"]["metadata"]["kind"], "workflowReview");
        assert_eq!(
            value["params"]["metadata"]["audit"]["title"],
            "Workflow/RunWorkflow permission review"
        );
        assert_eq!(value["params"]["metadata"]["audit"]["phaseCount"], 2);
    }

    #[test]
    fn parse_permission_response_unwraps_nested_wire_result() {
        let response = JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: Some(RequestId::Number(3)),
            result: Some(serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "allow_once"
                }
            })),
            error: None,
        };

        let result = parse_permission_response(response).unwrap();
        assert_eq!(result.outcome, PermissionOutcome::Selected);
        assert_eq!(result.option_id.as_deref(), Some("allow_once"));
    }

    #[test]
    fn parse_permission_response_surfaces_rpc_error() {
        let response = JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: Some(RequestId::Number(3)),
            result: None,
            error: Some(rebon_proto::types::JsonRpcError {
                code: -32001,
                message: "denied".into(),
                data: None,
            }),
        };

        let err = parse_permission_response(response).unwrap_err().to_string();
        assert!(err.contains("permission request failed"));
        assert!(err.contains("denied"));
    }

    #[tokio::test]
    async fn channel_permission_request_publisher_round_trips_response() {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let params = sample_permission_params();

        let requester =
            tokio::spawn(async move { publisher.request_permission(params).await.unwrap() });

        let outbound = rx.recv().await.expect("expected one permission request");
        assert_eq!(outbound.request_id, RequestId::Number(1));
        assert_eq!(outbound.params.session_id, "sess-1");

        outbound
            .response_tx
            .send(make_permission_result_response(
                outbound.request_id,
                RequestPermissionResult {
                    outcome: PermissionOutcome::Selected,
                    option_id: Some("allow_once".into()),
                    updated_input: None,
                },
            ))
            .expect("response receiver still live");

        let result = requester.await.unwrap();
        assert_eq!(result.outcome, PermissionOutcome::Selected);
        assert_eq!(result.option_id.as_deref(), Some("allow_once"));
    }

    #[test]
    fn resolve_pending_permission_response_delivers_once() {
        let (tx, rx) = oneshot::channel();
        let mut pending = PendingPermissionRequests::new();
        register_pending_permission_request(&mut pending, RequestId::Number(9), tx);

        let response = make_permission_result_response(
            RequestId::Number(9),
            RequestPermissionResult {
                outcome: PermissionOutcome::Cancelled,
                option_id: None,
                updated_input: None,
            },
        );

        assert!(resolve_pending_permission_response(&mut pending, response));
        assert_eq!(pending_permission_request_count(&pending), 0);
        assert!(!resolve_pending_permission_response(
            &mut pending,
            make_permission_error_response(RequestId::Number(9), -32001, "late"),
        ));

        let delivered = rx.blocking_recv().expect("response delivered");
        assert!(delivered.result.is_some());
    }
}
