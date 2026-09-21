//! The agents a host can point a session at.
//!
//! One backend per configured agent, built on first use and
//! then reused. That reuse is the whole point: a backend owns the child
//! process and its connection, so building a second one for the same
//! agent would start a second CLI, with a second model session and a
//! cold context — the ACP leg's version of a cache miss.
//!
//! The registry deliberately knows nothing about `config.json`. The
//! host reads the config, decides where each agent's writes and history
//! land, and hands over a lazy backend factory. That keeps
//! the mapping from user-facing config to spawn arguments in one place
//! (the host) instead of splitting it across two crates.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One configured agent, ready to be built on demand.
pub struct BackendEntry<B> {
    /// Id used by `/agent`, session metadata, and logs.
    pub id: String,
    /// Name shown to the user.
    pub label: String,
    /// How to start it, and where its writes and history go.
    factory: Box<dyn Fn() -> B + Send + Sync>,
}

impl<B> BackendEntry<B> {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        factory: impl Fn() -> B + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            factory: Box::new(factory),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum RegistryError {
    #[error("unknown agent `{id}` (configured: {})", format_available(.available))]
    UnknownAgent { id: String, available: Vec<String> },
}

fn format_available(available: &[String]) -> String {
    if available.is_empty() {
        "none — add one under `acpAgents` in config.json".to_string()
    } else {
        available.join(", ")
    }
}

struct Entry<B> {
    id: String,
    label: String,
    /// Kept, not consumed. Building a backend used to take it, which made a
    /// reconnect impossible: there was nothing left to build the replacement
    /// from. A panic between the take and store also used to wedge an entry;
    /// retaining the factory makes retry possible even after a poisoned lock.
    factory: Box<dyn Fn() -> B + Send + Sync>,
    backend: Option<Arc<B>>,
}

/// The configured agents, and the one live backend each.
pub struct BackendRegistry<B> {
    /// Canonical ids in configuration order — what the UI lists.
    order: Vec<String>,
    /// Keyed by folded id so `/agent Claude-Code` finds `claude-code`.
    entries: Mutex<HashMap<String, Entry<B>>>,
}

impl<B> BackendRegistry<B> {
    /// Build a registry. Later entries with an id already present are
    /// dropped; the config layer rejects duplicates before this, so
    /// reaching here means a caller built the list itself.
    pub fn new(entries: impl IntoIterator<Item = BackendEntry<B>>) -> Self {
        let mut order = Vec::new();
        let mut map = HashMap::new();
        for entry in entries {
            let key = fold(&entry.id);
            if map.contains_key(&key) {
                tracing::warn!(agent = %entry.id, "agent-core: ignoring a duplicate agent id");
                continue;
            }
            order.push(entry.id.clone());
            map.insert(
                key,
                Entry {
                    id: entry.id,
                    label: entry.label,
                    factory: entry.factory,
                    backend: None,
                },
            );
        }
        Self {
            order,
            entries: Mutex::new(map),
        }
    }

    /// An empty registry — no agents configured.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Configured ids, in configuration order.
    pub fn ids(&self) -> Vec<String> {
        self.order.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.lock().contains_key(&fold(id))
    }

    /// The canonical id and label for `id`, however it was spelled.
    pub fn resolve(&self, id: &str) -> Option<(String, String)> {
        self.lock()
            .get(&fold(id))
            .map(|entry| (entry.id.clone(), entry.label.clone()))
    }

    /// The backend for `id`, building it on first use.
    ///
    /// Building does not start the agent — connecting is lazy, so an
    /// agent nobody prompts never runs. Factories must only construct the
    /// backend; they must not connect or re-enter this registry under its lock.
    pub fn backend(&self, id: &str) -> Result<Arc<B>, RegistryError> {
        let mut entries = self.lock();
        let entry = entries
            .get_mut(&fold(id))
            .ok_or_else(|| RegistryError::UnknownAgent {
                id: id.to_string(),
                available: self.order.clone(),
            })?;
        if let Some(backend) = &entry.backend {
            return Ok(backend.clone());
        }
        let backend = Arc::new((entry.factory)());
        entry.backend = Some(backend.clone());
        Ok(backend)
    }

    /// Drops the built backend so the next use dials the agent again.
    ///
    /// Returns it, because stopping the old process is the caller's to do and
    /// it has to be done *after* the registry has let go — a shutdown awaited
    /// under the registry lock would hold every other agent's lookups behind a
    /// process that is on its way out.
    ///
    /// An agent that was never built reconnects to nothing and says so with
    /// `None`; the next prompt builds it for the first time either way.
    pub fn reconnect(&self, id: &str) -> Result<Option<Arc<B>>, RegistryError> {
        let mut entries = self.lock();
        let entry = entries
            .get_mut(&fold(id))
            .ok_or_else(|| RegistryError::UnknownAgent {
                id: id.to_string(),
                available: self.order.clone(),
            })?;
        Ok(entry.backend.take())
    }

    /// Backends that have actually been built, for shutdown.
    pub fn live_backends(&self) -> Vec<Arc<B>> {
        self.lock()
            .values()
            .filter_map(|entry| entry.backend.clone())
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry<B>>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<B> std::fmt::Debug for BackendRegistry<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendRegistry")
            .field("agents", &self.order)
            .finish()
    }
}

fn fold(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LocalAgentBackend, StubPromptExecutor};

    fn entry(id: &str) -> BackendEntry<LocalAgentBackend> {
        BackendEntry::new(id, format!("{id} label"), || {
            LocalAgentBackend::new(Arc::new(StubPromptExecutor))
        })
    }

    /// The point of keeping the configuration: a second build is possible, and
    /// it is a *different* backend — the old process is not reused.
    #[test]
    fn reconnecting_hands_back_the_old_backend_and_builds_a_new_one() {
        let registry = BackendRegistry::new([entry("claude")]);
        let first = registry.backend("claude").expect("builds");
        assert!(
            Arc::ptr_eq(&first, &registry.backend("claude").expect("cached")),
            "a second lookup reuses the built backend"
        );

        let dropped = registry.reconnect("claude").expect("known agent");
        let dropped = dropped.expect("it had been built");
        assert!(
            Arc::ptr_eq(&first, &dropped),
            "the caller gets the old one to stop"
        );

        let second = registry.backend("claude").expect("builds again");
        assert!(
            !Arc::ptr_eq(&first, &second),
            "reconnecting means a new backend, not the old one handed back"
        );
    }

    /// Reconnecting something nobody ever prompted is not an error: the next
    /// prompt builds it for the first time, which is what was wanted.
    #[test]
    fn reconnecting_an_unbuilt_agent_is_a_no_op() {
        let registry = BackendRegistry::new([entry("claude")]);
        assert!(registry.reconnect("claude").expect("known agent").is_none());
        assert!(registry.backend("claude").is_ok());
    }

    #[test]
    fn reconnecting_an_agent_that_is_not_configured_says_which_are() {
        let registry = BackendRegistry::new([entry("claude")]);
        let error = registry.reconnect("ghost").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("ghost"), "{text}");
        assert!(text.contains("claude"), "{text}");
    }

    /// Ids fold, so a reconnect typed with different casing still finds it.
    #[test]
    fn reconnect_folds_the_id_like_every_other_lookup() {
        let registry = BackendRegistry::new([entry("claude-code")]);
        registry.backend("claude-code").expect("builds");
        assert!(registry
            .reconnect("Claude-Code")
            .expect("known agent")
            .is_some());
    }

    #[test]
    fn ids_keep_configuration_order() {
        let registry = BackendRegistry::new([entry("zeta"), entry("alpha")]);
        assert_eq!(
            registry.ids(),
            vec!["zeta".to_string(), "alpha".to_string()]
        );
        assert!(!registry.is_empty());
        assert!(BackendRegistry::<LocalAgentBackend>::empty().is_empty());
    }

    #[test]
    fn lookup_is_case_insensitive_and_reports_the_canonical_spelling() {
        let registry = BackendRegistry::new([entry("claude-code")]);
        assert!(registry.contains("Claude-Code"));
        assert_eq!(
            registry.resolve("  CLAUDE-CODE  "),
            Some(("claude-code".to_string(), "claude-code label".to_string()))
        );
        assert!(registry.resolve("gemini").is_none());
    }

    #[test]
    fn the_same_agent_is_only_ever_built_once() {
        // A second backend would be a second child process with a cold
        // session — the thing this registry exists to prevent.
        let registry = BackendRegistry::new([entry("claude-code")]);
        let first = registry.backend("claude-code").expect("configured");
        let second = registry.backend("Claude-Code").expect("same agent");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(registry.live_backends().len(), 1);
    }

    #[test]
    fn an_unknown_agent_names_what_is_configured() {
        let registry = BackendRegistry::new([entry("claude-code")]);
        let err = registry.backend("ghost").expect_err("not configured");
        let message = err.to_string();
        assert!(message.contains("ghost"), "{message}");
        assert!(message.contains("claude-code"), "{message}");
    }

    #[test]
    fn an_empty_registry_says_where_agents_come_from() {
        let err = BackendRegistry::<LocalAgentBackend>::empty()
            .backend("claude-code")
            .expect_err("nothing configured");
        assert!(err.to_string().contains("acpAgents"), "{err}");
    }

    #[test]
    fn duplicate_ids_do_not_shadow_the_first_entry() {
        let registry = BackendRegistry::new([entry("dup"), entry("DUP")]);
        assert_eq!(registry.ids(), vec!["dup".to_string()]);
    }

    #[test]
    fn nothing_is_built_until_something_asks() {
        let registry = BackendRegistry::new([entry("claude-code")]);
        assert!(
            registry.live_backends().is_empty(),
            "an agent nobody prompted must not have a backend"
        );
    }

    #[test]
    fn a_panicking_factory_can_retry_without_losing_its_configuration() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let registry = BackendRegistry::new([BackendEntry::new("retry", "Retry", move || {
            assert_ne!(count.fetch_add(1, Ordering::SeqCst), 0, "first build fails");
            42usize
        })]);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || registry.backend("retry")
        ))
        .is_err());
        assert!(registry.live_backends().is_empty());
        assert_eq!(*registry.backend(" RETRY ").unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn simultaneous_first_lookups_share_one_backend() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let builds = Arc::new(AtomicUsize::new(0));
        let count = builds.clone();
        let registry = Arc::new(BackendRegistry::new([BackendEntry::new(
            "one",
            "One",
            move || count.fetch_add(1, Ordering::SeqCst),
        )]));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let registry = registry.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    registry.backend("ONE").unwrap()
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(results.iter().all(|b| Arc::ptr_eq(b, &results[0])));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn folding_preserves_non_ascii_and_deduplicates_whitespace() {
        let registry = BackendRegistry::new([entry(" a "), entry("A"), entry("É"), entry("é")]);
        assert_eq!(registry.ids(), vec![" a ", "É", "é"]);
        assert_eq!(registry.resolve("A").unwrap().0, " a ");
        assert!(!Arc::ptr_eq(
            &registry.backend("É").unwrap(),
            &registry.backend("é").unwrap()
        ));
        assert!(registry.backend("").is_err());
    }
}
