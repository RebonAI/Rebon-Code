//! The async bridge API trait and its in-memory implementation.
//!
//! One object-safe method per backend operation.
//!
//! No concrete transport appears here — neither `reqwest` nor
//! `tokio-tungstenite`. The real HTTP implementation lives in
//! `crate::http_client` behind the `http` cargo feature and carries its own
//! transport dependencies; this module ships only:
//!
//! - [`BridgeApiClient`] — the async trait a runtime plugs into
//! - [`BridgeApiError`] — the shared error type
//! - [`InMemoryBridgeApiClient`] — a record-and-replay test double
//!
//! The trait covers the whole surface: `register`, `poll`, `acknowledge`,
//! `stop`, `deregister`, `send_permission_response_event`, `archive_session`,
//! `reconnect_session`, `heartbeat_work`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use thiserror::Error;

use crate::config::{
    BridgeConfig, HeartbeatOutcome, PermissionResponseEvent, RegisteredEnvironment, WorkItem,
    WorkResponse,
};

/// Unified error type produced by [`BridgeApiClient`] implementations.
///
/// Covers auth failure, permanent (4xx-style) failure, transient
/// (5xx / network) failure, and a catch-all `Other` for unexpected
/// payload shapes.
#[derive(Debug, Error)]
pub enum BridgeApiError {
    /// OAuth / trusted-device auth failed. Matches the `401` path.
    #[error("bridge api unauthorized: {0}")]
    Unauthorized(String),
    /// Permanent failure (4xx that is not 401). Further retries would
    /// be pointless; the caller should surface the error to the user.
    #[error("bridge api permanent failure: {0}")]
    Permanent(String),
    /// Transient failure (network error, 5xx). The caller may retry
    /// with backoff.
    #[error("bridge api transient failure: {0}")]
    Transient(String),
    /// Unexpected payload / parse error. Treated as permanent on the
    /// caller side.
    #[error("bridge api protocol error: {0}")]
    Protocol(String),
    /// Catch-all for implementation-specific errors that don't fit
    /// the buckets above.
    #[error("bridge api error: {0}")]
    Other(String),
}

impl BridgeApiError {
    /// Convenience constructor for ad-hoc errors produced by test
    /// doubles and the runtime loop.
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// Whether a retry loop should back off and try again.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Result alias for [`BridgeApiClient`] operations.
pub type BridgeApiResult<T> = Result<T, BridgeApiError>;

/// Async trait every bridge transport implements.
///
/// One method per backend operation. Cancellation is not baked into
/// the trait: `poll_for_work` takes an owned [`PollOptions`] struct,
/// which keeps the trait object-safe without committing to a specific
/// cancellation primitive.
#[async_trait]
pub trait BridgeApiClient: Send + Sync {
    /// Register (or re-register) a bridge environment. Returns the
    /// backend-issued environment id + environment secret.
    async fn register_bridge_environment(
        &self,
        config: &BridgeConfig,
    ) -> BridgeApiResult<RegisteredEnvironment>;

    /// Long-poll for work. Returns `Ok(Some(work))` when work is
    /// available, `Ok(None)` when the poll timed out with no work.
    async fn poll_for_work(
        &self,
        environment_id: &str,
        environment_secret: &str,
        options: PollOptions,
    ) -> BridgeApiResult<Option<WorkResponse>>;

    /// Long-poll for work, keeping the [`WorkItem::session`] a session
    /// runner needs — the project, prompt and resume target that
    /// [`WorkResponse`] has no room for.
    ///
    /// The default wraps [`BridgeApiClient::poll_for_work`] and so never
    /// carries a session; a transport that can read one overrides it.
    async fn poll_for_work_item(
        &self,
        environment_id: &str,
        environment_secret: &str,
        options: PollOptions,
    ) -> BridgeApiResult<Option<WorkItem>> {
        Ok(self
            .poll_for_work(environment_id, environment_secret, options)
            .await?
            .map(WorkItem::from))
    }

    /// Acknowledge work receipt / completion.
    async fn acknowledge_work(
        &self,
        environment_id: &str,
        work_id: &str,
        session_token: &str,
    ) -> BridgeApiResult<()>;

    /// Stop a work item via the environments API.
    async fn stop_work(
        &self,
        environment_id: &str,
        work_id: &str,
        force: bool,
    ) -> BridgeApiResult<()>;

    /// Deregister / delete the bridge environment on graceful shutdown.
    async fn deregister_environment(&self, environment_id: &str) -> BridgeApiResult<()>;

    /// Send a permission response (control_response) to a session via
    /// the session events API.
    async fn send_permission_response_event(
        &self,
        session_id: &str,
        event: &PermissionResponseEvent,
        session_token: &str,
    ) -> BridgeApiResult<()>;

    /// Archive a session so it no longer appears as active on the
    /// server.
    async fn archive_session(&self, session_id: &str) -> BridgeApiResult<()>;

    /// Force-stop stale worker instances and re-queue a session on an
    /// environment. Used by `--session-id` to resume a session after
    /// its previous bridge is gone.
    async fn reconnect_session(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> BridgeApiResult<()>;

    /// Lightweight heartbeat that extends an active work item's lease.
    async fn heartbeat_work(
        &self,
        environment_id: &str,
        work_id: &str,
        session_token: &str,
    ) -> BridgeApiResult<HeartbeatOutcome>;
}

/// Options passed to [`BridgeApiClient::poll_for_work`].
///
/// Carries the optional reclaim-age hint.
#[derive(Debug, Clone, Default)]
pub struct PollOptions {
    /// Hint to the server: reclaim work items that are older than this
    /// many milliseconds (the wire `reclaimOlderThanMs` parameter).
    pub reclaim_older_than_ms: Option<u64>,
}

// ─── In-memory test double ─────────────────────────────────────────────────

/// In-memory [`BridgeApiClient`] implementation that records calls
/// and replays scripted poll responses. Used by tests, by the runtime
/// integration tests, and as the default wiring in the foundation module.
///
/// All mutable state lives behind an `Arc<Mutex<…>>` so the client is
/// cheap to clone and can be shared across tasks. Individual calls
/// append to [`InMemoryBridgeApiClient::calls`] and optionally inject
/// errors via [`InMemoryBridgeApiClient::inject_error`].
#[derive(Debug, Clone, Default)]
pub struct InMemoryBridgeApiClient {
    inner: Arc<Mutex<InMemoryInner>>,
}

#[derive(Debug, Default)]
struct InMemoryInner {
    environment: Option<RegisteredEnvironment>,
    scripted_polls: VecDeque<Result<Option<WorkItem>, BridgeApiError>>,
    heartbeat: Option<HeartbeatOutcome>,
    calls: Vec<RecordedCall>,
    // Each entry is a (method-name, error) pair that will be returned
    // the next time that method is called.
    injected_errors: Vec<(RecordedMethod, BridgeApiError)>,
}

/// One recorded invocation of an [`InMemoryBridgeApiClient`] method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    /// Which method was invoked.
    pub method: RecordedMethod,
    /// Free-form argument summary — method-specific stringification
    /// useful for test assertions.
    pub args: String,
}

/// Tag identifying which [`BridgeApiClient`] method produced a
/// [`RecordedCall`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedMethod {
    /// `register_bridge_environment`.
    Register,
    /// `poll_for_work`.
    Poll,
    /// `acknowledge_work`.
    Acknowledge,
    /// `stop_work`.
    Stop,
    /// `deregister_environment`.
    Deregister,
    /// `send_permission_response_event`.
    SendPermissionResponse,
    /// `archive_session`.
    Archive,
    /// `reconnect_session`.
    Reconnect,
    /// `heartbeat_work`.
    Heartbeat,
}

impl InMemoryBridgeApiClient {
    /// Construct an empty client. Registration returns a synthetic
    /// environment id unless overridden via
    /// [`Self::with_environment`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed the client with the environment that
    /// `register_bridge_environment` should return. If not set, the
    /// client synthesises `(env-inmemory, secret-inmemory)` the first
    /// time `register_bridge_environment` is called.
    pub fn with_environment(self, environment: RegisteredEnvironment) -> Self {
        {
            let mut guard = self
                .inner
                .lock()
                .expect("in-memory bridge api client poisoned");
            guard.environment = Some(environment);
        }
        self
    }

    /// Append a scripted poll response. The runtime's poll loop will
    /// consume scripted responses FIFO and fall back to `Ok(None)`
    /// once the script is exhausted.
    pub fn push_poll(&self, outcome: Result<Option<WorkResponse>, BridgeApiError>) {
        self.push_poll_item(outcome.map(|work| work.map(WorkItem::from)));
    }

    /// Append a scripted poll response that carries a whole [`WorkItem`],
    /// session included. `poll_for_work_item` hands it out as is;
    /// `poll_for_work` hands out its envelope.
    pub fn push_poll_item(&self, outcome: Result<Option<WorkItem>, BridgeApiError>) {
        let mut guard = self
            .inner
            .lock()
            .expect("in-memory bridge api client poisoned");
        guard.scripted_polls.push_back(outcome);
    }

    /// Configure the heartbeat outcome returned by `heartbeat_work`.
    pub fn set_heartbeat_outcome(&self, outcome: HeartbeatOutcome) {
        let mut guard = self
            .inner
            .lock()
            .expect("in-memory bridge api client poisoned");
        guard.heartbeat = Some(outcome);
    }

    /// Inject a one-shot error for the next call to `method`. Useful
    /// for testing error paths.
    pub fn inject_error(&self, method: RecordedMethod, error: BridgeApiError) {
        let mut guard = self
            .inner
            .lock()
            .expect("in-memory bridge api client poisoned");
        guard.injected_errors.push((method, error));
    }

    /// Snapshot the recorded calls so far.
    pub fn calls(&self) -> Vec<RecordedCall> {
        let guard = self
            .inner
            .lock()
            .expect("in-memory bridge api client poisoned");
        guard.calls.clone()
    }

    /// Number of recorded calls.
    pub fn call_count(&self) -> usize {
        let guard = self
            .inner
            .lock()
            .expect("in-memory bridge api client poisoned");
        guard.calls.len()
    }

    /// Reset the recorded-call log (keeps scripted state intact).
    pub fn clear_calls(&self) {
        let mut guard = self
            .inner
            .lock()
            .expect("in-memory bridge api client poisoned");
        guard.calls.clear();
    }

    fn record(&self, call: RecordedCall) {
        let mut guard = self
            .inner
            .lock()
            .expect("in-memory bridge api client poisoned");
        guard.calls.push(call);
    }

    fn take_injected_error(&self, method: RecordedMethod) -> Option<BridgeApiError> {
        let mut guard = self
            .inner
            .lock()
            .expect("in-memory bridge api client poisoned");
        let pos = guard
            .injected_errors
            .iter()
            .position(|(m, _)| *m == method)?;
        Some(guard.injected_errors.remove(pos).1)
    }
}

#[async_trait]
impl BridgeApiClient for InMemoryBridgeApiClient {
    async fn register_bridge_environment(
        &self,
        config: &BridgeConfig,
    ) -> BridgeApiResult<RegisteredEnvironment> {
        self.record(RecordedCall {
            method: RecordedMethod::Register,
            args: format!(
                "bridge_id={} env_id={} worker_type={}",
                config.bridge_id, config.environment_id, config.worker_type
            ),
        });
        if let Some(err) = self.take_injected_error(RecordedMethod::Register) {
            return Err(err);
        }
        let mut guard = self
            .inner
            .lock()
            .expect("in-memory bridge api client poisoned");
        if let Some(env) = guard.environment.clone() {
            return Ok(env);
        }
        let synthesised = RegisteredEnvironment {
            environment_id: format!("env-inmemory-{}", config.bridge_id),
            environment_secret: "secret-inmemory".to_string(),
        };
        guard.environment = Some(synthesised.clone());
        Ok(synthesised)
    }

    async fn poll_for_work(
        &self,
        environment_id: &str,
        environment_secret: &str,
        options: PollOptions,
    ) -> BridgeApiResult<Option<WorkResponse>> {
        Ok(self
            .poll_for_work_item(environment_id, environment_secret, options)
            .await?
            .map(|item| item.response))
    }

    /// Recorded as a `Poll` whichever of the two poll methods was called,
    /// and hands out the scripted item with its session.
    async fn poll_for_work_item(
        &self,
        environment_id: &str,
        _environment_secret: &str,
        options: PollOptions,
    ) -> BridgeApiResult<Option<WorkItem>> {
        self.record(RecordedCall {
            method: RecordedMethod::Poll,
            args: format!(
                "env_id={} reclaim_ms={:?}",
                environment_id, options.reclaim_older_than_ms
            ),
        });
        if let Some(err) = self.take_injected_error(RecordedMethod::Poll) {
            return Err(err);
        }
        let mut guard = self
            .inner
            .lock()
            .expect("in-memory bridge api client poisoned");
        match guard.scripted_polls.pop_front() {
            Some(outcome) => outcome,
            None => Ok(None),
        }
    }

    async fn acknowledge_work(
        &self,
        environment_id: &str,
        work_id: &str,
        _session_token: &str,
    ) -> BridgeApiResult<()> {
        self.record(RecordedCall {
            method: RecordedMethod::Acknowledge,
            args: format!("env_id={} work_id={}", environment_id, work_id),
        });
        if let Some(err) = self.take_injected_error(RecordedMethod::Acknowledge) {
            return Err(err);
        }
        Ok(())
    }

    async fn stop_work(
        &self,
        environment_id: &str,
        work_id: &str,
        force: bool,
    ) -> BridgeApiResult<()> {
        self.record(RecordedCall {
            method: RecordedMethod::Stop,
            args: format!(
                "env_id={} work_id={} force={}",
                environment_id, work_id, force
            ),
        });
        if let Some(err) = self.take_injected_error(RecordedMethod::Stop) {
            return Err(err);
        }
        Ok(())
    }

    async fn deregister_environment(&self, environment_id: &str) -> BridgeApiResult<()> {
        self.record(RecordedCall {
            method: RecordedMethod::Deregister,
            args: format!("env_id={}", environment_id),
        });
        if let Some(err) = self.take_injected_error(RecordedMethod::Deregister) {
            return Err(err);
        }
        Ok(())
    }

    async fn send_permission_response_event(
        &self,
        session_id: &str,
        event: &PermissionResponseEvent,
        _session_token: &str,
    ) -> BridgeApiResult<()> {
        self.record(RecordedCall {
            method: RecordedMethod::SendPermissionResponse,
            args: format!(
                "session_id={} request_id={} subtype={}",
                session_id, event.response.request_id, event.response.subtype
            ),
        });
        if let Some(err) = self.take_injected_error(RecordedMethod::SendPermissionResponse) {
            return Err(err);
        }
        Ok(())
    }

    async fn archive_session(&self, session_id: &str) -> BridgeApiResult<()> {
        self.record(RecordedCall {
            method: RecordedMethod::Archive,
            args: format!("session_id={}", session_id),
        });
        if let Some(err) = self.take_injected_error(RecordedMethod::Archive) {
            return Err(err);
        }
        Ok(())
    }

    async fn reconnect_session(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> BridgeApiResult<()> {
        self.record(RecordedCall {
            method: RecordedMethod::Reconnect,
            args: format!("env_id={} session_id={}", environment_id, session_id),
        });
        if let Some(err) = self.take_injected_error(RecordedMethod::Reconnect) {
            return Err(err);
        }
        Ok(())
    }

    async fn heartbeat_work(
        &self,
        environment_id: &str,
        work_id: &str,
        _session_token: &str,
    ) -> BridgeApiResult<HeartbeatOutcome> {
        self.record(RecordedCall {
            method: RecordedMethod::Heartbeat,
            args: format!("env_id={} work_id={}", environment_id, work_id),
        });
        if let Some(err) = self.take_injected_error(RecordedMethod::Heartbeat) {
            return Err(err);
        }
        let guard = self
            .inner
            .lock()
            .expect("in-memory bridge api client poisoned");
        Ok(guard.heartbeat.clone().unwrap_or(HeartbeatOutcome {
            lease_extended: true,
            state: "running".to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BridgeConfig, PermissionResponseBody, WorkData, WorkDataType};
    use serde_json::json;

    fn sample_config() -> BridgeConfig {
        BridgeConfig::minimal("bridge-1", "env-client-1", "https://api", "wss://sess")
    }

    #[tokio::test]
    async fn register_synthesises_environment_when_not_preseeded() {
        let client = InMemoryBridgeApiClient::new();
        let env = client
            .register_bridge_environment(&sample_config())
            .await
            .unwrap();
        assert_eq!(env.environment_id, "env-inmemory-bridge-1");
        assert_eq!(env.environment_secret, "secret-inmemory");
        assert_eq!(client.call_count(), 1);
        assert_eq!(client.calls()[0].method, RecordedMethod::Register);
    }

    #[tokio::test]
    async fn register_returns_preseeded_environment() {
        let client = InMemoryBridgeApiClient::new().with_environment(RegisteredEnvironment {
            environment_id: "env-backend".into(),
            environment_secret: "hunter2".into(),
        });
        let env = client
            .register_bridge_environment(&sample_config())
            .await
            .unwrap();
        assert_eq!(env.environment_id, "env-backend");
        assert_eq!(env.environment_secret, "hunter2");
    }

    #[tokio::test]
    async fn poll_returns_scripted_responses_then_falls_back_to_none() {
        let client = InMemoryBridgeApiClient::new();
        client.push_poll(Ok(Some(WorkResponse {
            id: "work-1".into(),
            response_type: "work".into(),
            environment_id: "env-1".into(),
            state: "ready".into(),
            data: WorkData {
                data_type: WorkDataType::Session,
                id: "sess-1".into(),
            },
            secret: "opaque".into(),
            created_at: "2026-04-09T00:00:00.000Z".into(),
        })));

        let first = client
            .poll_for_work("env-1", "secret", PollOptions::default())
            .await
            .unwrap();
        assert_eq!(first.as_ref().unwrap().id, "work-1");

        let second = client
            .poll_for_work("env-1", "secret", PollOptions::default())
            .await
            .unwrap();
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn a_scripted_item_keeps_its_session_on_the_item_poll_only() {
        let item = WorkItem {
            response: WorkResponse {
                id: "work-2".into(),
                response_type: "work".into(),
                environment_id: "env-1".into(),
                state: "leased".into(),
                data: WorkData {
                    data_type: WorkDataType::Session,
                    id: "sess-2".into(),
                },
                secret: "opaque".into(),
                created_at: "2026-09-16T00:00:00.000Z".into(),
            },
            session: Some(crate::config::SessionWork {
                project: "/srv/app".into(),
                prompt: Some("hi".into()),
                resume_rebon_session_id: None,
            }),
        };
        let client = InMemoryBridgeApiClient::new();
        client.push_poll_item(Ok(Some(item.clone())));
        client.push_poll_item(Ok(Some(item.clone())));

        let polled = client
            .poll_for_work_item("env-1", "secret", PollOptions::default())
            .await
            .unwrap();
        assert_eq!(polled, Some(item.clone()));
        let envelope = client
            .poll_for_work("env-1", "secret", PollOptions::default())
            .await
            .unwrap();
        assert_eq!(envelope, Some(item.response));
        let methods: Vec<_> = client.calls().into_iter().map(|c| c.method).collect();
        assert_eq!(methods, vec![RecordedMethod::Poll, RecordedMethod::Poll]);
    }

    #[tokio::test]
    async fn inject_error_fires_once_then_stops_intercepting() {
        let client = InMemoryBridgeApiClient::new();
        client.inject_error(
            RecordedMethod::Poll,
            BridgeApiError::Transient("network down".into()),
        );

        let err = client
            .poll_for_work("env-1", "secret", PollOptions::default())
            .await
            .unwrap_err();
        assert!(err.is_transient());
        assert!(matches!(err, BridgeApiError::Transient(_)));

        // Second call should no longer error.
        let outcome = client
            .poll_for_work("env-1", "secret", PollOptions::default())
            .await
            .unwrap();
        assert!(outcome.is_none());
    }

    #[tokio::test]
    async fn send_permission_response_event_records_args() {
        let client = InMemoryBridgeApiClient::new();
        let event = PermissionResponseEvent::new(PermissionResponseBody::success(
            "req-77",
            json!({"behavior": "deny", "message": "nope"}),
        ));
        client
            .send_permission_response_event("sess-1", &event, "tok")
            .await
            .unwrap();
        let calls = client.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, RecordedMethod::SendPermissionResponse);
        assert!(calls[0].args.contains("session_id=sess-1"));
        assert!(calls[0].args.contains("request_id=req-77"));
        assert!(calls[0].args.contains("subtype=success"));
    }

    #[tokio::test]
    async fn heartbeat_returns_scripted_outcome_when_set() {
        let client = InMemoryBridgeApiClient::new();
        client.set_heartbeat_outcome(HeartbeatOutcome {
            lease_extended: false,
            state: "lost".into(),
        });
        let outcome = client
            .heartbeat_work("env-1", "work-1", "tok")
            .await
            .unwrap();
        assert!(!outcome.lease_extended);
        assert_eq!(outcome.state, "lost");
    }

    #[tokio::test]
    async fn acknowledge_stop_deregister_and_archive_record_calls() {
        let client = InMemoryBridgeApiClient::new();
        client
            .acknowledge_work("env-1", "work-1", "tok")
            .await
            .unwrap();
        client.stop_work("env-1", "work-1", true).await.unwrap();
        client.archive_session("sess-1").await.unwrap();
        client.reconnect_session("env-1", "sess-1").await.unwrap();
        client.deregister_environment("env-1").await.unwrap();

        let methods: Vec<_> = client.calls().into_iter().map(|c| c.method).collect();
        assert_eq!(
            methods,
            vec![
                RecordedMethod::Acknowledge,
                RecordedMethod::Stop,
                RecordedMethod::Archive,
                RecordedMethod::Reconnect,
                RecordedMethod::Deregister,
            ]
        );
    }

    #[tokio::test]
    async fn clear_calls_resets_log_without_dropping_scripted_state() {
        let client = InMemoryBridgeApiClient::new();
        client.push_poll(Ok(None));
        client
            .poll_for_work("env-1", "secret", PollOptions::default())
            .await
            .unwrap();
        assert_eq!(client.call_count(), 1);
        client.clear_calls();
        assert_eq!(client.call_count(), 0);
        // Scripted poll was consumed; a fresh call returns the fallback None.
        let again = client
            .poll_for_work("env-1", "secret", PollOptions::default())
            .await
            .unwrap();
        assert!(again.is_none());
    }
}
