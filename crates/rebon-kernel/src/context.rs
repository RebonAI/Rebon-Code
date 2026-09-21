use std::sync::Arc;

use crate::disposer::{Disposer, Scope};
use crate::events::{EventBus, JsonNext, Next};
use crate::service::{JsonService, Service, ServiceRegistry};
use crate::KernelError;

/// Shared kernel state every context in the tree points at.
pub(crate) struct Shared {
    pub(crate) events: Arc<EventBus>,
}

/// One link in the service-scope chain. Lookups walk from the context's own
/// layer toward the root; registrations land in the context's layer.
pub(crate) struct ServiceLayer {
    registry: Arc<ServiceRegistry>,
    parent: Option<Arc<ServiceLayer>>,
}

impl ServiceLayer {
    fn root() -> Arc<Self> {
        Arc::new(Self {
            registry: Arc::new(ServiceRegistry::default()),
            parent: None,
        })
    }

    fn child(self: &Arc<Self>) -> Arc<Self> {
        Arc::new(Self {
            registry: Arc::new(ServiceRegistry::default()),
            parent: Some(self.clone()),
        })
    }
}

/// The set of service names a plugin's context subtree may resolve.
///
/// Built by the loader from the manifest (provides ∪ inject ∪
/// optional_inject) and extended dynamically by the subtree's own
/// registrations — a plugin can always resolve what it provided, even
/// undeclared. One plugin, one set, shared by that plugin's whole subtree.
///
/// This blocks **ambient lookup** and nothing more: same-process plugins
/// remain trusted-but-buggy (an `Arc` once handed over cannot be clawed
/// back); real security boundaries are isolates or processes.
pub(crate) struct Visibility {
    names: std::sync::RwLock<std::collections::HashSet<String>>,
}

impl Visibility {
    fn new(names: std::collections::HashSet<String>) -> Arc<Self> {
        Arc::new(Self {
            names: std::sync::RwLock::new(names),
        })
    }

    fn allows(&self, name: &str) -> bool {
        self.names.read().unwrap().contains(name)
    }

    fn extend(&self, name: &str) {
        self.names.write().unwrap().insert(name.to_string());
    }
}

/// A node in the context tree. Cloning is cheap; forking creates a child
/// whose registrations live in their own [`Scope`] and unwind when that
/// scope is disposed — disposing a parent disposes its children first.
#[derive(Clone)]
pub struct Context {
    pub(crate) shared: Arc<Shared>,
    pub(crate) services: Arc<ServiceLayer>,
    pub(crate) scope: Arc<Scope>,
    /// Diagnostic label: root is "", plugins get their name, forks append.
    pub(crate) label: Arc<str>,
    /// `None` = unrestricted (host wiring); `Some` = a plugin subtree
    /// confined to its manifest. Inherited by every fork.
    pub(crate) visibility: Option<Arc<Visibility>>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl Context {
    pub(crate) fn root() -> Self {
        Self {
            shared: Arc::new(Shared {
                events: Arc::new(EventBus::default()),
            }),
            services: ServiceLayer::root(),
            scope: Arc::new(Scope::new()),
            label: Arc::from(""),
            visibility: None,
        }
    }

    /// Confine this context (and every fork made from it) to resolving only
    /// the named services — the loader calls this on a plugin's fork with
    /// its manifest names. The subtree's own registrations extend the set
    /// automatically.
    pub(crate) fn confined(mut self, names: std::collections::HashSet<String>) -> Self {
        self.visibility = Some(Visibility::new(names));
        self
    }

    fn allows(&self, name: &str) -> bool {
        self.visibility
            .as_ref()
            .is_none_or(|visibility| visibility.allows(name))
    }

    fn refuse(&self, service: &str) -> KernelError {
        let plugin = self.label.split('/').next().unwrap_or("").to_string();
        tracing::warn!(
            plugin = %plugin,
            service,
            "kernel: undeclared service resolution refused (F3 ACL)"
        );
        KernelError::UnauthorizedResolve {
            plugin,
            service: service.to_string(),
        }
    }

    /// The diagnostic label of this context ("" for the root).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Create a child context. The child's registrations belong to its own
    /// scope; disposing this (parent) context disposes the child too. The
    /// child shares this context's service namespace.
    pub fn fork(&self, label: &str) -> Context {
        self.fork_inner(label, self.services.clone())
    }

    /// Like [`fork`](Self::fork), but the child gets its own service layer:
    /// services it provides are visible to itself and its descendants only,
    /// may shadow inherited names, and vanish with the child. Lookups fall
    /// back through the parent chain. This is what session-scoped services
    /// (per-session model client, tool registry, …) build on.
    pub fn fork_scoped(&self, label: &str) -> Context {
        self.fork_inner(label, self.services.child())
    }

    fn fork_inner(&self, label: &str, services: Arc<ServiceLayer>) -> Context {
        let child_scope = Arc::new(Scope::new());
        let for_parent = child_scope.clone();
        // Tracked entry: once the child unwinds on its own (single-plugin
        // unload), the parent-side entry becomes prunable — load/unload
        // cycles must not grow the parent scope without bound.
        self.scope.push_tracked(
            &format!("fork({label})"),
            Disposer::new(move || for_parent.dispose()),
            child_scope.disposed_flag(),
        );
        self.scope.track_child(&child_scope);
        Context {
            shared: self.shared.clone(),
            services,
            scope: child_scope,
            label: if self.label.is_empty() {
                Arc::from(label)
            } else {
                Arc::from(format!("{}/{}", self.label, label))
            },
            // Confinement is subtree-wide: a plugin cannot fork its way
            // out of its manifest.
            visibility: self.visibility.clone(),
        }
    }

    /// Tear down everything registered through this context (and its forks).
    pub fn dispose(&self) {
        self.scope.dispose();
    }

    /// Generic effect: run a registration that yields a [`Disposer`] and tie
    /// its lifetime to this context's scope.
    pub fn effect(&self, register: impl FnOnce() -> Disposer) {
        self.scope.push(register());
    }

    /// [`Context::effect`] with a diagnostic label for
    /// [`Context::registration_labels`].
    pub fn effect_labeled(&self, label: &str, register: impl FnOnce() -> Disposer) {
        self.scope.push_labeled(label, register());
    }

    /// Labels of every live registration made through this context (not
    /// including registrations of child forks — enumerate those through the
    /// child). Diagnostics for the "registration leaves no residue" contract.
    pub fn registration_labels(&self) -> Vec<String> {
        self.scope.live_labels()
    }

    /// Live registration counts across every event-bus dispatch table.
    pub fn event_stats(&self) -> crate::EventBusStats {
        self.shared.events.stats()
    }

    // ---- services: typed plane ----

    pub fn provide<S: Service>(&self, interface: Arc<S::Interface>) -> Result<(), KernelError> {
        let disposer = self.services.registry.provide_typed::<S>(interface)?;
        self.extend_visibility(S::NAME);
        self.scope
            .push_labeled(&format!("provide({})", S::NAME), disposer);
        Ok(())
    }

    /// A confined subtree may always resolve what it registered itself,
    /// declared or not — self-consistency, not a widening (the name only
    /// enters this plugin's own set).
    fn extend_visibility(&self, name: &str) {
        if let Some(visibility) = &self.visibility {
            visibility.extend(name);
        }
    }

    /// Provide a service on both planes at once: the typed interface for
    /// Rust consumers plus a JSON facade for dynamically-typed hosts.
    pub fn provide_dual<S: Service>(
        &self,
        interface: Arc<S::Interface>,
        json: Arc<dyn JsonService>,
    ) -> Result<(), KernelError> {
        let (disposer, lease) = self.services.registry.provide_dual::<S>(interface, json)?;
        self.extend_visibility(S::NAME);
        if let Some(lease) = &lease {
            self.scope.track_lease(lease);
        }
        self.scope
            .push_labeled(&format!("provide_dual({})", S::NAME), disposer);
        Ok(())
    }

    /// Walk the scope chain from this context's layer toward the root and
    /// apply `f` at the first layer that has ANY provider for `name`.
    fn resolve_layer<T>(&self, name: &str, f: impl Fn(&ServiceRegistry) -> Option<T>) -> Option<T> {
        let mut layer = Some(&self.services);
        while let Some(current) = layer {
            if current.registry.has(name) {
                return f(&current.registry);
            }
            layer = current.parent.as_ref();
        }
        None
    }

    pub fn get<S: Service>(&self) -> Option<Arc<S::Interface>> {
        if !self.allows(S::NAME) {
            drop(self.refuse(S::NAME));
            return None;
        }
        self.resolve_layer(S::NAME, |registry| registry.get_typed::<S>())
    }

    pub fn require<S: Service>(&self) -> Result<Arc<S::Interface>, KernelError> {
        if !self.allows(S::NAME) {
            return Err(self.refuse(S::NAME));
        }
        self.get::<S>()
            .ok_or_else(|| KernelError::ServiceNotFound(S::NAME.to_string()))
    }

    // ---- services: JSON plane ----

    pub fn provide_json(
        &self,
        name: &str,
        service: Arc<dyn JsonService>,
    ) -> Result<(), KernelError> {
        let (disposer, lease) = self.services.registry.provide_json(name, service)?;
        self.extend_visibility(name);
        if let Some(lease) = &lease {
            self.scope.track_lease(lease);
        }
        self.scope
            .push_labeled(&format!("provide_json({name})"), disposer);
        Ok(())
    }

    /// Close every JSON-plane lease registered through this context's
    /// subtree: held references start refusing with the stable
    /// `[STALE_PROVIDER]` error, running calls keep counting in
    /// [`Context::lease_inflight`]. The `Live → Closing` edge of the
    /// unload state machine (see `Kernel::unload_with`).
    pub fn close_leases(&self) {
        self.scope.close_leases();
    }

    /// In-flight JSON-plane calls across this context's subtree — the
    /// drain observable (`Closing → Drained` waits for zero).
    pub fn lease_inflight(&self) -> u64 {
        self.scope.lease_inflight()
    }

    pub fn get_json(&self, name: &str) -> Option<Arc<dyn JsonService>> {
        if !self.allows(name) {
            drop(self.refuse(name));
            return None;
        }
        self.resolve_layer(name, |registry| registry.get_json(name))
    }

    /// One-shot JSON-plane call: resolve + invoke.
    pub fn call_json(
        &self,
        service: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, KernelError> {
        if !self.allows(service) {
            return Err(self.refuse(service));
        }
        let svc = self
            .get_json(service)
            .ok_or_else(|| KernelError::ServiceNotFound(service.to_string()))?;
        svc.call(method, params)
    }

    /// Whether a service name has any provider (either plane) visible from
    /// this context's scope chain. A confined subtree cannot probe
    /// undeclared names (existence is information too).
    pub fn has_service(&self, name: &str) -> bool {
        self.allows(name) && self.resolve_layer(name, |_| Some(())).is_some()
    }

    /// All service names visible from this context (diagnostics). Shadowed
    /// names appear once.
    pub fn service_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut layer = Some(&self.services);
        while let Some(current) = layer {
            for name in current.registry.names() {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            layer = current.parent.as_ref();
        }
        names.sort();
        names
    }

    // ---- events: typed plane ----
    //
    // Every registration records this context's label path as its origin;
    // `*_scoped` dispatch uses the dispatching context's own path and only
    // reaches listeners on the same root-to-leaf chain (ancestors and
    // descendants — sibling forks never see it). Unscoped dispatch is the
    // deliberate registry-wide notification.

    pub fn on<E: Send + Sync + 'static>(&self, handler: impl Fn(&E) + Send + Sync + 'static) {
        let disposer = self.shared.events.on(self.label.clone(), handler);
        self.scope
            .push_labeled(&format!("on<{}>", std::any::type_name::<E>()), disposer);
    }

    /// Broadcast to every listener. Listener panics are contained and
    /// logged; the dispatcher is never affected.
    pub fn emit<E: Send + Sync + 'static>(&self, event: &E) {
        self.shared.events.emit(None, event);
    }

    /// Broadcast scoped to this context's fork subtree chain: ancestors and
    /// descendants of this context's path receive the event, sibling
    /// branches do not.
    pub fn emit_scoped<E: Send + Sync + 'static>(&self, event: &E) {
        self.shared.events.emit(Some(&self.label), event);
    }

    pub fn bail<E: Send + Sync + 'static, R: Send + Sync + 'static>(
        &self,
        handler: impl Fn(&E) -> Option<R> + Send + Sync + 'static,
    ) {
        let disposer = self.shared.events.bail(self.label.clone(), handler);
        self.scope
            .push_labeled(&format!("bail<{}>", std::any::type_name::<E>()), disposer);
    }

    pub fn bail_emit<E: Send + Sync + 'static, R: Send + Sync + 'static>(
        &self,
        event: &E,
    ) -> Option<R> {
        self.shared.events.bail_emit(event)
    }

    pub fn wrap<E: Send + Sync + 'static, R: Send + Sync + 'static>(
        &self,
        middleware: impl Fn(&E, Next<'_, E, R>) -> R + Send + Sync + 'static,
    ) {
        let disposer = self.shared.events.wrap(self.label.clone(), middleware);
        self.scope
            .push_labeled(&format!("wrap<{}>", std::any::type_name::<E>()), disposer);
    }

    pub fn waterfall<E: Send + Sync + 'static, R: Send + Sync + 'static>(
        &self,
        event: &E,
        terminal: impl FnOnce(&E) -> R,
    ) -> R {
        self.shared.events.waterfall(event, terminal)
    }

    /// Register a parallel responder: runs on [`Context::parallel`]
    /// dispatch, its outcome collected alongside every other responder's.
    pub fn on_parallel<E: Send + Sync + 'static, R: Send + Sync + 'static>(
        &self,
        handler: impl Fn(&E) -> R + Send + Sync + 'static,
    ) {
        let disposer = self.shared.events.on_parallel(self.label.clone(), handler);
        self.scope.push_labeled(
            &format!("on_parallel<{}>", std::any::type_name::<E>()),
            disposer,
        );
    }

    /// All-run barrier: every responder runs (panics contained and
    /// collected as `Err`), outcomes returned in registration order.
    pub fn parallel<E: Send + Sync + 'static, R: Send + Sync + 'static>(
        &self,
        event: &E,
    ) -> Vec<Result<R, String>> {
        self.shared.events.parallel(None, event)
    }

    /// [`Context::parallel`] scoped to this context's fork subtree chain.
    pub fn parallel_scoped<E: Send + Sync + 'static, R: Send + Sync + 'static>(
        &self,
        event: &E,
    ) -> Vec<Result<R, String>> {
        self.shared.events.parallel(Some(&self.label), event)
    }

    // ---- events: JSON plane ----

    pub fn on_json(
        &self,
        name: &str,
        handler: impl Fn(&serde_json::Value) + Send + Sync + 'static,
    ) {
        let disposer = self
            .shared
            .events
            .on_json(self.label.clone(), name, handler);
        self.scope
            .push_labeled(&format!("on_json({name})"), disposer);
    }

    pub fn emit_json(&self, name: &str, event: &serde_json::Value) {
        self.shared.events.emit_json(None, name, event);
    }

    /// JSON-plane broadcast scoped to this context's fork subtree chain.
    pub fn emit_json_scoped(&self, name: &str, event: &serde_json::Value) {
        self.shared.events.emit_json(Some(&self.label), name, event);
    }

    /// Live JSON-plane listener count for an event name (diagnostics /
    /// leak detection).
    pub fn json_listener_count(&self, name: &str) -> usize {
        self.shared.events.json_on_count(name)
    }

    pub fn wrap_json(
        &self,
        name: &str,
        middleware: impl Fn(serde_json::Value, JsonNext<'_>) -> serde_json::Value
            + Send
            + Sync
            + 'static,
    ) {
        let disposer = self
            .shared
            .events
            .wrap_json(self.label.clone(), name, middleware);
        self.scope
            .push_labeled(&format!("wrap_json({name})"), disposer);
    }

    /// Register a JSON parallel responder for [`Context::parallel_json`].
    pub fn on_parallel_json(
        &self,
        name: &str,
        handler: impl Fn(&serde_json::Value) -> Result<serde_json::Value, KernelError>
            + Send
            + Sync
            + 'static,
    ) {
        let disposer = self
            .shared
            .events
            .on_parallel_json(self.label.clone(), name, handler);
        self.scope
            .push_labeled(&format!("on_parallel_json({name})"), disposer);
    }

    /// JSON all-run barrier: every responder runs, each outcome collected
    /// (`Err` carries the responder's error or panic message).
    pub fn parallel_json(
        &self,
        name: &str,
        event: &serde_json::Value,
    ) -> Vec<Result<serde_json::Value, String>> {
        self.shared.events.parallel_json(None, name, event)
    }

    /// [`Context::parallel_json`] scoped to this context's fork subtree chain.
    pub fn parallel_json_scoped(
        &self,
        name: &str,
        event: &serde_json::Value,
    ) -> Vec<Result<serde_json::Value, String>> {
        self.shared
            .events
            .parallel_json(Some(&self.label), name, event)
    }

    /// Registry-wide waterfall: every middleware for `name` runs, whoever
    /// registered it. For a decision that belongs to one session, use
    /// [`Context::waterfall_json_scoped`] instead.
    pub fn waterfall_json(
        &self,
        name: &str,
        event: serde_json::Value,
        terminal: impl FnOnce(serde_json::Value) -> serde_json::Value,
    ) -> serde_json::Value {
        self.shared
            .events
            .waterfall_json(None, name, event, terminal)
    }

    /// [`Context::waterfall_json`] scoped to this context's fork subtree
    /// chain: ancestors (the deliberate host-observer position) and
    /// descendants run, sibling branches do not.
    ///
    /// This is the form a policy waterfall wants. Unscoped, a middleware
    /// registered on session A's fork also decides session B's queries —
    /// `wrap_json` records its origin precisely so that cannot happen.
    pub fn waterfall_json_scoped(
        &self,
        name: &str,
        event: serde_json::Value,
        terminal: impl FnOnce(serde_json::Value) -> serde_json::Value,
    ) -> serde_json::Value {
        self.shared
            .events
            .waterfall_json(Some(&self.label), name, event, terminal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn fork_disposal_unregisters_child_effects() {
        let root = Context::root();
        let child = root.fork("child");
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        child.on::<u32>(move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        });

        root.emit(&1u32);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        child.dispose();
        root.emit(&1u32);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "handler must be gone");
    }

    #[test]
    fn disposing_parent_disposes_children() {
        let root = Context::root();
        let parent = root.fork("parent");
        let child = parent.fork("child");
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        child.on::<u32>(move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        });

        parent.dispose();
        root.emit(&1u32);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn labels_chain() {
        let root = Context::root();
        let a = root.fork("alpha");
        let b = a.fork("beta");
        assert_eq!(b.label(), "alpha/beta");
    }

    struct EchoJson(&'static str);
    impl crate::JsonService for EchoJson {
        fn call(
            &self,
            _method: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value, crate::KernelError> {
            Ok(serde_json::json!(self.0))
        }
    }

    #[test]
    fn scoped_services_are_local_and_fall_back_to_parent() {
        let root = Context::root();
        root.provide_json("shared-svc", Arc::new(EchoJson("root")))
            .unwrap();

        let session = root.fork_scoped("session");
        session
            .provide_json("session-svc", Arc::new(EchoJson("session")))
            .unwrap();

        // Child sees both; root sees only its own.
        assert_eq!(
            session
                .call_json("session-svc", "m", serde_json::Value::Null)
                .unwrap(),
            "session"
        );
        assert_eq!(
            session
                .call_json("shared-svc", "m", serde_json::Value::Null)
                .unwrap(),
            "root"
        );
        assert!(
            root.get_json("session-svc").is_none(),
            "session service must not leak up"
        );

        // Two sibling sessions may provide the same name independently.
        let other = root.fork_scoped("other");
        other
            .provide_json("session-svc", Arc::new(EchoJson("other")))
            .unwrap();
        assert_eq!(
            other
                .call_json("session-svc", "m", serde_json::Value::Null)
                .unwrap(),
            "other"
        );
    }

    #[test]
    fn scoped_service_may_shadow_inherited_name() {
        let root = Context::root();
        root.provide_json("svc", Arc::new(EchoJson("root")))
            .unwrap();
        let session = root.fork_scoped("session");
        session
            .provide_json("svc", Arc::new(EchoJson("session")))
            .unwrap();

        assert_eq!(
            session
                .call_json("svc", "m", serde_json::Value::Null)
                .unwrap(),
            "session"
        );
        assert_eq!(
            root.call_json("svc", "m", serde_json::Value::Null).unwrap(),
            "root"
        );

        // Disposal un-shadows.
        session.dispose();
        // A fresh scoped child resolves the root provider again.
        let after = root.fork_scoped("after");
        assert_eq!(
            after
                .call_json("svc", "m", serde_json::Value::Null)
                .unwrap(),
            "root"
        );
    }

    #[test]
    fn plain_fork_shares_the_parent_namespace() {
        let root = Context::root();
        let plugin = root.fork("plugin");
        plugin
            .provide_json("svc", Arc::new(EchoJson("plugin")))
            .unwrap();
        // Non-scoped forks provide into the surrounding namespace (root here).
        assert_eq!(
            root.call_json("svc", "m", serde_json::Value::Null).unwrap(),
            "plugin"
        );
    }

    /// Contract: mount a plugin fork that registers on every plane,
    /// enumerate its registrations, dispose, and prove no registry keeps
    /// any residue.
    #[test]
    fn contract_registration_leaves_no_residue() {
        let root = Context::root();
        let baseline = root.event_stats();
        let baseline_services = root.service_names();

        let plugin = root.fork("residue-probe");
        plugin
            .provide_json("probe-svc", Arc::new(EchoJson("probe")))
            .unwrap();
        plugin.on::<u32>(|_| {});
        plugin.bail::<u32, String>(|_| None);
        plugin.wrap::<u32, String>(|e, next| next.call(e));
        plugin.on_parallel::<u32, u32>(|n| *n);
        plugin.on_json("probe/evt", |_| {});
        plugin.wrap_json("probe/evt", |v, next| next.call(v));
        plugin.on_parallel_json("probe/evt", |v| Ok(v.clone()));

        // Enumeration: every registration is visible with its label.
        let labels = plugin.registration_labels();
        for expected in [
            "provide_json(probe-svc)",
            "on<u32>",
            "bail<u32>",
            "wrap<u32>",
            "on_parallel<u32>",
            "on_json(probe/evt)",
            "wrap_json(probe/evt)",
            "on_parallel_json(probe/evt)",
        ] {
            assert!(
                labels.iter().any(|l| l == expected),
                "missing registration label {expected:?} in {labels:?}"
            );
        }
        assert_eq!(root.event_stats().total(), baseline.total() + 7);
        assert!(root.has_service("probe-svc"));

        plugin.dispose();
        assert_eq!(
            root.event_stats(),
            baseline,
            "every event-bus table must return to its pre-mount state"
        );
        assert_eq!(
            root.service_names(),
            baseline_services,
            "service registry clean"
        );
        assert!(plugin.registration_labels().is_empty(), "scope drained");
    }

    /// Contract: cascade disposal is strict LIFO with nested completion — a
    /// child fork releases all of its registrations at the fork's position
    /// in the parent's unwind.
    #[test]
    fn contract_cascade_dispose_is_lifo_with_nested_children() {
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mark = |tag: &'static str| {
            let order = order.clone();
            crate::Disposer::new(move || order.lock().unwrap().push(tag))
        };

        let root = Context::root();
        let p = root.fork("p");
        p.effect_labeled("p1", || mark("p1"));
        let c = p.fork("c");
        c.effect_labeled("c1", || mark("c1"));
        let g = c.fork("g");
        g.effect_labeled("g1", || mark("g1"));
        g.effect_labeled("g2", || mark("g2"));
        c.effect_labeled("c2", || mark("c2"));
        p.effect_labeled("p2", || mark("p2"));

        p.dispose();
        assert_eq!(
            *order.lock().unwrap(),
            vec!["p2", "c2", "g2", "g1", "c1", "p1"],
            "reverse registration order at each level; children fully release at their fork's unwind position"
        );
    }

    #[test]
    fn scoped_emit_isolates_sibling_forks() {
        let root = Context::root();
        let compose = root.fork("compose");
        let s1 = compose.fork("s1");
        let s2 = compose.fork("s2");

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        for (ctx, who) in [(&root, "root"), (&s1, "s1"), (&s2, "s2")] {
            let s = seen.clone();
            ctx.on::<u32>(move |_| s.lock().unwrap().push(who));
        }

        s1.emit_scoped(&1u32);
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["root", "s1"],
            "ancestor observers and the dispatching fork see it; the sibling never does"
        );

        seen.lock().unwrap().clear();
        root.emit(&1u32);
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["root", "s1", "s2"],
            "unscoped reaches all"
        );
    }
}
