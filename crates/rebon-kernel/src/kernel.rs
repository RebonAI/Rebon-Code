use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{Context, KernelError, Plugin, PluginMeta};

struct Loaded {
    meta: PluginMeta,
    ctx: Context,
}

/// Options for [`Kernel::unload_with`].
#[derive(Debug, Clone)]
pub struct UnloadOptions {
    /// How long the drain phase may wait for in-flight JSON-plane calls.
    pub budget: Duration,
    /// Unload even when other loaded plugins declare a required `inject`
    /// on a service this plugin provides.
    pub force: bool,
}

impl Default for UnloadOptions {
    fn default() -> Self {
        Self {
            budget: Duration::from_secs(5),
            force: false,
        }
    }
}

/// What [`Kernel::unload_with`] actually did.
#[derive(Debug, Clone)]
pub struct UnloadReport {
    pub plugin: String,
    /// In-flight calls hit zero before dispose.
    pub drained: bool,
    /// Dispose proceeded on budget exhaustion with calls still running —
    /// safe, because closed leases already guarantee stragglers only ever
    /// observe the stable `[STALE_PROVIDER]` error; we just stop waiting.
    pub forced: bool,
    pub waited: Duration,
    pub inflight_at_dispose: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum UnloadError {
    #[error("plugin `{0}` is not loaded")]
    NotLoaded(String),
    #[error(
        "plugin `{plugin}` still has dependents: {dependents:?} — unload them first, or force"
    )]
    HasDependents {
        plugin: String,
        dependents: Vec<String>,
    },
}

/// The kernel owns the root [`Context`] and the set of loaded plugins.
///
/// Loading a batch orders plugins so that every service provider applies
/// before its consumers (Kahn topological sort over declared service names),
/// fails fast on missing providers or cycles, and forks a dedicated context
/// per plugin so unloading is a single scope disposal.
pub struct Kernel {
    root: Context,
    loaded: Mutex<Vec<Loaded>>,
    /// Names reserved by an in-progress `load` batch: the duplicate check
    /// and `apply` are not under one lock, so without the reservation two
    /// concurrent loads of the same name would both pass the check.
    loading: Mutex<HashSet<String>>,
}

impl Kernel {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Arc<Kernel> {
        Arc::new(Kernel {
            root: Context::root(),
            loaded: Mutex::new(Vec::new()),
            loading: Mutex::new(HashSet::new()),
        })
    }

    /// The root context: host-side wiring (not owned by any plugin).
    pub fn context(&self) -> &Context {
        &self.root
    }

    /// Names of currently loaded plugins, in application order.
    pub fn plugin_names(&self) -> Vec<String> {
        self.loaded
            .lock()
            .unwrap()
            .iter()
            .map(|l| l.meta.name.clone())
            .collect()
    }

    /// Declared metadata of every loaded plugin, in application order —
    /// what the registry reads to show provides/dependents.
    pub fn loaded_metas(&self) -> Vec<PluginMeta> {
        self.loaded
            .lock()
            .unwrap()
            .iter()
            .map(|l| l.meta.clone())
            .collect()
    }

    /// Load a batch of plugins. Ordering constraints:
    /// - a plugin that `provides` service S applies before any plugin that
    ///   lists S in `inject` or `optional_inject` (within this batch);
    /// - `inject` against a service nobody in the batch provides fails with
    ///   [`KernelError::MissingProvider`] unless an earlier batch already
    ///   registered it;
    /// - cycles fail with [`KernelError::DependencyCycle`].
    ///
    /// On any error the whole batch is rolled back (already-applied plugins
    /// from this batch are disposed in reverse order).
    pub fn load(&self, plugins: Vec<Box<dyn Plugin>>) -> Result<(), KernelError> {
        let metas: Vec<PluginMeta> = plugins.iter().map(|p| p.meta()).collect();

        {
            let loaded = self.loaded.lock().unwrap();
            let mut loading = self.loading.lock().unwrap();
            for meta in &metas {
                if loaded.iter().any(|l| l.meta.name == meta.name) || loading.contains(&meta.name) {
                    return Err(KernelError::AlreadyLoaded(meta.name.clone()));
                }
            }
            for meta in &metas {
                loading.insert(meta.name.clone());
            }
        }
        // Reservation guard: whatever way this function exits, the names
        // leave the in-progress set (success moves them into `loaded`).
        struct Reservation<'a> {
            kernel: &'a Kernel,
            names: Vec<String>,
        }
        impl Drop for Reservation<'_> {
            fn drop(&mut self) {
                let mut loading = self.kernel.loading.lock().unwrap();
                for name in &self.names {
                    loading.remove(name);
                }
            }
        }
        let _reservation = Reservation {
            kernel: self,
            names: metas.iter().map(|m| m.name.clone()).collect(),
        };

        let order = self.resolve_order(&metas)?;

        let mut applied: Vec<Loaded> = Vec::with_capacity(plugins.len());
        let mut plugins: Vec<Option<Box<dyn Plugin>>> = plugins.into_iter().map(Some).collect();
        for index in order {
            let plugin = plugins[index].take().expect("each index applied once");
            let meta = metas[index].clone();
            // The plugin's subtree resolves only what its manifest names
            // (plus whatever it registers itself). Ambient lookup of an
            // undeclared service answers [UNAUTHORIZED_RESOLVE].
            let ctx = self.root.fork(&meta.name).confined(
                meta.provides
                    .iter()
                    .chain(meta.inject.iter())
                    .chain(meta.optional_inject.iter())
                    .cloned()
                    .collect(),
            );
            tracing::debug!(plugin = %meta.name, "kernel: applying plugin");
            if let Err(err) = plugin.apply(&ctx) {
                tracing::error!(plugin = %meta.name, %err, "kernel: plugin failed; rolling back batch");
                ctx.dispose();
                for loaded in applied.into_iter().rev() {
                    loaded.ctx.dispose();
                }
                return Err(KernelError::PluginFailed {
                    plugin: meta.name,
                    message: err.to_string(),
                });
            }
            applied.push(Loaded { meta, ctx });
        }

        let declarations: Vec<serde_json::Value> = applied
            .iter()
            .filter(|loaded| !loaded.meta.settings.is_empty())
            .map(|loaded| loaded.meta.settings_declaration())
            .collect();
        self.loaded.lock().unwrap().extend(applied);
        self.declare_settings(&declarations);
        Ok(())
    }

    /// Hand each newly loaded plugin's settings declaration to the seat.
    ///
    /// After the batch, not during it: within one batch a plugin may apply
    /// before the plugin that provides the seat, and a declaration lost to
    /// ordering is a plugin whose every write is later refused as undeclared.
    ///
    /// The kernel does this rather than the plugin because the kernel is what
    /// knows a plugin's real id, and the id is the namespace. A plugin that
    /// declared its own namespace could declare somebody else's.
    ///
    /// No seat is not an error: a kernel assembled without one has no settings
    /// to write, and every such plugin still loads.
    fn declare_settings(&self, declarations: &[serde_json::Value]) {
        if declarations.is_empty() || !self.root.has_service(crate::SETTINGS_SERVICE) {
            return;
        }
        for declaration in declarations {
            if let Err(err) =
                self.root
                    .call_json(crate::SETTINGS_SERVICE, "declare", declaration.clone())
            {
                tracing::warn!(
                    plugin = %declaration["namespace"],
                    %err,
                    "kernel: settings declaration refused"
                );
            }
        }
    }

    /// Unload one plugin by name, unwinding everything it registered.
    /// Compatibility shell over [`Kernel::unload_with`] with defaults.
    pub fn unload(&self, name: &str) -> bool {
        self.unload_with(name, UnloadOptions::default()).is_ok()
    }

    /// Unload one plugin through the lease state machine:
    ///
    /// ```text
    /// Live --close_leases--> Closing --inflight==0--> Drained --dispose--> Disposed
    ///                        Closing --budget spent--> Disposed (forced)
    /// ```
    ///
    /// - Dependency gate first (unless `force`): a loaded plugin whose
    ///   required `inject` names a service this plugin provides blocks the
    ///   unload, loudly, with the dependents listed.
    /// - `Closing`: every JSON-plane lease in the plugin's context subtree
    ///   closes — held references start refusing with the stable
    ///   `[STALE_PROVIDER]` error; running calls keep counting.
    /// - Drain is a blocking poll (this crate has no async runtime); call
    ///   from a blocking-friendly context.
    /// - Budget exhaustion still disposes: stragglers finish against
    ///   closed leases and can only observe the stable error, so waiting
    ///   longer buys nothing but latency.
    pub fn unload_with(
        &self,
        name: &str,
        options: UnloadOptions,
    ) -> Result<UnloadReport, UnloadError> {
        let entry = {
            let mut loaded = self.loaded.lock().unwrap();
            let Some(pos) = loaded.iter().position(|l| l.meta.name == name) else {
                return Err(UnloadError::NotLoaded(name.to_string()));
            };
            if !options.force {
                let provides: HashSet<&str> = loaded[pos]
                    .meta
                    .provides
                    .iter()
                    .map(String::as_str)
                    .collect();
                let dependents: Vec<String> = loaded
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != pos)
                    .filter(|(_, l)| l.meta.inject.iter().any(|s| provides.contains(s.as_str())))
                    .map(|(_, l)| l.meta.name.clone())
                    .collect();
                if !dependents.is_empty() {
                    return Err(UnloadError::HasDependents {
                        plugin: name.to_string(),
                        dependents,
                    });
                }
            }
            // Removed under the lock: from here the plugin is no longer
            // listed, and a concurrent unload of the same name observes
            // NotLoaded instead of racing the drain.
            loaded.remove(pos)
        };

        // The declaration goes with the plugin: an unloaded plugin's keys are
        // not writable by whatever loads under its name next.
        if !entry.meta.settings.is_empty() && self.root.has_service(crate::SETTINGS_SERVICE) {
            let _ = self.root.call_json(
                crate::SETTINGS_SERVICE,
                "undeclare",
                serde_json::json!({ "callerPluginId": name, "namespace": name }),
            );
        }

        entry.ctx.close_leases();
        let started = Instant::now();
        let mut inflight = entry.ctx.lease_inflight();
        while inflight > 0 && started.elapsed() < options.budget {
            std::thread::sleep(Duration::from_millis(1));
            inflight = entry.ctx.lease_inflight();
        }
        let drained = inflight == 0;
        tracing::debug!(plugin = %name, drained, inflight, "kernel: unloading plugin");
        entry.ctx.dispose();
        Ok(UnloadReport {
            plugin: name.to_string(),
            drained,
            forced: !drained,
            waited: started.elapsed(),
            inflight_at_dispose: inflight,
        })
    }

    /// Kahn topological sort over service-name edges. Returns application
    /// order as indices into `metas`.
    fn resolve_order(&self, metas: &[PluginMeta]) -> Result<Vec<usize>, KernelError> {
        let mut provider_of: HashMap<&str, usize> = HashMap::new();
        for (i, meta) in metas.iter().enumerate() {
            for service in &meta.provides {
                if let Some(&other) = provider_of.get(service.as_str()) {
                    return Err(KernelError::DuplicateProvider {
                        plugin: format!("{} vs {}", metas[other].name, meta.name),
                        service: service.clone(),
                    });
                }
                provider_of.insert(service, i);
            }
        }

        let mut edges: Vec<HashSet<usize>> = vec![HashSet::new(); metas.len()];
        let mut indegree = vec![0usize; metas.len()];
        for (i, meta) in metas.iter().enumerate() {
            for (service, required) in meta
                .inject
                .iter()
                .map(|s| (s, true))
                .chain(meta.optional_inject.iter().map(|s| (s, false)))
            {
                match provider_of.get(service.as_str()) {
                    Some(&provider) if provider != i => {
                        if edges[provider].insert(i) {
                            indegree[i] += 1;
                        }
                    }
                    Some(_) => {} // self-provided
                    None => {
                        // Not provided in this batch: fine if an earlier
                        // batch already registered it, or if optional.
                        if required && !self.root.has_service(service) {
                            return Err(KernelError::MissingProvider {
                                plugin: meta.name.clone(),
                                service: service.clone(),
                            });
                        }
                    }
                }
            }
        }

        let mut queue: VecDeque<usize> = (0..metas.len()).filter(|&i| indegree[i] == 0).collect();
        let mut order = Vec::with_capacity(metas.len());
        while let Some(i) = queue.pop_front() {
            order.push(i);
            for &next in &edges[i] {
                indegree[next] -= 1;
                if indegree[next] == 0 {
                    queue.push_back(next);
                }
            }
        }

        if order.len() != metas.len() {
            let stuck: Vec<String> = (0..metas.len())
                .filter(|i| !order.contains(i))
                .map(|i| metas[i].name.clone())
                .collect();
            return Err(KernelError::DependencyCycle(stuck));
        }
        Ok(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Service;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    trait Counter: Send + Sync {
        fn add(&self, n: usize);
        fn value(&self) -> usize;
    }

    struct CounterImpl(AtomicUsize);
    impl Counter for CounterImpl {
        fn add(&self, n: usize) {
            self.0.fetch_add(n, Ordering::SeqCst);
        }
        fn value(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct CounterService;
    impl Service for CounterService {
        type Interface = dyn Counter;
        const NAME: &'static str = "counter";
    }

    struct Provider;
    impl Plugin for Provider {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("provider").provides(&["counter"])
        }
        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<CounterService>(Arc::new(CounterImpl(AtomicUsize::new(0))))
        }
    }

    struct Consumer {
        log: Arc<StdMutex<Vec<&'static str>>>,
    }
    impl Plugin for Consumer {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("consumer").inject(&["counter"])
        }
        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            let counter = ctx.require::<CounterService>()?;
            counter.add(7);
            self.log.lock().unwrap().push("consumer");
            Ok(())
        }
    }

    #[test]
    fn providers_apply_before_consumers_regardless_of_batch_order() {
        let kernel = Kernel::new();
        let log = Arc::new(StdMutex::new(Vec::new()));
        kernel
            .load(vec![
                Box::new(Consumer { log: log.clone() }),
                Box::new(Provider),
            ])
            .unwrap();
        assert_eq!(kernel.plugin_names(), vec!["provider", "consumer"]);
        let counter = kernel.context().get::<CounterService>().unwrap();
        assert_eq!(counter.value(), 7);
    }

    #[test]
    fn missing_provider_fails_fast() {
        let kernel = Kernel::new();
        let log = Arc::new(StdMutex::new(Vec::new()));
        let err = kernel.load(vec![Box::new(Consumer { log })]).unwrap_err();
        assert!(matches!(err, KernelError::MissingProvider { .. }), "{err}");
    }

    #[test]
    fn later_batch_may_consume_earlier_batch_services() {
        let kernel = Kernel::new();
        kernel.load(vec![Box::new(Provider)]).unwrap();
        let log = Arc::new(StdMutex::new(Vec::new()));
        kernel.load(vec![Box::new(Consumer { log })]).unwrap();
    }

    struct Cyclic(&'static str, &'static str, &'static str);
    impl Plugin for Cyclic {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new(self.0)
                .provides(&[self.1])
                .inject(&[self.2])
        }
        fn apply(&self, _ctx: &Context) -> Result<(), KernelError> {
            Ok(())
        }
    }

    #[test]
    fn cycle_is_reported() {
        let kernel = Kernel::new();
        let err = kernel
            .load(vec![
                Box::new(Cyclic("a", "svc-a", "svc-b")),
                Box::new(Cyclic("b", "svc-b", "svc-a")),
            ])
            .unwrap_err();
        assert!(matches!(err, KernelError::DependencyCycle(_)), "{err}");
    }

    struct Failing;
    impl Plugin for Failing {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("failing").inject(&["counter"])
        }
        fn apply(&self, _ctx: &Context) -> Result<(), KernelError> {
            Err(KernelError::Other("boom".into()))
        }
    }

    #[test]
    fn failed_batch_rolls_back_already_applied_plugins() {
        let kernel = Kernel::new();
        let err = kernel
            .load(vec![Box::new(Provider), Box::new(Failing)])
            .unwrap_err();
        assert!(matches!(err, KernelError::PluginFailed { .. }), "{err}");
        // Provider applied first (dependency order) but must be rolled back.
        assert!(kernel.context().get::<CounterService>().is_none());
        assert!(kernel.plugin_names().is_empty());
    }

    #[test]
    fn unload_unwinds_registrations() {
        let kernel = Kernel::new();
        kernel.load(vec![Box::new(Provider)]).unwrap();
        assert!(kernel.context().get::<CounterService>().is_some());
        assert!(kernel.unload("provider"));
        assert!(kernel.context().get::<CounterService>().is_none());
        assert!(!kernel.unload("provider"), "second unload is a no-op");
    }

    /// Contract at the kernel tier: load → registries populated, unload →
    /// every table back to its pre-load state.
    #[test]
    fn contract_load_unload_leaves_no_residue() {
        struct Noisy;
        impl Plugin for Noisy {
            fn meta(&self) -> PluginMeta {
                PluginMeta::new("noisy").provides(&["noisy-svc"])
            }
            fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
                struct Echo;
                impl crate::JsonService for Echo {
                    fn call(
                        &self,
                        _m: &str,
                        _p: serde_json::Value,
                    ) -> Result<serde_json::Value, KernelError> {
                        Ok(serde_json::Value::Null)
                    }
                }
                ctx.provide_json("noisy-svc", Arc::new(Echo))?;
                ctx.on::<u32>(|_| {});
                ctx.on_json("noisy/evt", |_| {});
                ctx.on_parallel_json("noisy/evt", |v| Ok(v.clone()));
                // A nested fork with its own registration must unwind too.
                let child = ctx.fork("worker");
                child.on_json("noisy/evt", |_| {});
                Ok(())
            }
        }

        let kernel = Kernel::new();
        let baseline = kernel.context().event_stats();
        kernel.load(vec![Box::new(Noisy)]).unwrap();
        assert!(kernel.context().has_service("noisy-svc"));
        assert_eq!(kernel.context().json_listener_count("noisy/evt"), 2);
        assert!(kernel.context().event_stats().total() > baseline.total());

        assert!(kernel.unload("noisy"));
        assert_eq!(kernel.context().event_stats(), baseline, "no event residue");
        assert!(
            !kernel.context().has_service("noisy-svc"),
            "no service residue"
        );
        assert_eq!(kernel.context().json_listener_count("noisy/evt"), 0);
    }

    #[test]
    fn unload_refuses_while_dependents_hold_required_injects() {
        let kernel = Kernel::new();
        let log = Arc::new(StdMutex::new(Vec::new()));
        kernel
            .load(vec![Box::new(Provider), Box::new(Consumer { log })])
            .unwrap();

        let err = kernel
            .unload_with("provider", UnloadOptions::default())
            .unwrap_err();
        match err {
            UnloadError::HasDependents { dependents, .. } => {
                assert_eq!(dependents, vec!["consumer"]);
            }
            other => panic!("expected HasDependents, got {other}"),
        }
        assert!(
            kernel.context().get::<CounterService>().is_some(),
            "refusal changes nothing"
        );

        // Force overrides the gate; unloading the dependent first is the
        // polite path.
        let report = kernel
            .unload_with(
                "provider",
                UnloadOptions {
                    force: true,
                    ..UnloadOptions::default()
                },
            )
            .unwrap();
        assert!(report.drained);
        assert!(kernel.context().get::<CounterService>().is_none());
    }

    /// One JSON provider whose call blocks until the test says otherwise —
    /// the deterministic stand-in for an in-flight request.
    struct BlockingJson {
        entered: std::sync::mpsc::Sender<()>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
    }
    impl crate::JsonService for BlockingJson {
        fn call(&self, _m: &str, _p: serde_json::Value) -> Result<serde_json::Value, KernelError> {
            let _ = self.entered.send(());
            let _ = self.release.lock().unwrap().recv();
            Ok(serde_json::Value::Null)
        }
    }

    struct BlockingPlugin {
        service: StdMutex<Option<Arc<BlockingJson>>>,
    }
    impl Plugin for BlockingPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("blocking").provides(&["blocking-svc"])
        }
        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            let service = self.service.lock().unwrap().take().expect("applied once");
            ctx.provide_json("blocking-svc", service)
        }
    }

    fn blocking_kernel() -> (
        Arc<Kernel>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let kernel = Kernel::new();
        kernel
            .load(vec![Box::new(BlockingPlugin {
                service: StdMutex::new(Some(Arc::new(BlockingJson {
                    entered: entered_tx,
                    release: StdMutex::new(release_rx),
                }))),
            })])
            .unwrap();
        (kernel, entered_rx, release_tx)
    }

    #[test]
    fn unload_drains_an_inflight_call_then_disposes() {
        let (kernel, entered_rx, release_tx) = blocking_kernel();
        let held = kernel.context().get_json("blocking-svc").expect("resolved");

        let caller = std::thread::spawn(move || held.call("go", serde_json::Value::Null));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("call entered the provider");

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let unloader = {
            let kernel = kernel.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let report = kernel
                    .unload_with("blocking", UnloadOptions::default())
                    .unwrap();
                done.store(true, Ordering::SeqCst);
                report
            })
        };
        // The drain must actually wait for the in-flight call.
        std::thread::sleep(Duration::from_millis(50));
        assert!(!done.load(Ordering::SeqCst), "unload waits for the drain");

        release_tx.send(()).unwrap();
        let report = unloader.join().unwrap();
        assert!(report.drained && !report.forced, "{report:?}");
        assert!(
            caller.join().unwrap().is_ok(),
            "the drained call completed normally"
        );
        assert!(!kernel.context().has_service("blocking-svc"));
    }

    #[test]
    fn unload_budget_exhaustion_forces_dispose_and_stales_the_reference() {
        let (kernel, entered_rx, release_tx) = blocking_kernel();
        let held = kernel.context().get_json("blocking-svc").expect("resolved");

        let stuck = {
            let held = held.clone();
            std::thread::spawn(move || held.call("go", serde_json::Value::Null))
        };
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("call entered the provider");

        let report = kernel
            .unload_with(
                "blocking",
                UnloadOptions {
                    budget: Duration::from_millis(30),
                    ..UnloadOptions::default()
                },
            )
            .unwrap();
        assert!(report.forced && !report.drained, "{report:?}");
        assert_eq!(report.inflight_at_dispose, 1);

        // The held reference is stale now — the stable code, not a hang.
        let err = held.call("again", serde_json::Value::Null).unwrap_err();
        assert!(err.to_string().contains("[STALE_PROVIDER]"), "{err}");

        release_tx.send(()).unwrap();
        assert!(
            stuck.join().unwrap().is_ok(),
            "the straggler still finished"
        );
    }

    /// `Closing` is one-way. A registration that lands after the close —
    /// a straggling async seat, a fork made while the drain runs — must be
    /// born stale, not reopen the subtree the unload is tearing down.
    #[test]
    fn closing_is_sticky_for_late_registrations_and_late_forks() {
        struct Echo;
        impl crate::JsonService for Echo {
            fn call(
                &self,
                _m: &str,
                _p: serde_json::Value,
            ) -> Result<serde_json::Value, KernelError> {
                Ok(serde_json::Value::Null)
            }
        }

        let kernel = Kernel::new();
        let plugin = kernel.context().fork("closing-probe");
        plugin.close_leases();

        plugin
            .provide_json("late-svc", Arc::new(Echo))
            .expect("registration itself still succeeds");
        let held = plugin.get_json("late-svc").expect("resolved");
        let err = held.call("go", serde_json::Value::Null).unwrap_err();
        assert!(
            err.to_string().contains("[STALE_PROVIDER]"),
            "a lease minted after the close must be born closed: {err}"
        );

        // A fork made after the close inherits it.
        let late_fork = plugin.fork("late-fork");
        late_fork
            .provide_json("late-fork-svc", Arc::new(Echo))
            .expect("registration itself still succeeds");
        let held = late_fork.get_json("late-fork-svc").expect("resolved");
        let err = held.call("go", serde_json::Value::Null).unwrap_err();
        assert!(
            err.to_string().contains("[STALE_PROVIDER]"),
            "a fork born under a closing parent must close too: {err}"
        );
    }

    #[test]
    fn load_unload_cycles_keep_the_root_scope_bounded() {
        let kernel = Kernel::new();
        for _ in 0..50 {
            kernel.load(vec![Box::new(Provider)]).unwrap();
            assert!(kernel.unload("provider"));
        }
        let labels = kernel.context().registration_labels();
        assert!(
            labels.len() <= 2,
            "pruning keeps repeated cycles bounded, got {} entries: {labels:?}",
            labels.len()
        );
    }

    #[test]
    fn concurrent_same_name_loads_admit_exactly_one() {
        struct Slow;
        impl Plugin for Slow {
            fn meta(&self) -> PluginMeta {
                PluginMeta::new("slow")
            }
            fn apply(&self, _ctx: &Context) -> Result<(), KernelError> {
                std::thread::sleep(Duration::from_millis(50));
                Ok(())
            }
        }
        let kernel = Kernel::new();
        let (a, b) = {
            let k1 = kernel.clone();
            let k2 = kernel.clone();
            let t1 = std::thread::spawn(move || k1.load(vec![Box::new(Slow)]));
            let t2 = std::thread::spawn(move || k2.load(vec![Box::new(Slow)]));
            (t1.join().unwrap(), t2.join().unwrap())
        };
        assert!(
            a.is_ok() != b.is_ok(),
            "exactly one load wins the reservation: {a:?} vs {b:?}"
        );
        assert_eq!(kernel.plugin_names(), vec!["slow"]);
    }

    /// ACL contract: a plugin's subtree resolves exactly its manifest —
    /// undeclared names refuse with the stable code on every entry point,
    /// declared and self-registered names work, forks stay confined, and
    /// host contexts stay ambient.
    #[test]
    fn contract_undeclared_resolution_is_refused_with_the_stable_code() {
        struct Nosy {
            probe: Arc<StdMutex<Vec<(String, bool)>>>,
        }
        impl Plugin for Nosy {
            fn meta(&self) -> PluginMeta {
                // Declares nothing but its own service.
                PluginMeta::new("nosy").provides(&["nosy-svc"])
            }
            fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
                struct Echo;
                impl crate::JsonService for Echo {
                    fn call(
                        &self,
                        _m: &str,
                        _p: serde_json::Value,
                    ) -> Result<serde_json::Value, KernelError> {
                        Ok(serde_json::Value::Null)
                    }
                }
                ctx.provide_json("nosy-svc", Arc::new(Echo))?;
                // Undeclared dynamic registration: still resolvable by self.
                ctx.provide_json("nosy-extra", Arc::new(Echo))?;
                let mut probe = self.probe.lock().unwrap();

                // Undeclared foreign service: every entry point refuses.
                probe.push(("get".into(), ctx.get::<CounterService>().is_none()));
                probe.push((
                    "require".into(),
                    matches!(
                        ctx.require::<CounterService>(),
                        Err(KernelError::UnauthorizedResolve { .. })
                    ),
                ));
                probe.push(("get_json".into(), ctx.get_json("counter").is_none()));
                let call = ctx.call_json("counter", "x", serde_json::Value::Null);
                probe.push((
                    "call_json".into(),
                    call.as_ref()
                        .err()
                        .is_some_and(|e| e.to_string().contains("[UNAUTHORIZED_RESOLVE]")),
                ));
                probe.push(("has_service".into(), !ctx.has_service("counter")));

                // Own services — declared and dynamically registered.
                probe.push(("own".into(), ctx.get_json("nosy-svc").is_some()));
                probe.push(("own-extra".into(), ctx.get_json("nosy-extra").is_some()));

                // Forks stay confined.
                let worker = ctx.fork("worker");
                probe.push(("fork".into(), worker.get_json("counter").is_none()));
                probe.push(("fork-own".into(), worker.get_json("nosy-svc").is_some()));
                Ok(())
            }
        }

        let kernel = Kernel::new();
        kernel
            .load(vec![Box::new(Provider)])
            .expect("provider loads");
        let probe = Arc::new(StdMutex::new(Vec::new()));
        kernel
            .load(vec![Box::new(Nosy {
                probe: probe.clone(),
            })])
            .expect("nosy loads");
        for (what, held) in probe.lock().unwrap().iter() {
            assert!(held, "F3 contract point `{what}` failed");
        }
        // The host context stays ambient: everything resolves from root.
        assert!(kernel.context().get::<CounterService>().is_some());
        assert!(kernel.context().get_json("nosy-svc").is_some());
    }

    #[test]
    fn declared_injects_resolve_normally_under_acl() {
        let kernel = Kernel::new();
        let log = Arc::new(StdMutex::new(Vec::new()));
        // Consumer declares inject=["counter"] and resolves it in apply —
        // the whole existing suite doubles as the positive half, but pin
        // it explicitly next to the refusal contract.
        kernel
            .load(vec![
                Box::new(Provider),
                Box::new(Consumer { log: log.clone() }),
            ])
            .expect("declared inject resolves");
        assert_eq!(log.lock().unwrap().as_slice(), &["consumer"]);
    }

    #[test]
    fn optional_inject_orders_but_never_fails() {
        struct Optional;
        impl Plugin for Optional {
            fn meta(&self) -> PluginMeta {
                PluginMeta::new("optional").optional_inject(&["counter", "absent"])
            }
            fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
                assert!(
                    ctx.get::<CounterService>().is_some(),
                    "provider ordered first"
                );
                Ok(())
            }
        }
        let kernel = Kernel::new();
        kernel
            .load(vec![Box::new(Optional), Box::new(Provider)])
            .unwrap();
        assert_eq!(kernel.plugin_names(), vec!["provider", "optional"]);
    }
}
