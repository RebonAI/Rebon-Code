//! Sink trait + in-memory sink for routing
//! [`crate::auto_mode_denials::AutoModeDenialInput`] records from the
//! engine broker into a session-scoped
//! [`crate::auto_mode_denials::AutoModeDenialStore`].
//!
//! The permissions crate depends on no `rebon-*` crate but `rebon-tools-core`
//! (the shared allow/deny/ask verdict) and `rebon-shell-policy` (the shell
//! tokenisers); this crate's `Cargo.toml` carries the allowlist and the
//! `only_allowed_rebon_deps_in_cargo_toml` test in `lib.rs` enforces it. The
//! engine broker is therefore not allowed to reach into the UI's app state
//! directly. Instead we expose a small trait here — the UI provides the
//! concrete store-backed sink and hands a `dyn AutoModeDenialSink` down into
//! the broker.
//!
//! Storage stays explicit and swappable for tests rather than being a
//! module-level singleton.
//!
//! Also includes a trivial [`PermissionModeProvider`] trait so the
//! broker can ask "is the session currently in auto mode?" without
//! pulling in the settings panel or the UI state crate. This is the
//! mechanical piece that lets "auto mode" actually behave differently
//! at runtime — the broker short-circuits `Ask` to `Allow` when the
//! provider says so.

use std::sync::{Arc, Mutex};

use crate::auto_mode_denials::{AutoModeDenialInput, AutoModeDenialStore};
use crate::types::PermissionMode;
use crate::verdict_cache::AutoModeVerdictCache;

/// Sink for auto-mode denial records.
///
/// `record` must be cheap and non-blocking — it is called on the tool
/// dispatch critical path. Implementations typically enqueue the
/// record into a mutex-guarded store or forward to a channel.
pub trait AutoModeDenialSink: Send + Sync {
    /// Record a denial. Returns the assigned id on success; `None` if
    /// the sink chose to drop the record (e.g. rate-limiting). The
    /// default no-op sink ([`NullDenialSink`]) always returns `None`.
    fn record(&self, input: AutoModeDenialInput) -> Option<String>;
}

/// Source-of-truth for "what permission mode is active right now".
///
/// The engine needs to branch on this every tool dispatch — in `Auto`
/// the broker short-circuits `Ask` to `Allow` (this IS the auto-mode
/// UX), and `Deny` records into the sink. Pushing the mode through
/// a trait keeps this crate and the engine independent of the
/// UI's app state.
pub trait PermissionModeProvider: Send + Sync {
    fn current_mode(&self) -> PermissionMode;
}

/// Convenience impl — a boxed closure is always a valid provider.
impl<F> PermissionModeProvider for F
where
    F: Fn() -> PermissionMode + Send + Sync + 'static,
{
    fn current_mode(&self) -> PermissionMode {
        self()
    }
}

/// Sink that drops every record. Useful as the default when a caller
/// does not plug a real store in — keeps downstream code branch-free.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullDenialSink;

impl AutoModeDenialSink for NullDenialSink {
    fn record(&self, _input: AutoModeDenialInput) -> Option<String> {
        None
    }
}

/// Default sink — `Arc<Mutex<AutoModeDenialStore>>`. This is the shape
/// the UI holds in its app state; sharing it with the engine broker via
/// [`Arc<dyn AutoModeDenialSink>`] lets the broker record from a
/// `tokio` task without the UI giving up ownership of the store.
#[derive(Debug, Clone, Default)]
pub struct SharedDenialSink {
    store: Arc<Mutex<AutoModeDenialStore>>,
}

impl SharedDenialSink {
    pub fn new(store: Arc<Mutex<AutoModeDenialStore>>) -> Self {
        Self { store }
    }

    /// Construct an empty store behind the sink. Convenience for
    /// callers that haven't allocated a store yet.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Borrow the underlying store handle — the UI uses this to
    /// enumerate denials when opening `/permissions`.
    pub fn store(&self) -> Arc<Mutex<AutoModeDenialStore>> {
        Arc::clone(&self.store)
    }
}

impl AutoModeDenialSink for SharedDenialSink {
    fn record(&self, input: AutoModeDenialInput) -> Option<String> {
        let mut guard = self.store.lock().expect("denial store poisoned");
        Some(guard.record(input))
    }
}

/// Wrap an `Arc<dyn AutoModeDenialSink>` + `Arc<dyn
/// PermissionModeProvider>` pair. This is the handle the engine
/// broker takes — a single value it can clone cheaply, rather than
/// two separate fields.
#[derive(Clone)]
pub struct AutoModeHooks {
    pub sink: Arc<dyn AutoModeDenialSink>,
    pub mode: Arc<dyn PermissionModeProvider>,
    /// Deny/exemption fingerprint cache shared with the UI: the broker
    /// replays remembered denials deterministically and honors one-shot
    /// `/permissions approve` exemptions from here.
    verdicts: Arc<AutoModeVerdictCache>,
}

impl AutoModeHooks {
    pub fn new(sink: Arc<dyn AutoModeDenialSink>, mode: Arc<dyn PermissionModeProvider>) -> Self {
        Self {
            sink,
            mode,
            verdicts: Arc::new(AutoModeVerdictCache::default()),
        }
    }

    /// Share an externally owned verdict cache (the UI keeps the same
    /// handle so `/permissions approve` can install exemptions).
    pub fn with_verdicts(mut self, verdicts: Arc<AutoModeVerdictCache>) -> Self {
        self.verdicts = verdicts;
        self
    }

    pub fn verdicts(&self) -> &Arc<AutoModeVerdictCache> {
        &self.verdicts
    }

    /// The permission mode active right now, straight from the provider.
    pub fn current_mode(&self) -> PermissionMode {
        self.mode.current_mode()
    }

    /// Short-hand — true when [`PermissionModeProvider::current_mode`]
    /// returns [`PermissionMode::Auto`].
    pub fn is_auto(&self) -> bool {
        self.current_mode() == PermissionMode::Auto
    }

    /// Short-hand — true when the session is in
    /// [`PermissionMode::BypassPermissions`]. The broker skips approval
    /// dialogs outright in that mode; unlike auto it consults neither the
    /// classifier nor the denial sink, because nothing is being judged.
    pub fn is_bypass(&self) -> bool {
        self.current_mode() == PermissionMode::BypassPermissions
    }
}

impl std::fmt::Debug for AutoModeHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoModeHooks").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_input() -> AutoModeDenialInput {
        AutoModeDenialInput {
            tool_use_id: "tool-1".to_string(),
            tool_name: "Bash".to_string(),
            tool_input: "{}".to_string(),
            reason: "r".to_string(),
            display: "d".to_string(),
            timestamp_ms: 1,
            task_id: None,
            conversation_id: None,
        }
    }

    #[test]
    fn null_sink_drops_record_and_returns_none() {
        let sink = NullDenialSink;
        assert!(sink.record(make_input()).is_none());
    }

    #[test]
    fn shared_sink_pushes_into_store_and_returns_id() {
        let sink = SharedDenialSink::empty();
        let id = sink.record(make_input()).expect("sink should accept");
        let store = sink.store();
        let guard = store.lock().unwrap();
        assert_eq!(guard.len(), 1);
        assert!(guard.get(&id).is_some());
    }

    #[test]
    fn shared_sink_shares_underlying_store() {
        let store = Arc::new(Mutex::new(AutoModeDenialStore::default()));
        let sink_a = SharedDenialSink::new(Arc::clone(&store));
        let sink_b = SharedDenialSink::new(Arc::clone(&store));
        sink_a.record(make_input());
        sink_b.record(make_input());
        assert_eq!(store.lock().unwrap().len(), 2);
    }

    #[test]
    fn closure_impls_permission_mode_provider() {
        let mode = PermissionMode::Auto;
        let provider: Arc<dyn PermissionModeProvider> = Arc::new(move || mode);
        assert_eq!(provider.current_mode(), PermissionMode::Auto);
    }

    #[test]
    fn hooks_is_bypass_only_matches_the_bypass_mode() {
        for (mode, expected) in [
            (PermissionMode::BypassPermissions, true),
            (PermissionMode::Auto, false),
            (PermissionMode::AcceptEdits, false),
            (PermissionMode::Default, false),
            (PermissionMode::Plan, false),
            (PermissionMode::DontAsk, false),
        ] {
            let provider: Arc<dyn PermissionModeProvider> = Arc::new(move || mode);
            let hooks = AutoModeHooks::new(Arc::new(NullDenialSink), provider);
            assert_eq!(hooks.is_bypass(), expected, "mode={mode:?}");
            assert_eq!(hooks.current_mode(), mode);
        }
    }

    #[test]
    fn hooks_is_auto_tracks_provider() {
        let mode_cell = Arc::new(Mutex::new(PermissionMode::Default));
        let cloned = Arc::clone(&mode_cell);
        let provider: Arc<dyn PermissionModeProvider> = Arc::new(move || *cloned.lock().unwrap());
        let hooks = AutoModeHooks::new(Arc::new(NullDenialSink), provider);
        assert!(!hooks.is_auto());
        *mode_cell.lock().unwrap() = PermissionMode::Auto;
        assert!(hooks.is_auto());
        *mode_cell.lock().unwrap() = PermissionMode::Plan;
        assert!(!hooks.is_auto());
    }
}
