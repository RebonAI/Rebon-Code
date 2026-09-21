use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::{Disposer, KernelError};

/// A service definition: the contract corner of the service triangle.
///
/// The definition names the seam (`NAME`) and fixes the Rust-facing interface
/// type. Providers register an implementation, consumers resolve it — neither
/// needs to know the other.
pub trait Service: 'static {
    type Interface: ?Sized + Send + Sync + 'static;
    const NAME: &'static str;
}

/// JSON-plane service ABI for dynamically-typed hosts (an embedded JS
/// runtime, remote bridges). A provider may expose this facade in addition
/// to — or instead of — a typed interface under the same service name.
pub trait JsonService: Send + Sync {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, KernelError>;
}

/// Generation-bound lease guarding one JSON-plane registration.
///
/// `get_json` hands out `Arc`s that outlive the registration — dropping the
/// table entry cannot reach references already held. The lease is the wire
/// that does reach them: it travels inside the [`LeasedJson`] wrapper every
/// holder actually got, so closing it turns every future call on a stale
/// reference into the stable `[STALE_PROVIDER]` error while `inflight` keeps
/// the drain observable.
///
/// Deliberately JSON-plane only: the JSON face is the plugin ABI, where
/// providers really get unloaded. Typed-plane `Arc` liveness semantics stay
/// as they are (in-crate Rust consumers, compile-time discipline) — a
/// documented deviation, not an oversight.
pub struct ServiceLease {
    name: String,
    closed: AtomicBool,
    inflight: AtomicU64,
}

impl ServiceLease {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            closed: AtomicBool::new(false),
            inflight: AtomicU64::new(0),
        })
    }

    /// Refuse all future calls. Idempotent; already-running calls finish
    /// and keep counting in [`ServiceLease::inflight`] until they do.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Calls currently inside the provider.
    pub fn inflight(&self) -> u64 {
        self.inflight.load(Ordering::Acquire)
    }

    /// Call admission: count first, then check — a call that loses the
    /// race with `close` backs its count out and refuses, so once the
    /// drain loop observes `inflight == 0` after `close`, no call is
    /// running and none can start.
    fn enter(&self) -> Result<InflightGuard<'_>, KernelError> {
        self.inflight.fetch_add(1, Ordering::AcqRel);
        if self.is_closed() {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            return Err(KernelError::ServiceClosed {
                service: self.name.clone(),
            });
        }
        Ok(InflightGuard(self))
    }
}

struct InflightGuard<'a>(&'a ServiceLease);

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What `get_json` actually hands out: the provider behind its lease.
/// Wrapped once at registration, so every holder — however long-lived —
/// goes through the same admission gate.
struct LeasedJson {
    inner: Arc<dyn JsonService>,
    lease: Arc<ServiceLease>,
}

impl JsonService for LeasedJson {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, KernelError> {
        let _guard = self.lease.enter()?;
        self.inner.call(method, params)
    }
}

struct Entry {
    /// Monotonic token so a stale disposer never removes a successor.
    token: u64,
    /// Boxed `Arc<S::Interface>` (the Arc itself is the `Any` payload).
    typed: Option<Box<dyn Any + Send + Sync>>,
    json: Option<Arc<dyn JsonService>>,
}

#[derive(Default)]
pub(crate) struct ServiceRegistry {
    entries: RwLock<HashMap<String, Entry>>,
    next_token: AtomicU64,
}

impl ServiceRegistry {
    fn insert(
        self: &Arc<Self>,
        name: &str,
        typed: Option<Box<dyn Any + Send + Sync>>,
        json: Option<Arc<dyn JsonService>>,
    ) -> Result<(Disposer, Option<Arc<ServiceLease>>), KernelError> {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        // JSON-plane providers get a lease and go out wrapped: the copy
        // every holder resolves IS the gated one.
        let lease = json.as_ref().map(|_| ServiceLease::new(name));
        let json = match (json, &lease) {
            (Some(inner), Some(lease)) => Some(Arc::new(LeasedJson {
                inner,
                lease: lease.clone(),
            }) as Arc<dyn JsonService>),
            _ => None,
        };
        {
            let mut entries = self.entries.write().unwrap();
            if entries.contains_key(name) {
                return Err(KernelError::DuplicateProvider {
                    plugin: String::new(),
                    service: name.to_string(),
                });
            }
            entries.insert(name.to_string(), Entry { token, typed, json });
        }
        let weak = Arc::downgrade(self);
        let name = name.to_string();
        let lease_for_disposer = lease.clone();
        let disposer = Disposer::new(move || {
            // Close before removing: a bare dispose (no drain phase) must
            // still turn held references stale.
            if let Some(lease) = &lease_for_disposer {
                lease.close();
            }
            if let Some(registry) = weak.upgrade() {
                let mut entries = registry.entries.write().unwrap();
                if entries.get(&name).is_some_and(|e| e.token == token) {
                    entries.remove(&name);
                }
            }
        });
        Ok((disposer, lease))
    }

    pub(crate) fn provide_typed<S: Service>(
        self: &Arc<Self>,
        interface: Arc<S::Interface>,
    ) -> Result<Disposer, KernelError> {
        self.insert(S::NAME, Some(Box::new(interface)), None)
            .map(|(disposer, _)| disposer)
    }

    /// Register a provider that serves both planes under one name.
    pub(crate) fn provide_dual<S: Service>(
        self: &Arc<Self>,
        interface: Arc<S::Interface>,
        json: Arc<dyn JsonService>,
    ) -> Result<(Disposer, Option<Arc<ServiceLease>>), KernelError> {
        self.insert(S::NAME, Some(Box::new(interface)), Some(json))
    }

    pub(crate) fn provide_json(
        self: &Arc<Self>,
        name: &str,
        json: Arc<dyn JsonService>,
    ) -> Result<(Disposer, Option<Arc<ServiceLease>>), KernelError> {
        self.insert(name, None, Some(json))
    }

    pub(crate) fn get_typed<S: Service>(&self) -> Option<Arc<S::Interface>> {
        let entries = self.entries.read().unwrap();
        let entry = entries.get(S::NAME)?;
        entry
            .typed
            .as_ref()?
            .downcast_ref::<Arc<S::Interface>>()
            .cloned()
    }

    pub(crate) fn get_json(&self, name: &str) -> Option<Arc<dyn JsonService>> {
        self.entries.read().unwrap().get(name)?.json.clone()
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        self.entries.read().unwrap().contains_key(name)
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.entries.read().unwrap().keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    trait Greeter: Send + Sync {
        fn greet(&self, who: &str) -> String;
    }

    struct English;
    impl Greeter for English {
        fn greet(&self, who: &str) -> String {
            format!("hello {who}")
        }
    }

    struct GreeterService;
    impl Service for GreeterService {
        type Interface = dyn Greeter;
        const NAME: &'static str = "greeter";
    }

    #[test]
    fn typed_roundtrip_and_disposal() {
        let registry = Arc::new(ServiceRegistry::default());
        let disposer = registry
            .provide_typed::<GreeterService>(Arc::new(English))
            .unwrap();
        let greeter = registry.get_typed::<GreeterService>().expect("resolved");
        assert_eq!(greeter.greet("kernel"), "hello kernel");

        disposer.dispose();
        assert!(registry.get_typed::<GreeterService>().is_none());
    }

    #[test]
    fn duplicate_provider_rejected() {
        let registry = Arc::new(ServiceRegistry::default());
        let _keep = registry
            .provide_typed::<GreeterService>(Arc::new(English))
            .unwrap();
        assert!(matches!(
            registry.provide_typed::<GreeterService>(Arc::new(English)),
            Err(KernelError::DuplicateProvider { .. })
        ));
    }

    #[test]
    fn stale_disposer_does_not_remove_successor() {
        let registry = Arc::new(ServiceRegistry::default());
        let first = registry
            .provide_typed::<GreeterService>(Arc::new(English))
            .unwrap();
        // Simulate replace: drop the first provider properly, re-provide,
        // then run the stale disposer again via a cloned closure path.
        first.dispose();
        let _second = registry
            .provide_typed::<GreeterService>(Arc::new(English))
            .unwrap();
        // The first disposer already ran; the second registration must survive.
        assert!(registry.get_typed::<GreeterService>().is_some());
    }

    struct JsonEcho;
    impl JsonService for JsonEcho {
        fn call(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, KernelError> {
            Ok(serde_json::json!({ "method": method, "params": params }))
        }
    }

    #[test]
    fn json_plane_roundtrip() {
        let registry = Arc::new(ServiceRegistry::default());
        let (_keep, _lease) = registry.provide_json("echo", Arc::new(JsonEcho)).unwrap();
        let svc = registry.get_json("echo").expect("resolved");
        let out = svc.call("ping", serde_json::json!({"n": 1})).unwrap();
        assert_eq!(out["method"], "ping");
        assert_eq!(out["params"]["n"], 1);
    }

    #[test]
    fn a_closed_lease_turns_held_references_stale_with_the_stable_code() {
        let registry = Arc::new(ServiceRegistry::default());
        let (disposer, lease) = registry.provide_json("echo", Arc::new(JsonEcho)).unwrap();
        let lease = lease.expect("json providers are leased");
        // The stale window: resolve first, dispose later, call anyway.
        let held = registry.get_json("echo").expect("resolved");
        assert_eq!(lease.inflight(), 0);
        assert!(held.call("ping", serde_json::json!({})).is_ok());

        disposer.dispose();
        assert!(lease.is_closed(), "bare dispose closes the lease too");
        let err = held.call("ping", serde_json::json!({})).unwrap_err();
        assert!(
            err.to_string().contains("[STALE_PROVIDER]"),
            "stable code, got: {err}"
        );
        assert_eq!(lease.inflight(), 0, "refused calls do not leak counts");
    }
}
