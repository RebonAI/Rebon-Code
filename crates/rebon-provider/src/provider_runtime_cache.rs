//! Per-provider runtime cache for cross-provider sub-agent routing.
//!
//! Resolving a provider runtime is expensive and almost entirely repeatable
//! work: it re-reads `config.json` and `.credentials.json`, runs a preemptive
//! OAuth refresh, rebuilds the provider registry (re-scanning plugin
//! directories on the CLI path), and rebuilds the whole
//! `ContextPrune → Retry → Logging` middleware stack. For an **external**
//! plugin provider it also spawns a fresh child process and re-runs the
//! `initialize` handshake.
//!
//! None of that is per-call state, so doing it on every
//! [`ProviderRuntimeResolver::resolve_provider`] call meant every sub-agent
//! spawn that named a provider got its own child process — and therefore a
//! guaranteed prompt-cache miss on the other side.
//!
//! # What keeps the cache honest
//!
//! - **Stat gate.** `config.json`'s (mtime, len) is sampled on every lookup;
//!   if it moved, the whole cache is dropped so an edited provider entry takes
//!   effect immediately. A stat is orders of magnitude cheaper than the
//!   rebuild it guards. `.credentials.json` is deliberately *not* gated on:
//!   clients refresh their own OAuth tokens on 401 and write that file, so
//!   watching it would make the cache invalidate itself.
//! - **Idle TTL.** Entries unused for [`DEFAULT_PROVIDER_IDLE_TTL`] are
//!   dropped on the next lookup. Dropping the last reference is what releases
//!   an external provider's plane scope (`PlaneModelProviderClient` closes it
//!   on `Drop`), so a runtime still held by a running agent survives eviction
//!   — reference counting decides the actual release, not the cache.
//! - **Single flight.** Concurrent lookups for the same provider await one
//!   initialization instead of racing to build duplicate runtimes.
//!   Different providers still resolve in parallel.
//! - **Cacheability is decided per runtime.** Not every runtime tolerates
//!   sharing: an external plugin client has no `fork_for_sub_agent`, and its
//!   conversation signals (`endTurn`/`reset`/`invalidate`) travel on the
//!   scope that client opened, so concurrent agents sharing one client would
//!   clear each other's turn state. An OAuth provider without a
//!   401-refresh middleware could never rotate an expired token once cached.
//!   The `cacheable` predicate runs on the freshly built runtime; when it
//!   rejects one, the builder still gets its runtime but the slot remembers
//!   the verdict and every later lookup builds fresh — the pre-cache
//!   behavior.
//!
//! Callers cache the *provider* runtime only. Anything that must track live
//! session state — notably the fast/service-tier toggle — stays outside and is
//! re-applied on every hit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{Mutex, OnceCell};

/// How long an unused provider runtime is kept before it is dropped.
pub const DEFAULT_PROVIDER_IDLE_TTL: Duration = Duration::from_secs(300);

/// Cheap identity of `config.json`, used to detect edits without parsing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ConfigStamp {
    modified: Option<SystemTime>,
    len: u64,
}

fn stat_config(path: &Path) -> Option<ConfigStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(ConfigStamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}

/// Config lookup is `eq_ignore_ascii_case` and `model: "Kimi/k2"` prefix
/// routing preserves the author's casing, so distinct spellings of one
/// provider must land on one slot — two slots would mean two middleware
/// stacks and, for a plugin provider, two child processes.
fn slot_key(provider: &str) -> String {
    provider.trim().to_ascii_lowercase()
}

enum CacheDecision<T> {
    Cached(Arc<T>),
    /// The runtime must not be shared; every lookup builds its own.
    Bypass,
}

struct Slot<T> {
    cell: Arc<OnceCell<CacheDecision<T>>>,
    last_used: Instant,
}

struct State<T> {
    /// `None` until the first lookup samples the file, so a missing
    /// `config.json` (env-var fallback path) does not read as "just changed".
    stamp: Option<Option<ConfigStamp>>,
    slots: HashMap<String, Slot<T>>,
}

/// Caches resolved provider runtimes keyed by provider id.
pub struct ProviderRuntimeCache<T> {
    config_path: PathBuf,
    idle_ttl: Duration,
    state: Mutex<State<T>>,
}

impl<T> ProviderRuntimeCache<T> {
    /// Build a cache gated on the `config.json` inside `config_dir`.
    pub fn new(config_dir: &Path) -> Self {
        Self::with_idle_ttl(config_dir, DEFAULT_PROVIDER_IDLE_TTL)
    }

    pub fn with_idle_ttl(config_dir: &Path, idle_ttl: Duration) -> Self {
        Self {
            config_path: rebon_config::config_json_path(config_dir),
            idle_ttl,
            state: Mutex::new(State {
                stamp: None,
                slots: HashMap::new(),
            }),
        }
    }

    /// Return the cached runtime for `provider`, or build it with `init`.
    ///
    /// `cacheable` runs once on a freshly built runtime. If it accepts,
    /// `init` runs at most once per provider per cache generation and
    /// concurrent callers for the same provider await that single run. If it
    /// rejects, the builder keeps its runtime but nothing is stored — that
    /// lookup and every later one build fresh. A failed `init` is not cached
    /// — the next call retries.
    pub async fn get_or_try_init<F, Fut>(
        &self,
        provider: &str,
        init: F,
        cacheable: impl FnOnce(&T) -> bool,
    ) -> anyhow::Result<Arc<T>>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<T>>,
    {
        let cell = {
            let mut state = self.state.lock().await;
            let stamp = stat_config(&self.config_path);
            if state.stamp.is_some_and(|previous| previous != stamp) {
                tracing::debug!(
                    provider = %provider,
                    "provider runtime cache: config.json changed, dropping cached runtimes"
                );
                state.slots.clear();
            }
            state.stamp = Some(stamp);

            let now = Instant::now();
            let ttl = self.idle_ttl;
            state
                .slots
                .retain(|_, slot| now.duration_since(slot.last_used) < ttl);

            let slot = state
                .slots
                .entry(slot_key(provider))
                .or_insert_with(|| Slot {
                    cell: Arc::new(OnceCell::new()),
                    last_used: now,
                });
            slot.last_used = now;
            slot.cell.clone()
        };

        if let Some(decision) = cell.get() {
            return match decision {
                CacheDecision::Cached(runtime) => Ok(runtime.clone()),
                CacheDecision::Bypass => Ok(Arc::new(init().await?)),
            };
        }
        let mut fresh = None;
        let decision = cell
            .get_or_try_init(|| async {
                let runtime = Arc::new(init().await?);
                if cacheable(&runtime) {
                    anyhow::Ok(CacheDecision::Cached(runtime))
                } else {
                    fresh = Some(runtime);
                    anyhow::Ok(CacheDecision::Bypass)
                }
            })
            .await?;
        match decision {
            CacheDecision::Cached(runtime) => Ok(runtime.clone()),
            CacheDecision::Bypass => match fresh {
                Some(runtime) => Ok(runtime),
                // We waited on another caller whose build turned out
                // uncacheable; sharing theirs is exactly what Bypass forbids.
                None => Ok(Arc::new(init().await?)),
            },
        }
    }

    /// Drop every cached runtime. Entries still referenced by a running agent
    /// stay alive until that agent releases them.
    pub async fn clear(&self) {
        let _ = self.drop_all().await;
    }

    /// The same, reporting how many were dropped.
    ///
    /// A caller that asked for this out loud — `/provider reconnect` — should
    /// be told what happened, and "0" is a real answer: nothing had been dialed
    /// yet, so the next turn starts fresh either way.
    pub async fn drop_all(&self) -> usize {
        let mut state = self.state.lock().await;
        let dropped = state.slots.len();
        state.slots.clear();
        dropped
    }

    #[cfg(test)]
    async fn len(&self) -> usize {
        self.state.lock().await.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn cache_in(dir: &Path) -> ProviderRuntimeCache<u32> {
        ProviderRuntimeCache::new(dir)
    }

    #[tokio::test]
    async fn second_lookup_reuses_the_first_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(dir.path());
        let builds = AtomicUsize::new(0);

        let first = cache
            .get_or_try_init(
                "kimi",
                || async {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(7)
                },
                |_| true,
            )
            .await
            .unwrap();
        let second = cache
            .get_or_try_init(
                "kimi",
                || async {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(9)
                },
                |_| true,
            )
            .await
            .unwrap();

        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(*first, 7);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn distinct_providers_get_distinct_runtimes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(dir.path());

        let kimi = cache
            .get_or_try_init("kimi", || async { Ok(1) }, |_| true)
            .await
            .unwrap();
        let glm = cache
            .get_or_try_init("glm", || async { Ok(2) }, |_| true)
            .await
            .unwrap();

        assert_eq!((*kimi, *glm), (1, 2));
        assert_eq!(cache.len().await, 2);
    }

    #[tokio::test]
    async fn provider_ids_differing_only_by_case_share_a_slot() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(dir.path());
        let builds = AtomicUsize::new(0);

        let first = cache
            .get_or_try_init(
                "Kimi",
                || async {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(1)
                },
                |_| true,
            )
            .await
            .unwrap();
        let second = cache
            .get_or_try_init(
                " kimi ",
                || async {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(2)
                },
                |_| true,
            )
            .await
            .unwrap();

        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.len().await, 1);
    }

    #[tokio::test]
    async fn uncacheable_runtimes_build_fresh_every_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(dir.path());
        let builds = AtomicUsize::new(0);

        let first = cache
            .get_or_try_init(
                "plugin",
                || async {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(1)
                },
                |_| false,
            )
            .await
            .unwrap();
        // The verdict is remembered: the second lookup rebuilds without
        // consulting the predicate again.
        let second = cache
            .get_or_try_init(
                "plugin",
                || async {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(2)
                },
                |_| true,
            )
            .await
            .unwrap();

        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert_eq!((*first, *second), (1, 2));
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn editing_config_json_drops_cached_runtimes() {
        let dir = tempfile::tempdir().unwrap();
        let config = rebon_config::config_json_path(dir.path());
        std::fs::write(&config, b"{}").unwrap();
        let cache = cache_in(dir.path());

        let before = cache
            .get_or_try_init("kimi", || async { Ok(1) }, |_| true)
            .await
            .unwrap();
        // A different length is enough; mtime resolution varies per filesystem.
        std::fs::write(&config, b"{\"providers\":[]}").unwrap();
        let after = cache
            .get_or_try_init("kimi", || async { Ok(2) }, |_| true)
            .await
            .unwrap();

        assert_eq!((*before, *after), (1, 2));
    }

    #[tokio::test]
    async fn a_missing_config_json_does_not_read_as_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(dir.path());

        let before = cache
            .get_or_try_init("env", || async { Ok(1) }, |_| true)
            .await
            .unwrap();
        let after = cache
            .get_or_try_init("env", || async { Ok(2) }, |_| true)
            .await
            .unwrap();

        assert_eq!((*before, *after), (1, 1));
    }

    #[tokio::test]
    async fn idle_entries_are_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let cache =
            ProviderRuntimeCache::<u32>::with_idle_ttl(dir.path(), Duration::from_millis(1));

        let before = cache
            .get_or_try_init("kimi", || async { Ok(1) }, |_| true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let after = cache
            .get_or_try_init("kimi", || async { Ok(2) }, |_| true)
            .await
            .unwrap();

        assert_eq!((*before, *after), (1, 2));
        assert_eq!(cache.len().await, 1);
    }

    #[tokio::test]
    async fn a_failed_build_is_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(dir.path());

        let failed = cache
            .get_or_try_init(
                "kimi",
                || async { anyhow::bail!("plugin unavailable") },
                |_: &u32| true,
            )
            .await;
        assert!(failed.is_err());

        let retried = cache
            .get_or_try_init("kimi", || async { Ok(3) }, |_| true)
            .await
            .unwrap();
        assert_eq!(*retried, 3);
    }

    #[tokio::test]
    async fn concurrent_lookups_build_once() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(cache_in(dir.path()));
        let builds = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let builds = builds.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_try_init(
                        "kimi",
                        || async {
                            builds.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            Ok(5)
                        },
                        |_| true,
                    )
                    .await
                    .unwrap()
            }));
        }
        let results = futures_util::future::join_all(tasks).await;

        assert_eq!(builds.load(Ordering::SeqCst), 1);
        for result in results {
            assert_eq!(*result.unwrap(), 5);
        }
    }
}
