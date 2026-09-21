//! Session-scoped cache for auto-mode classifier verdicts and explicit
//! one-shot exemptions.
//!
//! Allow keys cover the complete evaluation context (tool input, transcript,
//! cwd, and isolation metadata), so newly observed context cannot inherit a
//! stale allow. Denial keys deliberately stop at the latest user turn: an
//! autonomous retry cannot re-roll a block merely by appending its own tool
//! result, while new user intent still earns a fresh classification. Exemptions
//! remain keyed by the raw tool invocation because `/permissions approve`
//! stores only the denied tool name and input.
//!
//! Two problems this solves:
//!
//! * **Deny stickiness** — the classifier is stochastic. Without a cache, an
//!   agent that resubmits the same action can append its own denial result and
//!   re-roll the classifier until an allow lands. Denial keys preserve the
//!   authorization context through the latest user turn, so only new user
//!   intent, changed input, or changed execution context earns a fresh verdict.
//! * **Record dedup** — the denial ring buffer holds 20 records. An
//!   autonomous loop hammering one denied call must not evict every
//!   other pending record; callers consult [`AutoModeVerdictCache::mark_denied`]
//!   and only append a store record for a *newly* denied fingerprint.
//!
//! It also carries the one-shot exemptions installed by `/permissions
//! approve`: the user reviewed one exact denied invocation, so the next
//! identical call slides through the auto-mode gate once. Consuming the
//! exemption re-arms the deny for any calls after that.
//!
//! Like the rest of this crate the cache is pure data over `std` — the caller
//! supplies opaque verdict keys and serialized invocation JSON; no clock is
//! consulted.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Cap on remembered denials. Eviction is insertion-ordered (oldest
/// out); an evicted fingerprint simply earns a fresh verdict next time,
/// which is the safe direction.
const DENIED_CAPACITY: usize = 64;
/// Cap on outstanding one-shot exemptions. These are installed one at a
/// time by explicit user action, so the bound exists only as a hard
/// backstop.
const EXEMPTION_CAPACITY: usize = 32;
/// Cap on remembered classifier allows. Large fan-outs (workflow
/// agents re-running near-identical commands) are the reason this
/// exists; eviction just means one extra classifier round-trip.
const ALLOWED_CAPACITY: usize = 128;

#[derive(Debug, Default)]
struct VerdictCacheInner {
    /// fingerprint → deny reason, insertion-ordered for eviction.
    denied: VecDeque<(String, String)>,
    /// Fingerprints the user approved in `/permissions`; each entry
    /// admits exactly one future identical call.
    exemptions: VecDeque<String>,
    /// Fingerprints the classifier allowed, insertion-ordered for eviction.
    /// Holds classifier verdicts only — never one-shot user exemptions — so
    /// replaying an entry means the complete evaluation key is unchanged.
    allowed: VecDeque<String>,
}

/// See the module docs. Shared as `Arc<AutoModeVerdictCache>` between
/// the engine's permission broker (deny/exempt checks on the tool
/// dispatch path) and the UI (`/permissions approve` installs
/// exemptions).
#[derive(Debug, Default)]
pub struct AutoModeVerdictCache {
    inner: Mutex<VerdictCacheInner>,
}

impl AutoModeVerdictCache {
    fn fingerprint(tool_name: &str, key_material: &str) -> String {
        // The NUL separator cannot occur inside a tool name, so the key cannot
        // be forged by key material whose text embeds a tool name.
        format!("{tool_name}\u{0}{key_material}")
    }

    /// Reason this classifier authorization context was previously denied, if
    /// any.
    pub fn denied_reason(&self, tool_name: &str, verdict_key: &str) -> Option<String> {
        let key = Self::fingerprint(tool_name, verdict_key);
        let inner = self.inner.lock().expect("verdict cache poisoned");
        inner
            .denied
            .iter()
            .find(|(fingerprint, _)| *fingerprint == key)
            .map(|(_, reason)| reason.clone())
    }

    /// Record a denial. Returns `true` when the fingerprint is new —
    /// the caller should append a denial-store record only then, so a
    /// hammered retry cannot flood the ring buffer.
    pub fn mark_denied(&self, tool_name: &str, verdict_key: &str, reason: &str) -> bool {
        let key = Self::fingerprint(tool_name, verdict_key);
        let Ok(mut inner) = self.inner.lock() else {
            // A poisoned cache degrades to "everything is fresh":
            // verdicts still flow, only dedup/stickiness is lost.
            return true;
        };
        if inner
            .denied
            .iter()
            .any(|(fingerprint, _)| *fingerprint == key)
        {
            return false;
        }
        // A denial supersedes any remembered allow for the same call.
        inner.allowed.retain(|entry| *entry != key);
        inner.denied.push_back((key, reason.to_string()));
        while inner.denied.len() > DENIED_CAPACITY {
            inner.denied.pop_front();
        }
        true
    }

    /// True when the classifier previously allowed this exact evaluation
    /// context. Replaying a remembered allow avoids a duplicate model request
    /// without allowing the verdict to survive changed user intent.
    pub fn is_allowed(&self, tool_name: &str, verdict_key: &str) -> bool {
        let key = Self::fingerprint(tool_name, verdict_key);
        let Ok(inner) = self.inner.lock() else {
            return false;
        };
        inner.allowed.iter().any(|entry| *entry == key)
    }

    /// Record a classifier allow for this exact evaluation context. Never call
    /// this for one-shot user exemptions — those admit a single call by design.
    pub fn mark_allowed(&self, tool_name: &str, verdict_key: &str) {
        let key = Self::fingerprint(tool_name, verdict_key);
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if inner.allowed.iter().any(|entry| *entry == key) {
            return;
        }
        inner.allowed.push_back(key);
        while inner.allowed.len() > ALLOWED_CAPACITY {
            inner.allowed.pop_front();
        }
    }

    /// Forget a cached classifier verdict for one exact cache key.
    /// One-shot approvals use this after consuming the raw invocation
    /// exemption so the following retry is classified again rather than
    /// inheriting the verdict the user just overrode.
    pub fn forget_verdict(&self, tool_name: &str, verdict_key: &str) {
        let key = Self::fingerprint(tool_name, verdict_key);
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.denied.retain(|(fingerprint, _)| *fingerprint != key);
        inner.allowed.retain(|fingerprint| *fingerprint != key);
    }

    /// Install a one-shot exemption for this raw invocation. The engine clears
    /// the matching context-aware verdict when it consumes the exemption.
    pub fn exempt_once(&self, tool_name: &str, tool_input_json: &str) {
        let key = Self::fingerprint(tool_name, tool_input_json);
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.denied.retain(|(fingerprint, _)| *fingerprint != key);
        if !inner.exemptions.iter().any(|entry| *entry == key) {
            inner.exemptions.push_back(key);
            while inner.exemptions.len() > EXEMPTION_CAPACITY {
                inner.exemptions.pop_front();
            }
        }
    }

    /// Consume the exemption for this exact invocation, if one is
    /// outstanding. Returns `true` at most once per [`Self::exempt_once`].
    pub fn take_exemption(&self, tool_name: &str, tool_input_json: &str) -> bool {
        let key = Self::fingerprint(tool_name, tool_input_json);
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        let before = inner.exemptions.len();
        inner.exemptions.retain(|entry| *entry != key);
        inner.exemptions.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denied_fingerprints_replay_deterministically() {
        let cache = AutoModeVerdictCache::default();
        assert!(cache.denied_reason("Bash", "{\"command\":\"x\"}").is_none());
        assert!(cache.mark_denied("Bash", "{\"command\":\"x\"}", "deletes files"));
        assert_eq!(
            cache
                .denied_reason("Bash", "{\"command\":\"x\"}")
                .as_deref(),
            Some("deletes files")
        );
        // Identical resubmission is not a fresh denial.
        assert!(!cache.mark_denied("Bash", "{\"command\":\"x\"}", "deletes files"));
        // A different invocation is independent.
        assert!(cache.denied_reason("Bash", "{\"command\":\"y\"}").is_none());
        // Same input under a different tool is a different fingerprint.
        assert!(cache
            .denied_reason("PowerShell", "{\"command\":\"x\"}")
            .is_none());
    }

    #[test]
    fn exemption_is_consumed_exactly_once_and_lifts_the_denial() {
        let cache = AutoModeVerdictCache::default();
        cache.mark_denied("Bash", "{}", "denied");
        cache.exempt_once("Bash", "{}");
        assert!(
            cache.denied_reason("Bash", "{}").is_none(),
            "approval must lift the remembered denial"
        );
        assert!(cache.take_exemption("Bash", "{}"));
        assert!(
            !cache.take_exemption("Bash", "{}"),
            "the exemption is one-shot"
        );
    }

    #[test]
    fn take_exemption_without_install_is_false() {
        let cache = AutoModeVerdictCache::default();
        assert!(!cache.take_exemption("Bash", "{}"));
    }

    #[test]
    fn denied_cache_evicts_oldest_beyond_capacity() {
        let cache = AutoModeVerdictCache::default();
        for index in 0..=DENIED_CAPACITY {
            assert!(cache.mark_denied("Bash", &format!("{{\"i\":{index}}}"), "r"));
        }
        assert!(
            cache.denied_reason("Bash", "{\"i\":0}").is_none(),
            "oldest entry must be evicted"
        );
        assert!(cache
            .denied_reason("Bash", &format!("{{\"i\":{DENIED_CAPACITY}}}"))
            .is_some());
    }

    #[test]
    fn tool_name_cannot_be_forged_via_input_text() {
        let cache = AutoModeVerdictCache::default();
        cache.mark_denied("Bash", "x", "r");
        assert!(cache.denied_reason("Bas", "hx").is_none());
    }

    #[test]
    fn allowed_fingerprints_replay_and_a_denial_supersedes_them() {
        let cache = AutoModeVerdictCache::default();
        assert!(!cache.is_allowed("Bash", "{\"command\":\"x\"}"));
        cache.mark_allowed("Bash", "{\"command\":\"x\"}");
        assert!(cache.is_allowed("Bash", "{\"command\":\"x\"}"));
        // A different invocation / tool is independent.
        assert!(!cache.is_allowed("Bash", "{\"command\":\"y\"}"));
        assert!(!cache.is_allowed("PowerShell", "{\"command\":\"x\"}"));
        // A later denial for the same call wins.
        cache.mark_denied("Bash", "{\"command\":\"x\"}", "r");
        assert!(!cache.is_allowed("Bash", "{\"command\":\"x\"}"));
    }

    #[test]
    fn allowed_cache_evicts_oldest_beyond_capacity() {
        let cache = AutoModeVerdictCache::default();
        for index in 0..=ALLOWED_CAPACITY {
            cache.mark_allowed("Bash", &format!("{{\"i\":{index}}}"));
        }
        assert!(!cache.is_allowed("Bash", "{\"i\":0}"));
        assert!(cache.is_allowed("Bash", &format!("{{\"i\":{ALLOWED_CAPACITY}}}")));
    }
}
