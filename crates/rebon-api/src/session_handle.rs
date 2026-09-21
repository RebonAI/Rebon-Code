//! `SessionHandle` — the owner of a model session's mutable state.
//!
//! [`ModelClient`] carries four methods that have nothing to do with
//! "send a request and stream the answer": `reset_session_state`,
//! `end_turn`, `invalidate_previous_response_id`, and
//! `fork_for_sub_agent_with_cache_key`. They exist because
//! server-side continuation (OpenAI Responses' `previous_response_id`,
//! a live WebSocket, an external plugin's child process) has a
//! lifecycle that outlives a single request, and the client happened
//! to be the only object every caller already held.
//!
//! Hanging that lifecycle off the transport has two costs:
//!
//! 1. **Every caller can fire it.** `client.end_turn()` is reachable
//!    from anything holding an `Arc<dyn ModelClient>`, including a
//!    sub-agent that was handed a *clone* of its parent's client
//!    because [`ModelClient::fork_for_sub_agent`] returned `None`.
//!    The child's turn ending then tears down the parent's session.
//! 2. **A session is not necessarily a `ModelClient`.** A third-party
//!    agent reached over ACP has sessions too — created, resumed,
//!    cancelled — but no `create_message_stream` anywhere in sight.
//!
//! [`SessionHandle`] fixes both. It owns the `Arc<dyn ModelClient>`
//! and is the only thing callers are given; the lifecycle verbs live
//! on the handle, and each handle knows whether it actually *owns*
//! the session state it would be resetting.
//!
//! # Ownership
//!
//! A handle built with [`SessionHandle::new`] is **owned**: it is the
//! sole holder of that client's session state, so every lifecycle
//! call is forwarded.
//!
//! [`SessionHandle::fork_for_sub_agent`] asks the client for an
//! isolated child. When the client provides one, the child handle is
//! owned too. When it returns `None` — the client has no session
//! state to isolate, *or* it has some but cannot split it — the child
//! handle shares the parent's `Arc` and is marked **borrowed**:
//! lifecycle calls on it are dropped rather than forwarded. For a
//! genuinely stateless client that is what the no-op default did
//! anyway; for a stateful-but-unforkable one (an external plugin
//! provider, whose `endTurn`/`reset` notifications are
//! connection-scoped and carry no stream id) it is the difference
//! between a sub-agent quietly resetting its parent mid-turn and not.
//!
//! # Registry
//!
//! [`SessionRegistry`] maps a session key to the live handle for that
//! session, holding it weakly so a finished session evicts itself and
//! reference counting — not a timer — decides when an external
//! provider's child process goes away.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::client::ModelClient;
use crate::context_prune::PruneLevelHandle;

/// Process-unique identifier for a [`SessionHandle`].
///
/// Handles are compared by this id rather than by pointer so a handle
/// can be tracked through a registry without keeping it alive.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionHandleId(u64);

impl SessionHandleId {
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Raw numeric value, for logs and trace fields.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for SessionHandleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session#{}", self.0)
    }
}

impl fmt::Display for SessionHandleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session#{}", self.0)
    }
}

/// Whether a handle owns the session-scoped state of its client.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ownership {
    /// This handle is the sole owner — lifecycle calls are forwarded.
    Owned,
    /// The client is shared with the parent handle, which owns the
    /// session state — lifecycle calls are dropped.
    Borrowed,
}

/// A live model session: a client plus the lifecycle verbs that act
/// on the state behind it.
pub struct SessionHandle {
    id: SessionHandleId,
    client: Arc<dyn ModelClient>,
    ownership: Ownership,
    /// Prompt cache key this handle was forked with, if any. Kept for
    /// diagnostics — the key itself is already baked into the forked
    /// client.
    prompt_cache_key: Option<String>,
    /// Handle this one was forked from, if any.
    parent: Option<SessionHandleId>,
}

impl SessionHandle {
    /// Wrap a client whose session state this handle owns.
    pub fn new(client: Arc<dyn ModelClient>) -> Arc<Self> {
        Arc::new(Self {
            id: SessionHandleId::next(),
            client,
            ownership: Ownership::Owned,
            prompt_cache_key: None,
            parent: None,
        })
    }

    /// Wrap a client whose session state is owned somewhere else.
    ///
    /// Lifecycle calls on the returned handle are dropped. Use this
    /// when a component needs to issue requests over a client it did
    /// not create and must not reset — the sub-agent fallback path
    /// gets one of these via [`Self::fork_for_sub_agent`].
    pub fn borrowed(client: Arc<dyn ModelClient>) -> Arc<Self> {
        Arc::new(Self {
            id: SessionHandleId::next(),
            client,
            ownership: Ownership::Borrowed,
            prompt_cache_key: None,
            parent: None,
        })
    }

    pub fn id(&self) -> SessionHandleId {
        self.id
    }

    /// The underlying client, for issuing requests.
    pub fn client(&self) -> &Arc<dyn ModelClient> {
        &self.client
    }

    /// Cheap clone of the underlying client `Arc`.
    pub fn client_arc(&self) -> Arc<dyn ModelClient> {
        self.client.clone()
    }

    pub fn provider_name(&self) -> &'static str {
        self.client.provider_name()
    }

    /// Whether lifecycle calls on this handle reach the client.
    pub fn owns_session_state(&self) -> bool {
        self.ownership == Ownership::Owned
    }

    /// The handle this one was forked from, if any.
    pub fn parent(&self) -> Option<SessionHandleId> {
        self.parent
    }

    /// Prompt cache key this handle was forked with, if any.
    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.prompt_cache_key.as_deref()
    }

    /// Context-prune handle for this session's client stack, if any.
    pub fn context_prune_handle(&self) -> Option<PruneLevelHandle> {
        self.client.context_prune_handle()
    }

    /// Start a fresh conversation on this session.
    ///
    /// Drops every piece of session-scoped state — continuation ids,
    /// long-lived transports — so nothing from the previous
    /// conversation leaks into the next one. See
    /// [`ModelClient::reset_session_state`].
    pub fn reset_session(&self) {
        if self.owns_session_state() {
            self.client.reset_session_state();
        }
    }

    /// Report that a turn finished (success, error, or cancel).
    ///
    /// See [`ModelClient::end_turn`] — this is the per-turn
    /// counterpart to [`Self::reset_session`] and must not tear down
    /// transports that the next turn can still validate.
    pub fn end_turn(&self) {
        if self.owns_session_state() {
            self.client.end_turn();
        }
    }

    /// Drop the server-side continuation chain without touching the
    /// transport. See [`ModelClient::invalidate_previous_response_id`].
    pub fn invalidate_continuation(&self) {
        if self.owns_session_state() {
            self.client.invalidate_previous_response_id();
        }
    }

    /// Derive an isolated session for a sub-agent.
    ///
    /// When the client can fork, the child handle owns its own
    /// session state. When it cannot, the child shares this handle's
    /// client and is marked borrowed so its turns never reset ours.
    pub fn fork_for_sub_agent(self: &Arc<Self>, prompt_cache_key: Option<String>) -> Arc<Self> {
        match self
            .client
            .fork_for_sub_agent_with_cache_key(prompt_cache_key.clone())
        {
            Some(client) => Arc::new(Self {
                id: SessionHandleId::next(),
                client,
                ownership: Ownership::Owned,
                prompt_cache_key,
                parent: Some(self.id),
            }),
            None => Arc::new(Self {
                id: SessionHandleId::next(),
                client: self.client.clone(),
                ownership: Ownership::Borrowed,
                prompt_cache_key,
                parent: Some(self.id),
            }),
        }
    }
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionHandle")
            .field("id", &self.id)
            .field("provider", &self.client.provider_name())
            .field("ownership", &self.ownership)
            .field("parent", &self.parent)
            .finish()
    }
}

/// Live sessions, keyed by whatever string the caller uses to name a
/// session (the engine uses the session id).
///
/// Entries are weak: the registry never keeps a session alive, it
/// only lets one be found again. That matters for external plugin
/// providers, whose child process dies when the last `Arc` to the
/// client drops — a strong entry here would keep every plugin the
/// process ever spoke to running until exit.
#[derive(Default)]
pub struct SessionRegistry {
    inner: Mutex<HashMap<String, Weak<SessionHandle>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `handle` under `key`, replacing any previous entry.
    pub fn insert(&self, key: impl Into<String>, handle: &Arc<SessionHandle>) {
        let mut inner = self.lock();
        inner.retain(|_, weak| weak.strong_count() > 0);
        inner.insert(key.into(), Arc::downgrade(handle));
    }

    /// Look up a live handle. Returns `None` when the session was
    /// never registered or has since been dropped.
    pub fn get(&self, key: &str) -> Option<Arc<SessionHandle>> {
        self.lock().get(key).and_then(Weak::upgrade)
    }

    /// Look up a live handle, or build and register one.
    pub fn get_or_insert_with(
        &self,
        key: impl Into<String>,
        make: impl FnOnce() -> Arc<SessionHandle>,
    ) -> Arc<SessionHandle> {
        let key = key.into();
        let mut inner = self.lock();
        if let Some(existing) = inner.get(&key).and_then(Weak::upgrade) {
            return existing;
        }
        let handle = make();
        inner.retain(|_, weak| weak.strong_count() > 0);
        inner.insert(key, Arc::downgrade(&handle));
        handle
    }

    /// Forget `key`, returning the handle if it was still live.
    pub fn remove(&self, key: &str) -> Option<Arc<SessionHandle>> {
        self.lock().remove(key).and_then(|weak| weak.upgrade())
    }

    /// Number of registered keys whose handle is still alive.
    pub fn live_len(&self) -> usize {
        self.lock()
            .values()
            .filter(|weak| weak.strong_count() > 0)
            .count()
    }

    /// Drop entries whose handle is gone.
    pub fn gc(&self) {
        self.lock().retain(|_, weak| weak.strong_count() > 0);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Weak<SessionHandle>>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl fmt::Debug for SessionRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionRegistry")
            .field("live", &self.live_len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockModelClient;

    fn mock() -> (Arc<MockModelClient>, Arc<dyn ModelClient>) {
        let mock = Arc::new(MockModelClient::new());
        let client: Arc<dyn ModelClient> = mock.clone();
        (mock, client)
    }

    /// A client that has session state but refuses to fork — the
    /// shape an external plugin provider has today.
    struct UnforkableClient {
        inner: Arc<MockModelClient>,
    }

    #[async_trait::async_trait]
    impl ModelClient for UnforkableClient {
        fn provider_name(&self) -> &'static str {
            "unforkable"
        }

        async fn create_message_stream(
            &self,
            request: crate::request::CreateMessageRequest,
        ) -> crate::error::ModelResult<crate::events::StreamEventStream> {
            self.inner.create_message_stream(request).await
        }

        fn end_turn(&self) {
            self.inner.end_turn();
        }

        fn reset_session_state(&self) {
            self.inner.reset_session_state();
        }

        fn invalidate_previous_response_id(&self) {
            self.inner.invalidate_previous_response_id();
        }
    }

    /// A client that forks into a distinct child.
    struct ForkableClient {
        inner: Arc<MockModelClient>,
        child: Arc<MockModelClient>,
    }

    #[async_trait::async_trait]
    impl ModelClient for ForkableClient {
        fn provider_name(&self) -> &'static str {
            "forkable"
        }

        async fn create_message_stream(
            &self,
            request: crate::request::CreateMessageRequest,
        ) -> crate::error::ModelResult<crate::events::StreamEventStream> {
            self.inner.create_message_stream(request).await
        }

        fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
            Some(self.child.clone() as Arc<dyn ModelClient>)
        }

        fn end_turn(&self) {
            self.inner.end_turn();
        }
    }

    #[test]
    fn owned_handle_forwards_lifecycle_calls() {
        let (mock, client) = mock();
        let session = SessionHandle::new(client);

        session.reset_session();
        session.end_turn();
        session.invalidate_continuation();

        assert_eq!(mock.reset_count(), 1);
        assert_eq!(mock.end_turn_count(), 1);
        assert_eq!(mock.invalidate_previous_response_id_count(), 1);
        assert!(session.owns_session_state());
    }

    #[test]
    fn borrowed_handle_drops_lifecycle_calls() {
        let (mock, client) = mock();
        let session = SessionHandle::borrowed(client);

        session.reset_session();
        session.end_turn();
        session.invalidate_continuation();

        assert_eq!(mock.reset_count(), 0);
        assert_eq!(mock.end_turn_count(), 0);
        assert_eq!(mock.invalidate_previous_response_id_count(), 0);
        assert!(!session.owns_session_state());
    }

    #[test]
    fn fork_of_unforkable_client_cannot_reset_the_parent() {
        let inner = Arc::new(MockModelClient::new());
        let client: Arc<dyn ModelClient> = Arc::new(UnforkableClient {
            inner: inner.clone(),
        });
        let parent = SessionHandle::new(client);

        let child = parent.fork_for_sub_agent(Some("child-key".into()));

        // The child had to share the parent's transport…
        assert!(Arc::ptr_eq(child.client(), parent.client()));
        // …so its turn ending must not reach the parent's state.
        child.end_turn();
        child.reset_session();
        child.invalidate_continuation();
        assert_eq!(inner.end_turn_count(), 0);
        assert_eq!(inner.reset_count(), 0);
        assert_eq!(inner.invalidate_previous_response_id_count(), 0);

        // The parent still owns its own lifecycle.
        parent.end_turn();
        assert_eq!(inner.end_turn_count(), 1);
        assert_eq!(child.parent(), Some(parent.id()));
        assert_eq!(child.prompt_cache_key(), Some("child-key"));
    }

    #[test]
    fn fork_of_forkable_client_owns_its_own_state() {
        let inner = Arc::new(MockModelClient::new());
        let child_mock = Arc::new(MockModelClient::new());
        let client: Arc<dyn ModelClient> = Arc::new(ForkableClient {
            inner: inner.clone(),
            child: child_mock.clone(),
        });
        let parent = SessionHandle::new(client);

        let child = parent.fork_for_sub_agent(None);
        assert!(child.owns_session_state());
        assert!(!Arc::ptr_eq(child.client(), parent.client()));

        child.end_turn();
        assert_eq!(child_mock.end_turn_count(), 1);
        assert_eq!(inner.end_turn_count(), 0);
    }

    #[test]
    fn handle_ids_are_unique() {
        let (_, a) = mock();
        let (_, b) = mock();
        assert_ne!(SessionHandle::new(a).id(), SessionHandle::new(b).id());
    }

    #[test]
    fn registry_finds_live_handles_and_forgets_dead_ones() {
        let registry = SessionRegistry::new();
        let (_, client) = mock();
        let session = SessionHandle::new(client);
        registry.insert("sess-1", &session);

        let found = registry.get("sess-1").expect("handle should be live");
        assert_eq!(found.id(), session.id());
        assert_eq!(registry.live_len(), 1);

        drop(found);
        drop(session);
        assert!(registry.get("sess-1").is_none());
        assert_eq!(registry.live_len(), 0);
    }

    #[test]
    fn registry_get_or_insert_reuses_the_live_handle() {
        let registry = SessionRegistry::new();
        let (_, client) = mock();
        let first = registry.get_or_insert_with("sess", || SessionHandle::new(client.clone()));
        let second = registry.get_or_insert_with("sess", || {
            panic!("must reuse the live handle instead of rebuilding it")
        });
        assert_eq!(first.id(), second.id());
    }

    #[test]
    fn registry_rebuilds_after_the_handle_drops() {
        let registry = SessionRegistry::new();
        let (_, client) = mock();
        let first_id = {
            let handle = registry.get_or_insert_with("sess", || SessionHandle::new(client.clone()));
            handle.id()
        };
        let rebuilt = registry.get_or_insert_with("sess", || SessionHandle::new(client.clone()));
        assert_ne!(rebuilt.id(), first_id);
    }

    #[test]
    fn registry_remove_returns_the_live_handle() {
        let registry = SessionRegistry::new();
        let (_, client) = mock();
        let session = SessionHandle::new(client);
        registry.insert("sess", &session);
        let removed = registry.remove("sess").expect("still live");
        assert_eq!(removed.id(), session.id());
        assert!(registry.get("sess").is_none());
    }
}
