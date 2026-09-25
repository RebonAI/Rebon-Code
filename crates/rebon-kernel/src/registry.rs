//! The plugin registry: desired state in, loaded state out.
//!
//! The kernel knows how to load and unload; the registry knows *which*
//! plugins should be loaded right now — every [`PluginDef`] the binary
//! shipped plus every [`DynPluginDef`] the machine turned out to have,
//! filtered by kind, switches and what the process withholds — and moves the
//! kernel to that set. It is
//! the one mutator: `reconcile`, `reload`, `set_enabled`,
//! [`withhold`](PluginRegistry::withhold) and
//! [`set_external_defs`](PluginRegistry::set_external_defs) all serialise on
//! the same lock, and every reconcile bumps a monotone generation that the
//! emitted [`PluginStateChanged`] events carry.
//!
//! Failure policy, because it is the part that matters:
//! - a `Feature` or `External` plugin whose factory or `apply` fails is
//!   marked [`PluginState::Failed`] with the message and **left out of the
//!   batch**, which is then loaded again without it — one broken plugin
//!   never takes its siblings down (the kernel rolls a batch back on the
//!   first failure, so the registry retries with the survivor set);
//! - a `Core` plugin is never disabled by a switch; whether its failure is
//!   fatal is the host's call, so the registry only reports it;
//! - disabling a plugin that others `inject` from unloads those dependents
//!   too, in reverse order, and says so in the report — a silent dangling
//!   `inject` is the one thing this must never produce.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};

use crate::def::{
    DynPluginDef, PluginDef, PluginHost, PluginKind, PluginState, PluginStateChanged,
};
use crate::kernel::{UnloadError, UnloadOptions};
use crate::{Kernel, KernelError};

/// Per-plugin switch overrides on top of each definition's default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesiredSet {
    overrides: BTreeMap<String, bool>,
}

impl DesiredSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set (or clear, with `None`) the switch for one plugin id.
    pub fn set(&mut self, id: &str, enabled: Option<bool>) {
        match enabled {
            Some(value) => {
                self.overrides.insert(id.to_string(), value);
            }
            None => {
                self.overrides.remove(id);
            }
        }
    }

    pub fn with(mut self, id: &str, enabled: bool) -> Self {
        self.set(id, Some(enabled));
        self
    }

    pub fn override_for(&self, id: &str) -> Option<bool> {
        self.overrides.get(id).copied()
    }

    /// Whether a definition should be loaded under these switches.
    pub fn wants(&self, def: &PluginDef) -> bool {
        self.wants_id(def.id, def.kind, def.default_enabled)
    }

    /// [`wants`](Self::wants) for a definition built at runtime.
    pub fn wants_dyn(&self, def: &DynPluginDef) -> bool {
        self.wants_id(&def.id, def.kind, def.default_enabled)
    }

    fn wants_id(&self, id: &str, kind: PluginKind, default_enabled: bool) -> bool {
        match kind {
            PluginKind::Core => true,
            PluginKind::Feature | PluginKind::External => {
                self.override_for(id).unwrap_or(default_enabled)
            }
        }
    }
}

/// One row of [`PluginRegistry::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStatus {
    pub id: String,
    pub title: String,
    pub kind: PluginKind,
    pub state: PluginState,
    /// Services it provides (empty unless loaded).
    pub provides: Vec<String>,
    /// Loaded plugins whose required `inject` names one of its services.
    pub dependents: Vec<String>,
}

/// What one reconcile / reload actually did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub generation: u64,
    pub loaded: Vec<String>,
    pub unloaded: Vec<String>,
    /// `(id, message)` for every plugin that failed this round.
    pub failed: Vec<(String, String)>,
    /// `(disabled id, dependents unloaded because of it)`.
    pub cascaded: Vec<(String, Vec<String>)>,
}

impl ReconcileReport {
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("no plugin definition with id `{0}`")]
    UnknownPlugin(String),
    #[error("plugin `{0}` is part of the kernel and cannot be disabled")]
    CoreCannotBeDisabled(String),
    #[error("plugin `{0}` is withheld from this process and cannot be enabled here")]
    Withheld(String),
}

struct Inner {
    generation: u64,
    desired: DesiredSet,
    failed: BTreeMap<String, String>,
}

/// Holds the definitions and drives the kernel to the desired set.
pub struct PluginRegistry {
    kernel: Arc<Kernel>,
    host: PluginHost,
    /// The binary's own list, first and unchanging, followed by whatever
    /// [`set_external_defs`](Self::set_external_defs) last put there. One
    /// vector rather than two because everything downstream — the desired
    /// set, the snapshot, the switches — treats a built-in and an external
    /// plugin the same way, and a second list would be a second set of
    /// rules waiting to happen.
    defs: RwLock<Vec<DynPluginDef>>,
    /// How much of `defs` is the built-in list.
    builtin_len: usize,
    inner: Mutex<Inner>,
}

impl PluginRegistry {
    /// Build a registry over `defs`. Nothing is loaded until the first
    /// [`reconcile`](Self::reconcile).
    ///
    /// Panics on two definitions with the same id — that is a build error
    /// in the binary's plugin list, not a runtime condition.
    pub fn new(kernel: Arc<Kernel>, defs: &[PluginDef], host: PluginHost) -> Arc<Self> {
        let mut seen = BTreeSet::new();
        for def in defs {
            assert!(
                seen.insert(def.id),
                "plugin definitions must have unique ids; `{}` appears twice",
                def.id
            );
        }
        let defs: Vec<DynPluginDef> = defs.iter().map(DynPluginDef::from).collect();
        Arc::new(Self {
            kernel,
            host,
            builtin_len: defs.len(),
            defs: RwLock::new(defs),
            inner: Mutex::new(Inner {
                generation: 0,
                desired: DesiredSet::new(),
                failed: BTreeMap::new(),
            }),
        })
    }

    /// Whether `def` should be loaded now: its switch, unless the process
    /// withholds it.
    fn wants(&self, inner: &Inner, def: &DynPluginDef) -> bool {
        !self.is_withheld(&def.id) && inner.desired.wants_dyn(def)
    }

    pub fn kernel(&self) -> &Arc<Kernel> {
        &self.kernel
    }

    pub fn defs(&self) -> Vec<DynPluginDef> {
        self.defs_snapshot()
    }

    pub fn def(&self, id: &str) -> Option<DynPluginDef> {
        self.defs_snapshot().into_iter().find(|d| d.id == id)
    }

    /// Replace the definitions that are not built in.
    ///
    /// The caller that knows what is installed on this machine — the Node
    /// host plugin — hands the whole set each time rather than adding and
    /// removing one at a time, because "what is installed" is a fact it
    /// re-reads whole. Ids colliding with a built-in are dropped: a package
    /// must not be able to take over a name the binary ships.
    ///
    /// Reconciles afterwards under the switches already in force, so a
    /// package that appears goes to the state its switch asks for, and one
    /// that disappears is unloaded.
    pub fn set_external_defs(&self, defs: Vec<DynPluginDef>) -> ReconcileReport {
        let builtin: BTreeSet<String> = self
            .defs_snapshot()
            .into_iter()
            .take(self.builtin_len)
            .map(|def| def.id)
            .collect();
        let mut wanted: Vec<DynPluginDef> = Vec::with_capacity(defs.len());
        let mut seen = builtin.clone();
        for def in defs {
            if seen.insert(def.id.clone()) {
                wanted.push(def);
            }
        }
        let arriving: BTreeSet<&str> = wanted.iter().map(|def| def.id.as_str()).collect();

        // Unload what is leaving *before* the table forgets it. Ownership is
        // what lets the registry unload a plugin at all, so a definition
        // dropped first would strand its plugin loaded in the kernel with
        // nothing left that knows to take it down.
        let mut departure = ReconcileReport::default();
        let leaving: Vec<String> = self
            .owned_loaded()
            .into_iter()
            .filter(|id| !builtin.contains(id) && !arriving.contains(id.as_str()))
            .collect();
        if !leaving.is_empty() {
            let mut inner = self.inner.lock().unwrap();
            inner.generation += 1;
            departure.generation = inner.generation;
            let before = self.states(&inner);
            drop(inner);
            self.unload_ids(leaving, &mut departure);
            let mut inner = self.inner.lock().unwrap();
            self.record_failures(&mut inner, &departure);
            // Still in the table at this point, so their rows report the
            // transition the way any other unload does; the truncation below
            // then removes rows that already read `Disabled`.
            let after = self.states(&inner);
            let generation = departure.generation;
            drop(inner);
            self.emit_transitions(before, after, generation);
        }

        {
            let mut table = self.defs.write().expect("plugin definitions poisoned");
            table.truncate(self.builtin_len);
            table.extend(wanted);
        }
        let desired = self.desired();
        let mut report = self.reconcile(&desired);
        report.unloaded.extend(departure.unloaded);
        report.failed.extend(departure.failed);
        report.cascaded.extend(departure.cascaded);
        report
    }

    fn defs_snapshot(&self) -> Vec<DynPluginDef> {
        self.defs
            .read()
            .expect("plugin definitions poisoned")
            .clone()
    }

    /// The switches the last reconcile ran with.
    pub fn desired(&self) -> DesiredSet {
        self.inner.lock().unwrap().desired.clone()
    }

    /// Move the kernel to the set `desired` selects. Unloads first (reverse
    /// load order, dependents cascading), then loads the rest as one batch.
    pub fn reconcile(&self, desired: &DesiredSet) -> ReconcileReport {
        let mut inner = self.inner.lock().unwrap();
        inner.desired = desired.clone();
        inner.generation += 1;
        let generation = inner.generation;
        let before = self.states(&inner);

        let mut report = ReconcileReport {
            generation,
            ..Default::default()
        };

        let defs = self.defs_snapshot();
        let loaded: BTreeSet<String> = self.owned_loaded().into_iter().collect();
        let wanted: BTreeSet<&str> = defs
            .iter()
            .filter(|def| self.wants(&inner, def))
            .map(|def| def.id.as_str())
            .collect();

        let to_unload: Vec<String> = self
            .owned_loaded()
            .into_iter()
            .filter(|id| !wanted.contains(id.as_str()))
            .collect();
        self.unload_ids(to_unload, &mut report);

        let to_load: Vec<DynPluginDef> = defs
            .iter()
            .filter(|def| wanted.contains(def.id.as_str()) && !loaded.contains(&def.id))
            .cloned()
            .collect();
        self.load_defs(to_load, &mut report);

        self.record_failures(&mut inner, &report);
        let after = self.states(&inner);
        drop(inner);
        self.emit_transitions(before, after, generation);
        report
    }

    /// Unload one plugin (and whatever depends on it), then instantiate and
    /// load them again from their definitions.
    pub fn reload(&self, id: &str) -> Result<ReconcileReport, RegistryError> {
        if self.def(id).is_none() {
            return Err(RegistryError::UnknownPlugin(id.to_string()));
        }
        let mut inner = self.inner.lock().unwrap();
        inner.generation += 1;
        let generation = inner.generation;
        let before = self.states(&inner);
        let mut report = ReconcileReport {
            generation,
            ..Default::default()
        };

        self.unload_ids(vec![id.to_string()], &mut report);
        let mut ids: Vec<String> = report.unloaded.clone();
        if !ids.iter().any(|u| u == id) {
            ids.push(id.to_string());
        }
        let to_load: Vec<DynPluginDef> = self
            .defs_snapshot()
            .into_iter()
            .filter(|def| ids.iter().any(|u| *u == def.id) && self.wants(&inner, def))
            .collect();
        self.load_defs(to_load, &mut report);

        self.record_failures(&mut inner, &report);
        let after = self.states(&inner);
        drop(inner);
        self.emit_transitions(before, after, generation);
        Ok(report)
    }

    /// Flip one switch and reconcile. Refused for `Core`, and refused for a
    /// withheld plugin when the switch would turn it on.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<ReconcileReport, RegistryError> {
        let def = self
            .def(id)
            .ok_or_else(|| RegistryError::UnknownPlugin(id.to_string()))?;
        if def.kind == PluginKind::Core && !enabled {
            return Err(RegistryError::CoreCannotBeDisabled(id.to_string()));
        }
        if enabled && self.is_withheld(id) {
            return Err(RegistryError::Withheld(id.to_string()));
        }
        let mut desired = self.desired();
        desired.set(id, Some(enabled));
        Ok(self.reconcile(&desired))
    }

    /// Keep `ids` unloaded for the rest of this registry's life, whatever
    /// the switches say, and unload any of them that is loaded now.
    ///
    /// For an entry point whose callers must not get a feature the user's
    /// settings turn on for their own sessions: the switch file is shared by
    /// every surface, so it cannot say "on, but not here". Withholding is the
    /// registry's answer because the registry is the one mutator — every
    /// later `reconcile`, `reload` and settings write asks the same "is it
    /// wanted" question, and [`set_enabled`](Self::set_enabled) refuses to
    /// turn one back on, so nothing can load it by another road.
    ///
    /// Only unloads; it never loads anything, so calling it on a registry
    /// nothing has reconciled yet leaves the first reconcile to load the rest.
    /// Refused, with nothing changed, for an unknown id and for `Core`.
    pub fn withhold(&self, ids: &[&str]) -> Result<ReconcileReport, RegistryError> {
        for id in ids {
            let def = self
                .def(id)
                .ok_or_else(|| RegistryError::UnknownPlugin(id.to_string()))?;
            if def.kind == PluginKind::Core {
                return Err(RegistryError::CoreCannotBeDisabled(id.to_string()));
            }
        }
        let mut inner = self.inner.lock().unwrap();
        self.kernel
            .context()
            .shared
            .withheld
            .write()
            .expect("withheld plugin set poisoned")
            .extend(ids.iter().map(|id| id.to_string()));
        inner.generation += 1;
        let generation = inner.generation;
        let before = self.states(&inner);
        let mut report = ReconcileReport {
            generation,
            ..Default::default()
        };
        let loaded: Vec<String> = self
            .owned_loaded()
            .into_iter()
            .filter(|id| self.is_withheld(id))
            .collect();
        self.unload_ids(loaded, &mut report);
        self.record_failures(&mut inner, &report);
        let after = self.states(&inner);
        drop(inner);
        self.emit_transitions(before, after, generation);
        Ok(report)
    }

    /// Whether [`withhold`](Self::withhold) keeps `id` out of this process.
    pub fn is_withheld(&self, id: &str) -> bool {
        self.kernel.context().is_plugin_withheld(id)
    }

    /// Every definition with its current state, in definition order.
    pub fn snapshot(&self) -> Vec<PluginStatus> {
        let inner = self.inner.lock().unwrap();
        let metas = self.kernel.loaded_metas();
        let states = self.states(&inner);
        drop(inner);
        self.defs_snapshot()
            .iter()
            .map(|def| {
                let state = states
                    .get(&def.id)
                    .cloned()
                    .unwrap_or(PluginState::Disabled);
                let provides = metas
                    .iter()
                    .find(|m| m.name == def.id)
                    .map(|m| m.provides.clone())
                    .unwrap_or_default();
                let dependents = if provides.is_empty() {
                    Vec::new()
                } else {
                    metas
                        .iter()
                        .filter(|m| m.name != def.id)
                        .filter(|m| m.inject.iter().any(|s| provides.contains(s)))
                        .map(|m| m.name.clone())
                        .collect()
                };
                PluginStatus {
                    id: def.id.to_string(),
                    title: def.title.to_string(),
                    kind: def.kind,
                    state,
                    provides,
                    dependents,
                }
            })
            .collect()
    }

    // ------------------------------------------------------------ internals

    fn owns(&self, id: &str) -> bool {
        self.defs
            .read()
            .expect("plugin definitions poisoned")
            .iter()
            .any(|d| d.id == id)
    }

    /// Registry-owned plugins currently loaded, in kernel load order.
    fn owned_loaded(&self) -> Vec<String> {
        self.kernel
            .plugin_names()
            .into_iter()
            .filter(|id| self.owns(id))
            .collect()
    }

    fn states(&self, inner: &Inner) -> BTreeMap<String, PluginState> {
        let loaded: BTreeSet<String> = self.owned_loaded().into_iter().collect();
        self.defs_snapshot()
            .iter()
            .map(|def| {
                let state = if loaded.contains(&def.id) {
                    PluginState::Loaded
                } else if let Some(message) = inner.failed.get(&def.id) {
                    PluginState::Failed(message.clone())
                } else {
                    PluginState::Disabled
                };
                (def.id.to_string(), state)
            })
            .collect()
    }

    fn record_failures(&self, inner: &mut Inner, report: &ReconcileReport) {
        for id in &report.loaded {
            inner.failed.remove(id);
        }
        for (id, message) in &report.failed {
            inner.failed.insert(id.clone(), message.clone());
        }
        // A plugin that went away without failing is simply off.
        for id in &report.unloaded {
            inner.failed.remove(id);
        }
    }

    fn emit_transitions(
        &self,
        before: BTreeMap<String, PluginState>,
        after: BTreeMap<String, PluginState>,
        generation: u64,
    ) {
        for (id, to) in after {
            let from = before.get(&id).cloned().unwrap_or(PluginState::Disabled);
            if from != to {
                tracing::info!(plugin = %id, ?from, ?to, generation, "kernel: plugin state changed");
                self.kernel.context().emit(&PluginStateChanged {
                    id,
                    from,
                    to,
                    generation,
                });
            }
        }
    }

    /// Unload `ids` (given in load order; processed in reverse), pulling in
    /// dependents as the kernel reports them. A dependent the registry does
    /// not own cannot be unloaded and is reported as a failure instead.
    fn unload_ids(&self, ids: Vec<String>, report: &mut ReconcileReport) {
        let mut stack = ids;
        // Every plugin can be pushed back at most once per dependent it
        // has; this bound only guards against a kernel that keeps naming
        // dependents we already removed.
        let mut budget = 16 * (self.defs.read().expect("plugin definitions poisoned").len() + 1);
        while let Some(id) = stack.pop() {
            if budget == 0 {
                report
                    .failed
                    .push((id, "unload gave up: dependents kept reappearing".into()));
                break;
            }
            budget -= 1;
            match self.kernel.unload_with(&id, UnloadOptions::default()) {
                Ok(_) => report.unloaded.push(id),
                Err(UnloadError::NotLoaded(_)) => {}
                Err(UnloadError::HasDependents { dependents, .. }) => {
                    let foreign: Vec<&String> =
                        dependents.iter().filter(|d| !self.owns(d)).collect();
                    if !foreign.is_empty() {
                        report.failed.push((
                            id,
                            format!(
                                "cannot unload: depended on by plugins outside the registry {foreign:?}"
                            ),
                        ));
                        continue;
                    }
                    report.cascaded.push((id.clone(), dependents.clone()));
                    stack.push(id);
                    for dependent in dependents {
                        if !stack.contains(&dependent) {
                            stack.push(dependent);
                        }
                    }
                }
            }
        }
    }

    /// Instantiate and load `defs` as one batch, dropping any that fail
    /// and retrying with the rest until the batch loads or is empty.
    fn load_defs(&self, defs: Vec<DynPluginDef>, report: &mut ReconcileReport) {
        let mut remaining = defs;
        loop {
            if remaining.is_empty() {
                return;
            }
            let mut plugins: Vec<Box<dyn crate::Plugin>> = Vec::with_capacity(remaining.len());
            let mut ids: Vec<String> = Vec::with_capacity(remaining.len());
            let mut rejected: Vec<String> = Vec::new();
            for def in &remaining {
                match (def.factory)(&self.host) {
                    Ok(plugin) => {
                        let name = plugin.meta().name;
                        if name != def.id {
                            report.failed.push((
                                def.id.clone(),
                                format!(
                                    "factory produced a plugin named `{name}`, expected `{}`",
                                    def.id
                                ),
                            ));
                            rejected.push(def.id.clone());
                            continue;
                        }
                        plugins.push(plugin);
                        ids.push(def.id.clone());
                    }
                    Err(err) => {
                        report
                            .failed
                            .push((def.id.clone(), format!("factory failed: {err}")));
                        rejected.push(def.id.clone());
                    }
                }
            }
            remaining.retain(|d| !rejected.contains(&d.id));
            if plugins.is_empty() {
                return;
            }
            match self.kernel.load(plugins) {
                Ok(()) => {
                    report.loaded.extend(ids);
                    return;
                }
                Err(err) => {
                    let culprits = culprits_of(&err, &remaining);
                    if culprits.is_empty() {
                        for def in &remaining {
                            report.failed.push((def.id.clone(), err.to_string()));
                        }
                        return;
                    }
                    for culprit in &culprits {
                        report.failed.push((culprit.clone(), err.to_string()));
                    }
                    remaining.retain(|d| !culprits.iter().any(|c| *c == d.id));
                }
            }
        }
    }
}

/// Which ids in the batch a load error is about.
fn culprits_of(err: &KernelError, batch: &[DynPluginDef]) -> Vec<String> {
    let in_batch = |name: &str| batch.iter().any(|d| d.id == name);
    match err {
        KernelError::PluginFailed { plugin, .. }
        | KernelError::MissingProvider { plugin, .. }
        | KernelError::AlreadyLoaded(plugin)
            if in_batch(plugin) =>
        {
            vec![plugin.clone()]
        }
        KernelError::DuplicateProvider { plugin, .. } => {
            // Formatted as "a vs b"; the later one in the batch loses.
            plugin
                .split(" vs ")
                .filter(|name| in_batch(name))
                .last()
                .map(|name| vec![name.to_string()])
                .unwrap_or_default()
        }
        KernelError::DependencyCycle(names) => names
            .iter()
            .filter(|name| in_batch(name))
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Context, Disposer, JsonService, Plugin, PluginMeta};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static MADE: AtomicUsize = AtomicUsize::new(0);
    /// Only the reload test's factory bumps this, so parallel tests that
    /// share `alpha` cannot skew its count.
    static RELOADED: AtomicUsize = AtomicUsize::new(0);

    struct Echo;
    impl JsonService for Echo {
        fn call(
            &self,
            _method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, KernelError> {
            Ok(params)
        }
    }

    struct Ping;

    /// Provides `svc-<name>`, listens to one event, and adds a labelled
    /// effect — three kinds of residue an unload must clean.
    struct Provider(&'static str);
    impl Plugin for Provider {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new(self.0).provides(&[&format!("svc-{}", self.0)])
        }
        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide_json(&format!("svc-{}", self.0), Arc::new(Echo))?;
            ctx.on::<Ping>(|_| {});
            ctx.effect_labeled("residue", || Disposer::noop());
            Ok(())
        }
    }

    struct Consumer(&'static str, &'static str);
    impl Plugin for Consumer {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new(self.0).inject(&[&format!("svc-{}", self.1)])
        }
        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.get_json(&format!("svc-{}", self.1))
                .ok_or_else(|| KernelError::Other("provider missing".into()))?;
            Ok(())
        }
    }

    struct Broken;
    impl Plugin for Broken {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("broken")
        }
        fn apply(&self, _ctx: &Context) -> Result<(), KernelError> {
            Err(KernelError::Other("apply exploded".into()))
        }
    }

    struct Misnamed;
    impl Plugin for Misnamed {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("not-what-the-def-says")
        }
        fn apply(&self, _ctx: &Context) -> Result<(), KernelError> {
            Ok(())
        }
    }

    fn core(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        MADE.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Provider("core")))
    }
    fn alpha(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        MADE.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Provider("alpha")))
    }
    fn beta_needs_alpha(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(Consumer("beta", "alpha")))
    }
    fn alpha_counted(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        RELOADED.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Provider("alpha")))
    }
    fn gamma_needs_beta_missing(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(Consumer("gamma", "nobody")))
    }
    fn broken(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(Broken))
    }
    fn factory_fails(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Err(KernelError::Other("no such binary".into()))
    }
    fn misnamed(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(Misnamed))
    }

    const fn def(
        id: &'static str,
        kind: PluginKind,
        default_enabled: bool,
        factory: fn(&PluginHost) -> Result<Box<dyn Plugin>, KernelError>,
    ) -> PluginDef {
        PluginDef {
            id,
            title: id,
            kind,
            default_enabled,
            factory,
        }
    }

    fn registry(defs: &[PluginDef]) -> Arc<PluginRegistry> {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        PluginRegistry::new(kernel, defs, host)
    }

    fn state_of(registry: &PluginRegistry, id: &str) -> PluginState {
        registry
            .snapshot()
            .into_iter()
            .find(|s| s.id == id)
            .map(|s| s.state)
            .expect("known id")
    }

    #[test]
    fn reconcile_loads_defaults_and_a_switch_unloads_without_residue() {
        let defs = [
            def("core", PluginKind::Core, true, core),
            def("alpha", PluginKind::Feature, true, alpha),
        ];
        let registry = registry(&defs);
        let root = registry.kernel().context().clone();
        let baseline_events = root.event_stats();

        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.loaded, vec!["core", "alpha"]);
        assert!(root.has_service("svc-alpha"));
        assert_eq!(root.event_stats().typed_on, baseline_events.typed_on + 2);

        let report = registry
            .set_enabled("alpha", false)
            .expect("feature switches off");
        assert_eq!(report.unloaded, vec!["alpha"]);
        assert!(!root.has_service("svc-alpha"), "I1: service gone");
        assert!(root.has_service("svc-core"), "core untouched");
        assert_eq!(
            root.event_stats().typed_on,
            baseline_events.typed_on + 1,
            "I1: the listener went with the plugin"
        );
        assert_eq!(registry.kernel().plugin_names(), vec!["core"]);
        assert_eq!(state_of(&registry, "alpha"), PluginState::Disabled);

        let report = registry
            .set_enabled("alpha", true)
            .expect("feature switches back on");
        assert_eq!(report.loaded, vec!["alpha"]);
        assert!(root.has_service("svc-alpha"), "I4: back after the switch");
    }

    #[test]
    fn core_cannot_be_disabled() {
        let defs = [def("core", PluginKind::Core, true, core)];
        let registry = registry(&defs);
        registry.reconcile(&DesiredSet::new());
        let err = registry.set_enabled("core", false).unwrap_err();
        assert!(matches!(err, RegistryError::CoreCannotBeDisabled(ref id) if id == "core"));
        // A switch file that says otherwise is ignored too.
        registry.reconcile(&DesiredSet::new().with("core", false));
        assert_eq!(registry.kernel().plugin_names(), vec!["core"]);
    }

    /// Withheld before the first reconcile — how an entry point declares it —
    /// the plugin never loads, however its switch reads, and its neighbours
    /// load exactly as they would have.
    #[test]
    fn a_plugin_withheld_before_boot_never_loads_whatever_the_switches_say() {
        let defs = [
            def("core", PluginKind::Core, true, core),
            def("alpha", PluginKind::Feature, false, alpha),
            def("beta", PluginKind::Feature, true, beta_needs_alpha),
        ];
        let registry = registry(&defs);
        let report = registry.withhold(&["alpha"]).expect("a feature withholds");
        assert!(report.unloaded.is_empty(), "nothing was loaded: {report:?}");
        assert!(
            registry.kernel().plugin_names().is_empty(),
            "withholding never loads anything"
        );
        assert!(registry.is_withheld("alpha"));
        assert!(!registry.is_withheld("beta"));
        // Anyone holding a context of this kernel can ask, down any fork.
        let session = registry.kernel().context().fork_scoped("session");
        assert!(session.fork("turn").is_plugin_withheld("alpha"));
        assert!(!session.is_plugin_withheld("beta"));

        registry.reconcile(&DesiredSet::new().with("alpha", true));
        assert!(!registry.kernel().context().has_service("svc-alpha"));
        assert_eq!(state_of(&registry, "alpha"), PluginState::Disabled);
        assert_eq!(state_of(&registry, "core"), PluginState::Loaded);
        // `beta` needs what `alpha` would have provided, so it fails the way
        // it would with `alpha` switched off — not silently loaded.
        assert!(matches!(
            state_of(&registry, "beta"),
            PluginState::Failed(_)
        ));

        let err = registry.set_enabled("alpha", true).unwrap_err();
        assert!(matches!(err, RegistryError::Withheld(ref id) if id == "alpha"));
        assert!(!registry.kernel().context().has_service("svc-alpha"));
        // Switching it *off* is still allowed; it only records the switch.
        registry
            .set_enabled("alpha", false)
            .expect("off is harmless");

        registry.reload("alpha").expect("known id");
        assert!(
            !registry.kernel().context().has_service("svc-alpha"),
            "a reload does not bring a withheld plugin back"
        );
    }

    /// Withheld after boot, the plugin is taken out now, together with what
    /// depended on it, and stays out through later reconciles.
    #[test]
    fn withholding_a_loaded_plugin_unloads_it_and_its_dependents() {
        let defs = [
            def("core", PluginKind::Core, true, core),
            def("alpha", PluginKind::Feature, true, alpha),
            def("beta", PluginKind::Feature, true, beta_needs_alpha),
        ];
        let registry = registry(&defs);
        registry.reconcile(&DesiredSet::new());
        assert_eq!(
            registry.kernel().plugin_names(),
            vec!["core", "alpha", "beta"]
        );

        let report = registry.withhold(&["alpha"]).expect("a feature withholds");
        let mut unloaded = report.unloaded.clone();
        unloaded.sort();
        assert_eq!(unloaded, vec!["alpha", "beta"], "{report:?}");
        assert_eq!(registry.kernel().plugin_names(), vec!["core"]);

        registry.reconcile(&registry.desired());
        assert!(!registry.kernel().context().has_service("svc-alpha"));
        assert!(registry.kernel().context().has_service("svc-core"));
    }

    #[test]
    fn withholding_refuses_core_and_unknown_ids_without_changing_anything() {
        let defs = [
            def("core", PluginKind::Core, true, core),
            def("alpha", PluginKind::Feature, true, alpha),
        ];
        let registry = registry(&defs);
        registry.reconcile(&DesiredSet::new());

        let err = registry.withhold(&["alpha", "core"]).unwrap_err();
        assert!(matches!(err, RegistryError::CoreCannotBeDisabled(ref id) if id == "core"));
        let err = registry.withhold(&["alpha", "nope"]).unwrap_err();
        assert!(matches!(err, RegistryError::UnknownPlugin(ref id) if id == "nope"));
        assert!(
            !registry.is_withheld("alpha"),
            "a refused call withholds nothing, not even the valid ids before it"
        );
        assert_eq!(registry.kernel().plugin_names(), vec!["core", "alpha"]);
    }

    #[test]
    fn a_failing_feature_is_isolated_and_reported() {
        let defs = [
            def("core", PluginKind::Core, true, core),
            def("broken", PluginKind::Feature, true, broken),
            def("no-binary", PluginKind::External, true, factory_fails),
            def("misnamed", PluginKind::Feature, true, misnamed),
            def("gamma", PluginKind::Feature, true, gamma_needs_beta_missing),
            def("alpha", PluginKind::Feature, true, alpha),
        ];
        let registry = registry(&defs);
        let report = registry.reconcile(&DesiredSet::new());

        let mut loaded = registry.kernel().plugin_names();
        loaded.sort();
        assert_eq!(loaded, vec!["alpha", "core"], "I5: survivors load");
        let failed: BTreeSet<&str> = report.failed.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            failed,
            ["broken", "no-binary", "misnamed", "gamma"]
                .into_iter()
                .collect()
        );
        assert!(
            matches!(state_of(&registry, "broken"), PluginState::Failed(ref m) if m.contains("apply exploded"))
        );
        assert!(
            matches!(state_of(&registry, "no-binary"), PluginState::Failed(ref m) if m.contains("no such binary"))
        );
        assert!(
            matches!(state_of(&registry, "gamma"), PluginState::Failed(ref m) if m.contains("svc-nobody"))
        );
        assert_eq!(state_of(&registry, "alpha"), PluginState::Loaded);
    }

    #[test]
    fn disabling_a_provider_cascades_to_its_dependents() {
        let defs = [
            def("alpha", PluginKind::Feature, true, alpha),
            def("beta", PluginKind::Feature, true, beta_needs_alpha),
        ];
        let registry = registry(&defs);
        let report = registry.reconcile(&DesiredSet::new());
        assert_eq!(report.loaded, vec!["alpha", "beta"]);
        assert_eq!(
            registry
                .snapshot()
                .into_iter()
                .find(|s| s.id == "alpha")
                .unwrap()
                .dependents,
            vec!["beta"]
        );

        let report = registry.set_enabled("alpha", false).unwrap();
        assert_eq!(
            report.unloaded,
            vec!["beta", "alpha"],
            "I2: dependents first"
        );
        assert_eq!(
            report.cascaded,
            vec![("alpha".to_string(), vec!["beta".to_string()])]
        );
        assert!(registry.kernel().plugin_names().is_empty());
    }

    #[test]
    fn reload_makes_a_fresh_instance_and_brings_dependents_back() {
        let defs = [
            def("alpha", PluginKind::Feature, true, alpha_counted),
            def("beta", PluginKind::Feature, true, beta_needs_alpha),
        ];
        let registry = registry(&defs);
        registry.reconcile(&DesiredSet::new());
        assert_eq!(RELOADED.load(Ordering::SeqCst), 1);

        let report = registry.reload("alpha").expect("known id");
        assert_eq!(report.unloaded, vec!["beta", "alpha"]);
        assert_eq!(report.loaded, vec!["alpha", "beta"]);
        assert_eq!(RELOADED.load(Ordering::SeqCst), 2, "factory ran again");
        assert!(registry.kernel().context().has_service("svc-alpha"));
        assert!(matches!(
            registry.reload("nope"),
            Err(RegistryError::UnknownPlugin(_))
        ));
    }

    #[test]
    fn state_changes_are_announced_on_the_root_with_a_monotone_generation() {
        let defs = [def("alpha", PluginKind::Feature, false, alpha)];
        let registry = registry(&defs);
        let seen: Arc<Mutex<Vec<PluginStateChanged>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        registry
            .kernel()
            .context()
            .on::<PluginStateChanged>(move |event| sink.lock().unwrap().push(event.clone()));

        let g1 = registry.reconcile(&DesiredSet::new()).generation;
        assert!(
            seen.lock().unwrap().is_empty(),
            "off by default: nothing moved"
        );
        let g2 = registry.set_enabled("alpha", true).unwrap().generation;
        let g3 = registry.set_enabled("alpha", false).unwrap().generation;
        assert!(g1 < g2 && g2 < g3);

        let events = seen.lock().unwrap().clone();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].from, PluginState::Disabled);
        assert_eq!(events[0].to, PluginState::Loaded);
        assert_eq!(events[0].generation, g2);
        assert_eq!(events[1].to, PluginState::Disabled);
        assert_eq!(events[1].generation, g3);
    }

    #[test]
    fn duplicate_definition_ids_are_a_build_error() {
        let defs = [
            def("alpha", PluginKind::Feature, true, alpha),
            def("alpha", PluginKind::Feature, true, alpha),
        ];
        let result = std::panic::catch_unwind(|| registry(&defs));
        assert!(result.is_err());
    }

    /// A plugin whose name is only known at runtime — what an external
    /// definition produces.
    struct Named(String);
    impl Plugin for Named {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new(self.0.clone())
        }
        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.effect_labeled("residue", Disposer::noop);
            Ok(())
        }
    }

    fn external(id: &str, default_enabled: bool) -> DynPluginDef {
        let name = id.to_string();
        DynPluginDef::new(id, id, PluginKind::External, default_enabled, move |_| {
            Ok(Box::new(Named(name.clone())) as Box<dyn Plugin>)
        })
    }

    /// An external definition is a row like any other — same table, same
    /// switch, same unload.
    #[test]
    fn external_definitions_join_the_table_and_answer_the_same_switches() {
        let defs = [def("alpha", PluginKind::Feature, true, alpha)];
        let registry = registry(&defs);
        registry.reconcile(&DesiredSet::new());

        let report = registry.set_external_defs(vec![
            external("node:demo", true),
            external("node:off", false),
        ]);
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(state_of(&registry, "node:demo"), PluginState::Loaded);
        assert_eq!(state_of(&registry, "node:off"), PluginState::Disabled);
        let row = registry
            .snapshot()
            .into_iter()
            .find(|s| s.id == "node:demo")
            .expect("the external row is in the one table");
        assert_eq!(row.kind, PluginKind::External);

        // The same switch a built-in answers.
        registry
            .set_enabled("node:demo", false)
            .expect("switchable");
        assert_eq!(state_of(&registry, "node:demo"), PluginState::Disabled);
        registry.set_enabled("node:demo", true).expect("switchable");
        assert_eq!(state_of(&registry, "node:demo"), PluginState::Loaded);

        // A package that goes away takes its plugin with it, and leaves the
        // built-ins alone.
        let report = registry.set_external_defs(Vec::new());
        assert!(
            report.unloaded.contains(&"node:demo".to_string()),
            "{report:?}"
        );
        assert!(registry.snapshot().iter().all(|s| s.id != "node:demo"));
        assert_eq!(state_of(&registry, "alpha"), PluginState::Loaded);
    }

    /// A package must not be able to take over a name the binary ships.
    #[test]
    fn an_external_definition_cannot_shadow_a_builtin_id() {
        let defs = [def("alpha", PluginKind::Feature, true, alpha)];
        let registry = registry(&defs);
        registry.reconcile(&DesiredSet::new());
        registry.set_external_defs(vec![external("alpha", true)]);
        let rows: Vec<String> = registry
            .snapshot()
            .into_iter()
            .filter(|s| s.id == "alpha")
            .map(|s| format!("{:?}", s.kind))
            .collect();
        assert_eq!(
            rows,
            vec!["Feature".to_string()],
            "the built-in kept its id"
        );
    }
}
