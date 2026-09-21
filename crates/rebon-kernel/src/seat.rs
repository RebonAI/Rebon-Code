//! Reusable provider registry for **arbitrated single-slot seats** — the
//! second of the two resolution patterns a seat may follow.
//!
//! A single-slot seat (web search, web fetch, …) holds many registered
//! providers but serves requests through exactly one, chosen at **execution
//! time** so registration order never matters:
//!
//! 1. a **configured** provider id wins — an unknown id fails loudly with the
//!    known set, an unavailable one fails `Unavailable`;
//! 2. otherwise exactly one available provider auto-selects;
//! 3. zero available → [`SeatError::Unavailable`];
//! 4. more than one available and none configured → [`SeatError::Ambiguous`]
//!    listing the candidates — the seat never guesses.
//!
//! Registration follows the kernel registration primitive: `register`
//! returns a [`Disposer`] (tie it to a [`crate::Context`] via
//! `ctx.effect_labeled`), duplicate ids are rejected, and a stale disposer
//! never removes a successor registration (token-guarded, same rule as the
//! service registry).
//!
//! Keyed-route seats (an exact `{provider, model}` table, say) are the other
//! pattern and do not use this type.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::{Disposer, KernelError};

/// Stable machine-readable codes for arbitration failures. They prefix the
/// `Display` output so JSON-plane consumers can match on them.
pub const SEAT_PROVIDER_UNKNOWN: &str = "SEAT_PROVIDER_UNKNOWN";
pub const SEAT_PROVIDER_UNAVAILABLE: &str = "SEAT_PROVIDER_UNAVAILABLE";
pub const SEAT_PROVIDER_AMBIGUOUS: &str = "SEAT_PROVIDER_AMBIGUOUS";

/// Why arbitration produced no provider.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SeatError {
    #[error(
        "SEAT_PROVIDER_UNKNOWN: seat `{seat}` has no provider `{configured}` (known: {known:?})"
    )]
    UnknownProvider {
        seat: &'static str,
        configured: String,
        known: Vec<String>,
    },
    #[error("SEAT_PROVIDER_UNAVAILABLE: seat `{seat}` has no available provider")]
    Unavailable { seat: &'static str },
    #[error("SEAT_PROVIDER_AMBIGUOUS: seat `{seat}` has multiple available providers ({candidates:?}) and none configured")]
    Ambiguous {
        seat: &'static str,
        candidates: Vec<String>,
    },
}

impl SeatError {
    /// The stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            SeatError::UnknownProvider { .. } => SEAT_PROVIDER_UNKNOWN,
            SeatError::Unavailable { .. } => SEAT_PROVIDER_UNAVAILABLE,
            SeatError::Ambiguous { .. } => SEAT_PROVIDER_AMBIGUOUS,
        }
    }
}

struct SeatEntry<P> {
    id: String,
    token: u64,
    provider: P,
}

/// Provider registry for one arbitrated single-slot seat.
pub struct SeatRegistry<P> {
    seat: &'static str,
    entries: RwLock<Vec<SeatEntry<P>>>,
    next_token: AtomicU64,
}

impl<P: Clone + Send + Sync + 'static> SeatRegistry<P> {
    pub fn new(seat: &'static str) -> Arc<Self> {
        Arc::new(Self {
            seat,
            entries: RwLock::new(Vec::new()),
            next_token: AtomicU64::new(0),
        })
    }

    /// The seat name this registry arbitrates for.
    pub fn seat(&self) -> &'static str {
        self.seat
    }

    /// Register a provider. Duplicate ids are rejected. The returned
    /// [`Disposer`] removes exactly this registration (a stale disposer
    /// never removes a successor under the same id) — tie it to the
    /// registering plugin's [`crate::Context`]:
    ///
    /// ```ignore
    /// ctx.effect_labeled("web provider(exa)", || registry.register("exa", provider)?);
    /// ```
    pub fn register(self: &Arc<Self>, id: &str, provider: P) -> Result<Disposer, KernelError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(KernelError::Other(format!(
                "seat `{}` provider id must be non-empty",
                self.seat
            )));
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut entries = self.entries.write().unwrap();
            if entries.iter().any(|e| e.id == id) {
                return Err(KernelError::DuplicateProvider {
                    plugin: String::new(),
                    service: format!("{}:{id}", self.seat),
                });
            }
            entries.push(SeatEntry {
                id: id.to_string(),
                token,
                provider,
            });
        }
        let weak = Arc::downgrade(self);
        let id = id.to_string();
        Ok(Disposer::new(move || {
            if let Some(registry) = weak.upgrade() {
                registry
                    .entries
                    .write()
                    .unwrap()
                    .retain(|e| !(e.id == id && e.token == token));
            }
        }))
    }

    /// Registered provider ids, in registration order.
    pub fn ids(&self) -> Vec<String> {
        self.entries
            .read()
            .unwrap()
            .iter()
            .map(|e| e.id.clone())
            .collect()
    }

    /// Fetch one provider by id.
    pub fn get(&self, id: &str) -> Option<P> {
        self.entries
            .read()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.provider.clone())
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Arbitrate with every registered provider considered available.
    pub fn resolve(&self, configured: Option<&str>) -> Result<(String, P), SeatError> {
        self.resolve_with(configured, |_| true)
    }

    /// Arbitrate at execution time: configured id wins, else the sole
    /// available provider, else a loud three-state error. `available` lets
    /// a seat model an `available()` probe (missing credentials, …).
    pub fn resolve_with(
        &self,
        configured: Option<&str>,
        available: impl Fn(&P) -> bool,
    ) -> Result<(String, P), SeatError> {
        let entries = self.entries.read().unwrap();
        if let Some(configured) = configured.map(str::trim).filter(|c| !c.is_empty()) {
            let Some(entry) = entries.iter().find(|e| e.id == configured) else {
                return Err(SeatError::UnknownProvider {
                    seat: self.seat,
                    configured: configured.to_string(),
                    known: entries.iter().map(|e| e.id.clone()).collect(),
                });
            };
            if !available(&entry.provider) {
                return Err(SeatError::Unavailable { seat: self.seat });
            }
            return Ok((entry.id.clone(), entry.provider.clone()));
        }
        let candidates: Vec<&SeatEntry<P>> =
            entries.iter().filter(|e| available(&e.provider)).collect();
        match candidates.as_slice() {
            [] => Err(SeatError::Unavailable { seat: self.seat }),
            [sole] => Ok((sole.id.clone(), sole.provider.clone())),
            many => Err(SeatError::Ambiguous {
                seat: self.seat,
                candidates: many.iter().map(|e| e.id.clone()).collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitration_three_states_and_configured_wins() {
        let registry: Arc<SeatRegistry<&'static str>> = SeatRegistry::new("probe");

        // Zero providers → Unavailable.
        assert_eq!(
            registry.resolve(None),
            Err(SeatError::Unavailable { seat: "probe" })
        );

        // Exactly one → auto-selects.
        let d_a = registry.register("a", "provider-a").unwrap();
        assert_eq!(registry.resolve(None), Ok(("a".into(), "provider-a")));

        // Two and none configured → Ambiguous, candidates listed.
        let _d_b = registry.register("b", "provider-b").unwrap();
        assert_eq!(
            registry.resolve(None),
            Err(SeatError::Ambiguous {
                seat: "probe",
                candidates: vec!["a".into(), "b".into()]
            })
        );

        // Configured id wins over auto-selection.
        assert_eq!(registry.resolve(Some("b")), Ok(("b".into(), "provider-b")));

        // Unknown configured id fails loudly with the known set.
        let err = registry.resolve(Some("nope")).unwrap_err();
        assert_eq!(err.code(), SEAT_PROVIDER_UNKNOWN);
        assert!(err.to_string().contains("[\"a\", \"b\"]"), "{err}");

        // Disposal returns arbitration to the sole-provider state.
        d_a.dispose();
        assert_eq!(registry.resolve(None), Ok(("b".into(), "provider-b")));
    }

    #[test]
    fn availability_probe_gates_arbitration() {
        let registry: Arc<SeatRegistry<u32>> = SeatRegistry::new("probe");
        let _d1 = registry.register("ready", 1).unwrap();
        let _d2 = registry.register("cold", 2).unwrap();

        // Only one passes the probe → auto-selects despite two registered.
        assert_eq!(
            registry.resolve_with(None, |p| *p == 1),
            Ok(("ready".into(), 1))
        );
        // A configured-but-unavailable provider is Unavailable, not a fallback.
        assert_eq!(
            registry.resolve_with(Some("cold"), |p| *p == 1),
            Err(SeatError::Unavailable { seat: "probe" })
        );
    }

    #[test]
    fn duplicate_ids_rejected_and_disposal_frees_the_id() {
        let registry: Arc<SeatRegistry<u32>> = SeatRegistry::new("probe");
        let first = registry.register("x", 1).unwrap();
        assert!(matches!(
            registry.register("x", 2),
            Err(KernelError::DuplicateProvider { .. })
        ));

        // Disposal frees the id for a successor. (The token in the removal
        // closure is defense-in-depth mirroring the service registry; with
        // consume-once disposers and the duplicate check there is currently
        // no reachable stale-disposer path.)
        first.dispose();
        let _second = registry.register("x", 3).unwrap();
        assert_eq!(registry.get("x"), Some(3));
        assert_eq!(registry.len(), 1);
    }
}
