//! The bridge permission response shape and the in-process callback registry.
//!
//! Request, response and cancellation callbacks touch the transport and the
//! JSON layer, so they stay caller-owned. What lives here is the registration
//! side: request-scoped response handlers get an unsubscribe handle that
//! removes the registration before the eventual response can be delivered.
//!
//! The module also models:
//!
//! * [`BridgePermissionResponse`] — what the controller posts back (behavior
//!   discriminant, optional input/permission-update overrides, optional
//!   human-visible message).
//! * [`parse_behavior`] — checks a response's `behavior` discriminant and
//!   returns the parsed variant, refusing anything else.
//!
//! No JSON library is pulled in for this: `updated_input` and
//! `updated_permissions` travel as [`OpaqueJson`] — raw JSON text the bridge
//! forwards unchanged. A caller that wants typed access parses it at its own
//! boundary.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Discriminant of a [`BridgePermissionResponse`]: the wire strings
/// `"allow"` and `"deny"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BridgePermissionBehavior {
    /// Permission accepted.
    Allow,
    /// Permission rejected.
    Deny,
}

impl BridgePermissionBehavior {
    /// Wire string used on the bridge.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// Parse a wire `behavior` string into [`BridgePermissionBehavior`].
///
/// Performs response validation and returns
/// `None` for any value other than the two recognised discriminants.
pub fn parse_behavior(value: &str) -> Option<BridgePermissionBehavior> {
    match value {
        "allow" => Some(BridgePermissionBehavior::Allow),
        "deny" => Some(BridgePermissionBehavior::Deny),
        _ => None,
    }
}

/// Caller-opaque JSON payload.
///
/// Used for `updated_input` / `updated_permissions`: callers treat
/// these as rich typed objects (a JSON object of tool input and a
/// list of permission updates), but the bridge *messaging* layer — which is
/// what this crate handles — never introspects them. Keeping them opaque
/// means this crate stays JSON-dep-free while still round-tripping the
/// full web-app response.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OpaqueJson(pub String);

impl OpaqueJson {
    /// Construct from any string-like value.
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Borrow the raw JSON text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A permission response the web app posts back over the bridge.
///
/// * `behavior` — `"allow"` or `"deny"`
/// * `updated_input` — optional override of the tool input (web app
///   may edit arguments before approving)
/// * `updated_permissions` — optional opaque list of permission-update
///   fragments to install
/// * `message` — optional human-visible message attached to a deny
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgePermissionResponse {
    /// Discriminant — `allow` or `deny`.
    pub behavior: BridgePermissionBehavior,
    /// Optional override of the tool input.
    pub updated_input: Option<OpaqueJson>,
    /// Optional list of permission-update fragments.
    pub updated_permissions: Option<OpaqueJson>,
    /// Optional human-visible message.
    pub message: Option<String>,
}

impl BridgePermissionResponse {
    /// Minimal constructor — allow/deny with no overrides.
    pub fn new(behavior: BridgePermissionBehavior) -> Self {
        Self {
            behavior,
            updated_input: None,
            updated_permissions: None,
            message: None,
        }
    }

    /// Attach a message and return `self` (builder-style).
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Attach an `updated_input` blob and return `self`.
    pub fn with_updated_input(mut self, payload: OpaqueJson) -> Self {
        self.updated_input = Some(payload);
        self
    }

    /// Attach an `updated_permissions` blob and return `self`.
    pub fn with_updated_permissions(mut self, payload: OpaqueJson) -> Self {
        self.updated_permissions = Some(payload);
        self
    }
}

// ─── Callback registry ──────────────────────────────────────────────────────

/// Handler type installed via [`PermissionResponseRegistry::register`].
///
/// Boxed as `FnOnce` because a permission response arrives at most
/// once per request — the handler is invoked once then the slot is
/// freed.
pub type PermissionResponseHandler = Box<dyn FnOnce(BridgePermissionResponse) + Send + 'static>;

/// In-process registry that matches request IDs to pending permission
/// handlers, with request-scoped registration and an unsubscribe handle.
///
/// The transport-facing request, response and cancellation methods stay
/// outside this crate — they touch I/O — but the Rust matching logic lives
/// here so callers share one implementation instead of each growing its own
/// HashMap-behind-a-Mutex.
#[derive(Clone, Default)]
pub struct PermissionResponseRegistry {
    inner: Arc<Mutex<HashMap<String, PermissionResponseHandler>>>,
}

/// Outcome of [`PermissionResponseRegistry::deliver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// A handler was registered and was invoked with the response.
    Delivered,
    /// No handler was registered for that request id — response
    /// dropped. Late responses after a
    /// timeout-driven unsubscribe are silently discarded.
    NoHandler,
}

impl PermissionResponseRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler for `request_id`. Returns a
    /// [`RegistrationGuard`] — dropping it (or calling `unsubscribe`)
    /// removes the handler, matching the registry unsubscribe contract.
    ///
    /// If a handler was already registered for that id it is evicted,
    /// matching the last-writer-wins registry semantics.
    pub fn register<F>(&self, request_id: impl Into<String>, handler: F) -> RegistrationGuard
    where
        F: FnOnce(BridgePermissionResponse) + Send + 'static,
    {
        let id = request_id.into();
        {
            let mut guard = self.inner.lock().expect("permission registry poisoned");
            guard.insert(id.clone(), Box::new(handler));
        }
        RegistrationGuard {
            registry: self.clone(),
            request_id: Some(id),
        }
    }

    /// Deliver a response. If a handler is registered for `request_id`,
    /// it is removed and invoked with the response; otherwise the
    /// response is dropped and [`DeliveryOutcome::NoHandler`] is
    /// returned.
    pub fn deliver(&self, request_id: &str, response: BridgePermissionResponse) -> DeliveryOutcome {
        let handler = {
            let mut guard = self.inner.lock().expect("permission registry poisoned");
            guard.remove(request_id)
        };
        match handler {
            Some(h) => {
                h(response);
                DeliveryOutcome::Delivered
            }
            None => DeliveryOutcome::NoHandler,
        }
    }

    /// Cancel a pending handler without invoking it. Returns `true` if
    /// a handler was present and removed, `false` otherwise. Used for
    /// the cancel-request verb — the web app dismisses its own prompt
    /// and nothing else happens locally.
    pub fn cancel(&self, request_id: &str) -> bool {
        let mut guard = self.inner.lock().expect("permission registry poisoned");
        guard.remove(request_id).is_some()
    }

    /// Number of pending handlers (diagnostic / test helper).
    pub fn pending_count(&self) -> usize {
        let guard = self.inner.lock().expect("permission registry poisoned");
        guard.len()
    }

    /// True when a handler is registered for `request_id`.
    pub fn has(&self, request_id: &str) -> bool {
        let guard = self.inner.lock().expect("permission registry poisoned");
        guard.contains_key(request_id)
    }
}

/// RAII unsubscribe guard returned by
/// [`PermissionResponseRegistry::register`].
///
/// Dropping the guard unsubscribes the handler. You can also call
/// [`RegistrationGuard::unsubscribe`] explicitly or
/// [`RegistrationGuard::forget`] to keep the handler installed past the
/// lifetime of the guard (matching the registry pattern where callers that
/// *want* automatic cleanup capture the returned closure and callers
/// that want fire-and-forget discard it).
pub struct RegistrationGuard {
    registry: PermissionResponseRegistry,
    request_id: Option<String>,
}

impl RegistrationGuard {
    /// Explicitly unsubscribe. Idempotent — a second call is a no-op.
    pub fn unsubscribe(mut self) -> bool {
        match self.request_id.take() {
            Some(id) => self.registry.cancel(&id),
            None => false,
        }
    }

    /// Keep the handler installed past the guard's lifetime. The
    /// handler is then only removed by `deliver` or `cancel`.
    pub fn forget(mut self) {
        self.request_id = None;
    }
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        if let Some(id) = self.request_id.take() {
            let _ = self.registry.cancel(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn behavior_parser_accepts_allow() {
        assert_eq!(
            parse_behavior("allow"),
            Some(BridgePermissionBehavior::Allow)
        );
    }

    #[test]
    fn behavior_parser_accepts_deny() {
        assert_eq!(parse_behavior("deny"), Some(BridgePermissionBehavior::Deny));
    }

    #[test]
    fn behavior_parser_rejects_unknown() {
        assert_eq!(parse_behavior(""), None);
        assert_eq!(parse_behavior("Allow"), None); // case-sensitive
        assert_eq!(parse_behavior("allowed"), None);
        assert_eq!(parse_behavior("prompt"), None);
    }

    #[test]
    fn behavior_as_str_roundtrip() {
        assert_eq!(BridgePermissionBehavior::Allow.as_str(), "allow");
        assert_eq!(BridgePermissionBehavior::Deny.as_str(), "deny");
        assert_eq!(
            parse_behavior(BridgePermissionBehavior::Allow.as_str()),
            Some(BridgePermissionBehavior::Allow)
        );
    }

    #[test]
    fn response_minimal_constructor_leaves_options_empty() {
        let r = BridgePermissionResponse::new(BridgePermissionBehavior::Allow);
        assert_eq!(r.behavior, BridgePermissionBehavior::Allow);
        assert!(r.updated_input.is_none());
        assert!(r.updated_permissions.is_none());
        assert!(r.message.is_none());
    }

    #[test]
    fn response_builder_composes_fields() {
        let r = BridgePermissionResponse::new(BridgePermissionBehavior::Deny)
            .with_message("nope")
            .with_updated_input(OpaqueJson::new("{\"a\":1}"))
            .with_updated_permissions(OpaqueJson::new("[]"));
        assert_eq!(r.behavior, BridgePermissionBehavior::Deny);
        assert_eq!(r.message.as_deref(), Some("nope"));
        assert_eq!(
            r.updated_input.as_ref().map(OpaqueJson::as_str),
            Some("{\"a\":1}")
        );
        assert_eq!(
            r.updated_permissions.as_ref().map(OpaqueJson::as_str),
            Some("[]")
        );
    }

    #[test]
    fn register_then_deliver_invokes_handler_once() {
        let reg = PermissionResponseRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&counter);
        let guard = reg.register("req-1", move |resp| {
            assert_eq!(resp.behavior, BridgePermissionBehavior::Allow);
            captured.fetch_add(1, Ordering::SeqCst);
        });
        // Keep the registration installed — otherwise dropping the
        // guard would immediately remove the handler.
        guard.forget();
        assert_eq!(reg.pending_count(), 1);
        assert!(reg.has("req-1"));

        let outcome = reg.deliver(
            "req-1",
            BridgePermissionResponse::new(BridgePermissionBehavior::Allow),
        );
        assert_eq!(outcome, DeliveryOutcome::Delivered);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(reg.pending_count(), 0);
        assert!(!reg.has("req-1"));
    }

    #[test]
    fn deliver_unknown_request_returns_no_handler() {
        let reg = PermissionResponseRegistry::new();
        let outcome = reg.deliver(
            "nope",
            BridgePermissionResponse::new(BridgePermissionBehavior::Allow),
        );
        assert_eq!(outcome, DeliveryOutcome::NoHandler);
    }

    #[test]
    fn cancel_removes_handler_without_invoking() {
        let reg = PermissionResponseRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&counter);
        reg.register("req-1", move |_| {
            captured.fetch_add(1, Ordering::SeqCst);
        })
        .forget();
        assert!(reg.cancel("req-1"));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert_eq!(reg.pending_count(), 0);

        // Second cancel is a no-op.
        assert!(!reg.cancel("req-1"));
    }

    #[test]
    fn dropping_registration_guard_unsubscribes() {
        let reg = PermissionResponseRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&counter);
        {
            let _guard = reg.register("req-1", move |_| {
                captured.fetch_add(1, Ordering::SeqCst);
            });
            assert_eq!(reg.pending_count(), 1);
        } // drop here
        assert_eq!(reg.pending_count(), 0);
        let outcome = reg.deliver(
            "req-1",
            BridgePermissionResponse::new(BridgePermissionBehavior::Allow),
        );
        assert_eq!(outcome, DeliveryOutcome::NoHandler);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn guard_unsubscribe_is_idempotent_and_reports_state() {
        let reg = PermissionResponseRegistry::new();
        let guard = reg.register("req-1", |_| {});
        assert!(guard.unsubscribe());
    }

    #[test]
    fn re_register_same_id_evicts_previous_handler() {
        let reg = PermissionResponseRegistry::new();
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let f1 = Arc::clone(&first_calls);
        let f2 = Arc::clone(&second_calls);

        reg.register("req-1", move |_| {
            f1.fetch_add(1, Ordering::SeqCst);
        })
        .forget();
        reg.register("req-1", move |_| {
            f2.fetch_add(1, Ordering::SeqCst);
        })
        .forget();
        assert_eq!(reg.pending_count(), 1);

        reg.deliver(
            "req-1",
            BridgePermissionResponse::new(BridgePermissionBehavior::Deny),
        );
        assert_eq!(first_calls.load(Ordering::SeqCst), 0);
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn many_pending_handlers_do_not_interfere() {
        let reg = PermissionResponseRegistry::new();
        let a_hits = Arc::new(AtomicUsize::new(0));
        let b_hits = Arc::new(AtomicUsize::new(0));
        let ha = Arc::clone(&a_hits);
        let hb = Arc::clone(&b_hits);

        reg.register("req-a", move |r| {
            assert_eq!(r.behavior, BridgePermissionBehavior::Allow);
            ha.fetch_add(1, Ordering::SeqCst);
        })
        .forget();
        reg.register("req-b", move |r| {
            assert_eq!(r.behavior, BridgePermissionBehavior::Deny);
            hb.fetch_add(1, Ordering::SeqCst);
        })
        .forget();
        assert_eq!(reg.pending_count(), 2);

        reg.deliver(
            "req-b",
            BridgePermissionResponse::new(BridgePermissionBehavior::Deny),
        );
        assert_eq!(reg.pending_count(), 1);
        assert!(reg.has("req-a"));
        assert!(!reg.has("req-b"));

        reg.deliver(
            "req-a",
            BridgePermissionResponse::new(BridgePermissionBehavior::Allow),
        );
        assert_eq!(a_hits.load(Ordering::SeqCst), 1);
        assert_eq!(b_hits.load(Ordering::SeqCst), 1);
        assert_eq!(reg.pending_count(), 0);
    }

    #[test]
    fn registry_is_clone_and_shares_state() {
        let reg = PermissionResponseRegistry::new();
        let clone = reg.clone();
        let hits = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&hits);
        reg.register("req-1", move |_| {
            captured.fetch_add(1, Ordering::SeqCst);
        })
        .forget();
        // Clone sees the same pending handler.
        assert_eq!(clone.pending_count(), 1);
        // Deliver via the clone — original observer should see the
        // invocation.
        clone.deliver(
            "req-1",
            BridgePermissionResponse::new(BridgePermissionBehavior::Allow),
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(reg.pending_count(), 0);
    }
}
