//! Seat conformance test templates.
//!
//! Every seat ships its conformance tests built from these helpers — they are
//! part of the seat's deliverable, not optional hygiene. The templates cover
//! the three contract families:
//!
//! - **Disposability** ([`assert_mount_leaves_no_residue`]): mount →
//!   enumerate → dispose → every registry back to its pre-mount state.
//! - **Arbitration** ([`assert_seat_arbitration`]): the three-state
//!   execution-time selection contract of an arbitrated single-slot seat.
//! - **Fail-closed** ([`assert_json_calls_fail_closed`]): unauthorized or
//!   unconfigured probes must error, never silently succeed or degrade.
//!
//! The helpers panic with actionable messages, so call them from `#[test]`
//! functions.

use std::sync::Arc;

use crate::seat::{SeatError, SeatRegistry};
use crate::{Context, JsonService};

/// Disposability contract: `mount` registers through the probe fork it is
/// given; afterwards the fork must hold visible registrations, and once the
/// fork disposes, the kernel event tables, the service namespace, and the
/// seat's own registry (`seat_count` — pass `|| 0` when the seat keeps no
/// table of its own) must all be back to their pre-mount state.
pub fn assert_mount_leaves_no_residue(
    root: &Context,
    seat_count: impl Fn() -> usize,
    mount: impl FnOnce(&Context),
) {
    let baseline_events = root.event_stats();
    let baseline_services = root.service_names();
    let baseline_seat = seat_count();

    let probe = root.fork("conformance-probe");
    mount(&probe);
    assert!(
        !probe.registration_labels().is_empty(),
        "mount registered nothing through the probe fork — a seat whose \
         registrations bypass the Context primitive cannot prove disposal"
    );

    probe.dispose();
    assert_eq!(
        root.event_stats(),
        baseline_events,
        "event-bus tables must return to their pre-mount state"
    );
    assert_eq!(
        root.service_names(),
        baseline_services,
        "service namespace must return to its pre-mount state"
    );
    assert_eq!(
        seat_count(),
        baseline_seat,
        "the seat's own registry must return to its pre-mount count"
    );
    assert!(
        probe.registration_labels().is_empty(),
        "the probe fork's scope must be drained"
    );
}

/// Arbitration contract for an arbitrated single-slot seat built on
/// [`SeatRegistry`]: zero → Unavailable, sole → auto-select, many →
/// Ambiguous, configured wins, unknown configured id fails loudly, and
/// disposal restores the previous arbitration state. `make` mints distinct
/// providers by index.
pub fn assert_seat_arbitration<P: Clone + Send + Sync + 'static>(make: impl Fn(usize) -> P) {
    let registry: Arc<SeatRegistry<P>> = SeatRegistry::new("arbitration-conformance");

    assert!(
        matches!(registry.resolve(None), Err(SeatError::Unavailable { .. })),
        "zero providers must resolve Unavailable"
    );

    let d_first = registry
        .register("first", make(1))
        .expect("first registers");
    let (sole, _) = registry.resolve(None).expect("sole provider auto-selects");
    assert_eq!(sole, "first");

    let _d_second = registry
        .register("second", make(2))
        .expect("second registers");
    match registry.resolve(None) {
        Err(SeatError::Ambiguous { candidates, .. }) => {
            assert_eq!(
                candidates,
                vec!["first".to_string(), "second".to_string()],
                "Ambiguous must name every candidate"
            );
        }
        Ok((id, _)) => panic!("two unconfigured providers must be Ambiguous, got Ok({id})"),
        Err(other) => panic!("two unconfigured providers must be Ambiguous, got {other}"),
    }

    let (configured, _) = registry
        .resolve(Some("second"))
        .map_err(|err| err.to_string())
        .expect("configured id wins over ambiguity");
    assert_eq!(configured, "second");

    match registry.resolve(Some("absent")) {
        Err(SeatError::UnknownProvider { known, .. }) => {
            assert_eq!(known, vec!["first".to_string(), "second".to_string()]);
        }
        Ok((id, _)) => panic!("unknown configured id must fail loudly, got Ok({id})"),
        Err(other) => panic!("unknown configured id must fail loudly, got {other}"),
    }

    d_first.dispose();
    let (after, _) = registry
        .resolve(None)
        .expect("disposal must restore sole-provider auto-selection");
    assert_eq!(after, "second");
}

/// Fail-closed contract: every probe must error, and each error must carry
/// `marker` (the seat's stable denial code or phrase). A probe that
/// succeeds — or fails with an unrelated message — is a hole in the seat's
/// closed default.
pub fn assert_json_calls_fail_closed(
    service: &dyn JsonService,
    probes: &[(&str, serde_json::Value)],
    marker: &str,
) {
    assert!(
        !probes.is_empty(),
        "fail-closed template needs at least one probe"
    );
    for (method, params) in probes {
        match service.call(method, params.clone()) {
            Ok(value) => panic!("probe `{method}` must fail closed but succeeded with {value}"),
            Err(err) => {
                let text = err.to_string();
                assert!(
                    text.contains(marker),
                    "probe `{method}` failed, but without the stable marker \
                     {marker:?}: {text}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KernelError;

    #[test]
    fn arbitration_template_passes_on_the_reference_registry() {
        assert_seat_arbitration(|n| n as u32);
    }

    #[test]
    fn no_residue_template_passes_on_a_conforming_mount() {
        let root = Context::root();
        let registry: Arc<SeatRegistry<u32>> = SeatRegistry::new("probe-seat");
        let seat = registry.clone();
        assert_mount_leaves_no_residue(
            &root,
            move || seat.len(),
            |ctx| {
                ctx.on_json("probe/evt", |_| {});
                let registry = registry.clone();
                ctx.effect_labeled("probe provider(x)", || {
                    registry.register("x", 7u32).expect("registers")
                });
            },
        );
    }

    #[test]
    #[should_panic(expected = "registered nothing")]
    fn no_residue_template_rejects_a_bypassing_mount() {
        let root = Context::root();
        // A "seat" that registers nothing through the fork it was given
        // (e.g. writes some global table directly) must be caught.
        assert_mount_leaves_no_residue(&root, || 0, |_ctx| {});
    }

    #[test]
    fn fail_closed_template_accepts_denials_only() {
        struct Denying;
        impl JsonService for Denying {
            fn call(
                &self,
                _method: &str,
                _params: serde_json::Value,
            ) -> Result<serde_json::Value, KernelError> {
                Err(KernelError::Other("credential not granted".into()))
            }
        }
        assert_json_calls_fail_closed(
            &Denying,
            &[("resolveEnv", serde_json::json!({"ref": "X"}))],
            "not granted",
        );
    }

    #[test]
    #[should_panic(expected = "must fail closed")]
    fn fail_closed_template_rejects_a_leaky_service() {
        struct Leaky;
        impl JsonService for Leaky {
            fn call(
                &self,
                _method: &str,
                _params: serde_json::Value,
            ) -> Result<serde_json::Value, KernelError> {
                Ok(serde_json::json!("leaked"))
            }
        }
        assert_json_calls_fail_closed(&Leaky, &[("get", serde_json::Value::Null)], "denied");
    }
}
