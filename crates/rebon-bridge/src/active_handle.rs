//! The slot holding the bridge handle that is currently active.
//!
//! Callers that do not own the bridge — tools, slash commands — need to reach
//! the running one without a handle threaded through every call chain. This
//! module gives them that reach-through as plain bookkeeping: a set/get/compat
//! id lens with no I/O of its own. Publishing the new compat id to the session
//! record is the one side effect `set` implies, and it stays caller-owned.
//!
//! * [`BridgeHandle`] — implemented on a caller's concrete handle type. The
//!   only thing it requires is `bridge_session_id(&self) -> &str`; anything
//!   richer (message writing, PR subscription) belongs in the caller's own
//!   layer.
//! * [`ActiveHandleSlot<H>`] — a thread-safe `Option<Arc<H>>` that starts
//!   empty and can be set, cleared and read.
//! * [`ActiveHandleChange`] — what [`ActiveHandleSlot::set`] hands back: the
//!   new `session_*` compat id, so the caller can publish it without this
//!   crate knowing where it goes.
//!
//! A slot is **not** a process global. A caller that wants singleton semantics
//! parks its own in a static; a caller that does not is never forced into
//! one.

use std::sync::{Arc, RwLock};

use crate::session_id_compat::to_compat_session_id;

/// Minimum contract a concrete bridge handle must satisfy to live in
/// an [`ActiveHandleSlot`].
///
/// Deliberately narrower than a full bridge handle — this crate is
/// the foundation module; richer handle methods (PR subscription,
/// message writing, etc.) belong in the heavier bridge integration
/// layer.
pub trait BridgeHandle: Send + Sync {
    /// The id this handle is keyed by — `ReplBridgeHandle` answers with its
    /// environment id. The slot runs whatever comes back through
    /// [`to_compat_session_id`] when callers ask for the compat-form id.
    fn bridge_session_id(&self) -> &str;

    /// Best-effort runtime status snapshot for UI surfaces.
    fn runtime_status(&self) -> Option<crate::runtime::RuntimeStatus> {
        None
    }

    /// Best-effort last error string for UI surfaces.
    fn last_error(&self) -> Option<String> {
        None
    }

    /// Gracefully shut down the underlying runtime, if any.
    fn shutdown_blocking(&self, _runtime: &tokio::runtime::Handle) -> Result<(), String> {
        Ok(())
    }
}

/// Thread-safe slot for the current active [`BridgeHandle`].
///
/// Use [`ActiveHandleSlot::set`] to install / clear, [`ActiveHandleSlot::get`]
/// to read, and [`ActiveHandleSlot::self_bridge_compat_id`] for the
/// compat-form id of the installed handle.
#[derive(Default)]
pub struct ActiveHandleSlot<H: BridgeHandle + ?Sized> {
    inner: RwLock<Option<Arc<H>>>,
}

/// Outcome of [`ActiveHandleSlot::set`].
///
/// The bridge layer reads `new_compat_id` and publishes it itself.
/// Keeping the side-effect
/// as a value the caller consumes (rather than an injected closure)
/// means this crate has no stake in the caller's async/await or
/// logging machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveHandleChange {
    /// The compat-form (`session_*`) id of the new handle, or `None`
    /// if the slot was cleared.
    pub new_compat_id: Option<String>,
}

impl<H: BridgeHandle + ?Sized> ActiveHandleSlot<H> {
    /// Construct an empty slot.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }

    /// Install `handle` (or clear with `None`). Returns the compat id
    /// the caller should publish (`None` when the slot was cleared).
    pub fn set(&self, handle: Option<Arc<H>>) -> ActiveHandleChange {
        let new_compat_id = handle
            .as_ref()
            .map(|h| to_compat_session_id(h.bridge_session_id()));
        let mut guard = self.inner.write().expect("active handle slot poisoned");
        *guard = handle;
        ActiveHandleChange { new_compat_id }
    }

    /// Borrow the current handle as a cheap `Arc` clone. Returns
    /// `None` when the slot is empty.
    pub fn get(&self) -> Option<Arc<H>> {
        let guard = self.inner.read().expect("active handle slot poisoned");
        guard.clone()
    }

    /// Return the compat-form id of the currently installed handle,
    /// or `None` when the slot is empty.
    pub fn self_bridge_compat_id(&self) -> Option<String> {
        let guard = self.inner.read().expect("active handle slot poisoned");
        guard
            .as_ref()
            .map(|h| to_compat_session_id(h.bridge_session_id()))
    }

    /// True when a handle is installed.
    pub fn is_set(&self) -> bool {
        let guard = self.inner.read().expect("active handle slot poisoned");
        guard.is_some()
    }

    /// Clear the slot. Convenience wrapper around `set(None)`.
    pub fn clear(&self) -> ActiveHandleChange {
        self.set(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_id_compat::{clear_cse_shim_gate, set_cse_shim_gate};
    use std::sync::Mutex;
    use std::sync::OnceLock;

    // Tests in this module reach into the shared `cse_shim` gate; run
    // them serialized against other gate-touching tests.
    fn gate_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn with_clean_gate<R>(body: impl FnOnce() -> R) -> R {
        let _guard = gate_lock().lock().unwrap_or_else(|p| p.into_inner());
        clear_cse_shim_gate();
        let out = body();
        clear_cse_shim_gate();
        out
    }

    struct TestHandle {
        id: String,
    }

    impl BridgeHandle for TestHandle {
        fn bridge_session_id(&self) -> &str {
            &self.id
        }
    }

    fn handle(id: &str) -> Arc<TestHandle> {
        Arc::new(TestHandle { id: id.to_string() })
    }

    #[test]
    fn new_slot_is_empty() {
        let slot: ActiveHandleSlot<TestHandle> = ActiveHandleSlot::new();
        assert!(slot.get().is_none());
        assert!(!slot.is_set());
        assert_eq!(slot.self_bridge_compat_id(), None);
    }

    #[test]
    fn set_some_returns_compat_id_for_cse_prefix() {
        with_clean_gate(|| {
            let slot: ActiveHandleSlot<TestHandle> = ActiveHandleSlot::new();
            let change = slot.set(Some(handle("cse_abc")));
            assert_eq!(change.new_compat_id.as_deref(), Some("session_abc"));
            assert!(slot.is_set());
        });
    }

    #[test]
    fn set_some_returns_unchanged_id_for_session_prefix() {
        with_clean_gate(|| {
            let slot: ActiveHandleSlot<TestHandle> = ActiveHandleSlot::new();
            let change = slot.set(Some(handle("session_xyz")));
            assert_eq!(change.new_compat_id.as_deref(), Some("session_xyz"));
        });
    }

    #[test]
    fn set_none_clears_slot_and_reports_none() {
        with_clean_gate(|| {
            let slot: ActiveHandleSlot<TestHandle> = ActiveHandleSlot::new();
            slot.set(Some(handle("cse_abc")));
            assert!(slot.is_set());
            let change = slot.clear();
            assert_eq!(change.new_compat_id, None);
            assert!(!slot.is_set());
            assert!(slot.get().is_none());
            assert_eq!(slot.self_bridge_compat_id(), None);
        });
    }

    #[test]
    fn self_compat_id_respects_gate() {
        with_clean_gate(|| {
            let slot: ActiveHandleSlot<TestHandle> = ActiveHandleSlot::new();
            slot.set(Some(handle("cse_abc")));
            // Shim active by default -> rewrites to session_*
            assert_eq!(slot.self_bridge_compat_id().as_deref(), Some("session_abc"));

            set_cse_shim_gate(Box::new(|| false));
            // Shim disabled -> keeps cse_*
            assert_eq!(slot.self_bridge_compat_id().as_deref(), Some("cse_abc"));
        });
    }

    #[test]
    fn get_returns_arc_clone_so_multiple_callers_observe_handle() {
        with_clean_gate(|| {
            let slot: ActiveHandleSlot<TestHandle> = ActiveHandleSlot::new();
            slot.set(Some(handle("cse_abc")));
            let a = slot.get().expect("handle installed");
            let b = slot.get().expect("handle installed");
            assert_eq!(a.bridge_session_id(), "cse_abc");
            assert_eq!(b.bridge_session_id(), "cse_abc");
            assert!(Arc::ptr_eq(&a, &b));
        });
    }

    #[test]
    fn replacing_the_handle_drops_the_previous_arc() {
        with_clean_gate(|| {
            let slot: ActiveHandleSlot<TestHandle> = ActiveHandleSlot::new();
            let first = handle("cse_first");
            slot.set(Some(Arc::clone(&first)));
            // The slot owns one Arc + the `first` local = 2 strong refs.
            assert_eq!(Arc::strong_count(&first), 2);

            let change = slot.set(Some(handle("cse_second")));
            assert_eq!(change.new_compat_id.as_deref(), Some("session_second"));

            // Previous handle is no longer referenced by the slot —
            // only the `first` local survives.
            assert_eq!(Arc::strong_count(&first), 1);
            assert_eq!(
                slot.get().map(|h| h.bridge_session_id().to_string()),
                Some("cse_second".to_string())
            );
        });
    }

    #[test]
    fn set_none_returns_change_with_none_even_when_slot_was_full() {
        with_clean_gate(|| {
            let slot: ActiveHandleSlot<TestHandle> = ActiveHandleSlot::new();
            slot.set(Some(handle("cse_abc")));
            let change = slot.set(None);
            assert_eq!(change.new_compat_id, None);
        });
    }

    #[test]
    fn non_prefixed_ids_pass_through() {
        with_clean_gate(|| {
            let slot: ActiveHandleSlot<TestHandle> = ActiveHandleSlot::new();
            slot.set(Some(handle("uuid-no-prefix")));
            assert_eq!(
                slot.self_bridge_compat_id().as_deref(),
                Some("uuid-no-prefix")
            );
        });
    }
}
