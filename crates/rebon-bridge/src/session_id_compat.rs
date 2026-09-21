//! Session id tag translation.
//!
//! The worker endpoints (`/v1/code/sessions/{id}/worker/*`) take `cse_*`,
//! which is what a work poll delivers, while the client-facing endpoints
//! (`/v1/sessions/{id}`, `/v1/sessions/{id}/archive`,
//! `/v1/sessions/{id}/events`) take `session_*`. Same UUID, different costume.
//!
//! The shim's kill switch is injected with [`set_cse_shim_gate`], so a caller
//! that already holds an `Fn() -> bool` gate can plug it in without this crate
//! pulling in config or environment handling. A path that never registers a
//! gate gets the default, and the default is that the shim is **active**.

use std::sync::{OnceLock, RwLock};

/// Injected `Fn() -> bool` gate predicate for the `cse_*` → `session_*` shim.
///
/// Kept as `Fn() -> bool` (not `FnMut`) so the gate can live behind a
/// shared pointer and be queried from any thread.
pub type CseShimGate = Box<dyn Fn() -> bool + Send + Sync + 'static>;

fn gate_slot() -> &'static RwLock<Option<CseShimGate>> {
    static GATE: OnceLock<RwLock<Option<CseShimGate>>> = OnceLock::new();
    GATE.get_or_init(|| RwLock::new(None))
}

/// Register the gate for the `cse_*` shim.
///
/// Calling this replaces any previously registered gate. Passing `None`
/// (via [`clear_cse_shim_gate`]) restores the default — shim active.
pub fn set_cse_shim_gate(gate: CseShimGate) {
    let mut guard = gate_slot().write().expect("cse shim gate poisoned");
    *guard = Some(gate);
}

/// Remove any previously registered gate, restoring the default (shim
/// active).
pub fn clear_cse_shim_gate() {
    let mut guard = gate_slot().write().expect("cse shim gate poisoned");
    *guard = None;
}

/// True when the shim is active:
///
/// * no gate registered → `true`
/// * gate registered → gate's return value
fn is_cse_shim_enabled() -> bool {
    let guard = gate_slot().read().expect("cse shim gate poisoned");
    match guard.as_ref() {
        Some(gate) => gate(),
        None => true,
    }
}

/// Re-tag a `cse_*` session ID to `session_*` for use with the v1 compat
/// API. No-op for IDs that are not `cse_*` or when the shim is disabled.
pub fn to_compat_session_id(id: &str) -> String {
    const CSE_PREFIX: &str = "cse_";
    if !id.starts_with(CSE_PREFIX) {
        return id.to_string();
    }
    if !is_cse_shim_enabled() {
        return id.to_string();
    }
    let mut out = String::with_capacity("session_".len() + id.len() - CSE_PREFIX.len());
    out.push_str("session_");
    out.push_str(&id[CSE_PREFIX.len()..]);
    out
}

/// Re-tag a `session_*` session ID to `cse_*` for infrastructure-layer
/// calls. Inverse of [`to_compat_session_id`] — no-op for IDs that are
/// not `session_*`.
///
/// Unlike [`to_compat_session_id`], this is **not** gated by
/// [`is_cse_shim_enabled`]: once the server-side compat flag flips on, the
/// infra layer always needs the `cse_*` costume.
pub fn to_infra_session_id(id: &str) -> String {
    const SESSION_PREFIX: &str = "session_";
    if !id.starts_with(SESSION_PREFIX) {
        return id.to_string();
    }
    let mut out = String::with_capacity("cse_".len() + id.len() - SESSION_PREFIX.len());
    out.push_str("cse_");
    out.push_str(&id[SESSION_PREFIX.len()..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;

    // The shim gate is a process-global, so tests that touch it must run
    // serialized. We use a test-local mutex to enforce that rather than
    // depending on `serial_test`.
    fn gate_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn with_gate_scope<R>(body: impl FnOnce() -> R) -> R {
        let _guard = gate_lock().lock().unwrap_or_else(|p| p.into_inner());
        // Ensure each scope starts clean regardless of prior test state.
        clear_cse_shim_gate();
        let out = body();
        clear_cse_shim_gate();
        out
    }

    #[test]
    fn compat_rewrites_cse_prefix_by_default() {
        with_gate_scope(|| {
            assert_eq!(
                to_compat_session_id("cse_abc-123"),
                "session_abc-123".to_string()
            );
        });
    }

    #[test]
    fn compat_leaves_non_cse_ids_untouched() {
        with_gate_scope(|| {
            assert_eq!(to_compat_session_id("session_xyz"), "session_xyz");
            assert_eq!(to_compat_session_id("raw-uuid-123"), "raw-uuid-123");
            assert_eq!(to_compat_session_id(""), "");
        });
    }

    #[test]
    fn compat_respects_registered_gate_when_disabled() {
        with_gate_scope(|| {
            set_cse_shim_gate(Box::new(|| false));
            assert_eq!(to_compat_session_id("cse_abc"), "cse_abc");
        });
    }

    #[test]
    fn compat_respects_registered_gate_when_enabled() {
        with_gate_scope(|| {
            set_cse_shim_gate(Box::new(|| true));
            assert_eq!(to_compat_session_id("cse_abc"), "session_abc");
        });
    }

    #[test]
    fn compat_re_reads_gate_on_every_call() {
        with_gate_scope(|| {
            let toggle = Arc::new(AtomicBool::new(true));
            let captured = Arc::clone(&toggle);
            set_cse_shim_gate(Box::new(move || captured.load(Ordering::SeqCst)));
            assert_eq!(to_compat_session_id("cse_x"), "session_x");
            toggle.store(false, Ordering::SeqCst);
            assert_eq!(to_compat_session_id("cse_x"), "cse_x");
            toggle.store(true, Ordering::SeqCst);
            assert_eq!(to_compat_session_id("cse_x"), "session_x");
        });
    }

    #[test]
    fn clear_gate_restores_default_active_shim() {
        with_gate_scope(|| {
            set_cse_shim_gate(Box::new(|| false));
            assert_eq!(to_compat_session_id("cse_abc"), "cse_abc");
            clear_cse_shim_gate();
            assert_eq!(to_compat_session_id("cse_abc"), "session_abc");
        });
    }

    #[test]
    fn compat_handles_empty_body_after_prefix() {
        with_gate_scope(|| {
            assert_eq!(to_compat_session_id("cse_"), "session_");
        });
    }

    #[test]
    fn infra_rewrites_session_prefix() {
        with_gate_scope(|| {
            assert_eq!(
                to_infra_session_id("session_abc-123"),
                "cse_abc-123".to_string()
            );
        });
    }

    #[test]
    fn infra_leaves_non_session_ids_untouched() {
        with_gate_scope(|| {
            assert_eq!(to_infra_session_id("cse_abc"), "cse_abc");
            assert_eq!(to_infra_session_id("raw-uuid"), "raw-uuid");
            assert_eq!(to_infra_session_id(""), "");
        });
    }

    #[test]
    fn infra_is_not_gated_by_cse_shim() {
        with_gate_scope(|| {
            set_cse_shim_gate(Box::new(|| false));
            // Even with the compat shim disabled, infra-direction rewrite
            // still happens — `to_infra_session_id` ignores the gate.
            assert_eq!(to_infra_session_id("session_abc"), "cse_abc");
        });
    }

    #[test]
    fn infra_handles_empty_body_after_prefix() {
        with_gate_scope(|| {
            assert_eq!(to_infra_session_id("session_"), "cse_");
        });
    }

    #[test]
    fn roundtrip_compat_then_infra_is_identity_when_shim_active() {
        with_gate_scope(|| {
            let original = "cse_abc-123";
            let compat = to_compat_session_id(original);
            let back = to_infra_session_id(&compat);
            assert_eq!(back, original);
        });
    }
}
