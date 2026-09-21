//! Outbound request bookkeeping.
//!
//! Every JSON-RPC request we send needs an id nobody else is using and
//! somewhere to park the caller until the matching response arrives.
//! That is all this is — but two details are load-bearing:
//!
//! - **It is generic over the method.** The ACP server's table was
//!   permission-specific because permissions were the only reverse
//!   request it made. A client sends `initialize`, `session/new`,
//!   `session/load`, `session/prompt`, and `session/cancel`, all
//!   waiting on the same wire, so the table cannot know what it holds.
//! - **A dead connection fails every waiter.** If the agent process
//!   exits mid-turn, the response for the in-flight `session/prompt`
//!   never comes. Without [`PendingRequests::fail_all`] the caller
//!   waits forever on a oneshot whose sender lives in a table nobody
//!   will touch again — the turn hangs with no error and no output.
//!   Closing the table is what turns a dead agent into an error
//!   message.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use rebon_proto::types::{JsonRpcError, JsonRpcResponse, JsonRpcVersion, RequestId};

/// Why a pending request will never be answered.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PendingError {
    /// The connection was torn down before the response arrived.
    #[error("agent connection closed before responding: {0}")]
    ConnectionClosed(String),
}

#[derive(Default)]
struct Inner {
    waiting: HashMap<RequestId, tokio::sync::oneshot::Sender<JsonRpcResponse>>,
    /// Set once the connection dies. New registrations fail
    /// immediately rather than parking forever.
    closed: Option<String>,
}

/// In-flight outbound requests, keyed by the id we minted for them.
pub struct PendingRequests {
    inner: Mutex<Inner>,
    next_id: AtomicI64,
}

impl Default for PendingRequests {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingRequests {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            next_id: AtomicI64::new(1),
        }
    }

    /// Mint an id and park a slot for its response.
    ///
    /// Fails immediately if the connection is already gone, so a
    /// caller that raced the shutdown gets an error instead of a
    /// receiver nothing will ever send to.
    pub fn register(
        &self,
    ) -> Result<(RequestId, tokio::sync::oneshot::Receiver<JsonRpcResponse>), PendingError> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut inner = self.lock();
        if let Some(reason) = inner.closed.clone() {
            return Err(PendingError::ConnectionClosed(reason));
        }
        inner.waiting.insert(id.clone(), tx);
        Ok((id, rx))
    }

    /// Hand a response to whoever is waiting for it.
    ///
    /// Returns `false` when the id is unknown — a response to a
    /// request we never sent, or one whose caller already gave up.
    /// Either way it is the agent's problem to log, not ours to panic
    /// over.
    pub fn resolve(&self, response: JsonRpcResponse) -> bool {
        let Some(id) = response.id.clone() else {
            return false;
        };
        let sender = { self.lock().waiting.remove(&id) };
        let Some(sender) = sender else {
            return false;
        };
        if sender.send(response).is_err() {
            tracing::debug!(
                ?id,
                "acp-client: requester gave up before the response arrived"
            );
        }
        true
    }

    /// Drop a request we are no longer waiting on (caller timed out,
    /// turn cancelled).
    pub fn forget(&self, id: &RequestId) {
        self.lock().waiting.remove(id);
    }

    /// Fail every waiter and refuse new ones.
    ///
    /// Called when the agent process exits or its stdout closes. Each
    /// waiter receives a JSON-RPC error rather than a dropped sender,
    /// so callers report *why* the turn ended instead of "the channel
    /// closed".
    pub fn fail_all(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let waiting = {
            let mut inner = self.lock();
            if inner.closed.is_none() {
                inner.closed = Some(reason.clone());
            }
            std::mem::take(&mut inner.waiting)
        };
        for (id, sender) in waiting {
            let _ = sender.send(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: Some(id),
                result: None,
                error: Some(JsonRpcError {
                    code: rebon_proto::types::error_code::INTERNAL_ERROR,
                    message: reason.clone(),
                    data: None,
                }),
            });
        }
    }

    /// Whether the connection has been declared dead.
    pub fn is_closed(&self) -> bool {
        self.lock().closed.is_some()
    }

    /// How many requests are still waiting.
    pub fn in_flight(&self) -> usize {
        self.lock().waiting.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_response(id: RequestId) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: Some(id),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        }
    }

    #[tokio::test]
    async fn a_registered_request_receives_its_response() {
        let pending = PendingRequests::new();
        let (id, rx) = pending.register().unwrap();
        assert_eq!(pending.in_flight(), 1);

        assert!(pending.resolve(ok_response(id)));
        let response = rx.await.unwrap();
        assert_eq!(response.result, Some(serde_json::json!({"ok": true})));
        assert_eq!(pending.in_flight(), 0);
    }

    #[tokio::test]
    async fn ids_are_unique_across_concurrent_requests() {
        let pending = PendingRequests::new();
        let (first, _rx1) = pending.register().unwrap();
        let (second, _rx2) = pending.register().unwrap();
        assert_ne!(first, second);
        assert_eq!(pending.in_flight(), 2);
    }

    #[tokio::test]
    async fn an_unknown_response_is_reported_not_panicked() {
        let pending = PendingRequests::new();
        assert!(!pending.resolve(ok_response(RequestId::Number(99))));
    }

    #[tokio::test]
    async fn a_response_without_an_id_is_ignored() {
        let pending = PendingRequests::new();
        let mut response = ok_response(RequestId::Number(1));
        response.id = None;
        assert!(!pending.resolve(response));
    }

    #[tokio::test]
    async fn a_dead_connection_fails_every_waiter_instead_of_hanging() {
        let pending = PendingRequests::new();
        let (_id, rx) = pending.register().unwrap();

        pending.fail_all("agent exited with status 1");

        let response = rx.await.expect("waiter must be answered, not dropped");
        let error = response.error.expect("must carry the reason");
        assert!(error.message.contains("agent exited"));
        assert!(pending.is_closed());
        assert_eq!(pending.in_flight(), 0);
    }

    #[tokio::test]
    async fn registering_after_the_connection_died_fails_immediately() {
        let pending = PendingRequests::new();
        pending.fail_all("stdout closed");
        let err = pending.register().expect_err("must not park a new waiter");
        assert!(matches!(err, PendingError::ConnectionClosed(reason) if reason == "stdout closed"));
    }

    #[tokio::test]
    async fn forgetting_a_request_drops_its_slot() {
        let pending = PendingRequests::new();
        let (id, _rx) = pending.register().unwrap();
        pending.forget(&id);
        assert_eq!(pending.in_flight(), 0);
        assert!(!pending.resolve(ok_response(id)));
    }
}
