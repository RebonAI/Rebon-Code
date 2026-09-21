//! Updater state machine — pure transitions, side-effect-free.
//!
//! ## Behavior reference
//!
//! The updater does not otherwise have an explicit state machine. Its
//! state is the combination of the [`UpdaterPhase`] and the various
//! version / install-status fields. This module makes that implicit
//! state machine explicit: each observed field becomes a field of
//! [`UpdateState`], and each observably-mutating step becomes a typed
//! [`UpdateEvent`] consumed by [`transition`].
//!
//! ## State diagram
//!
//! ```text
//!                          start: Idle
//!                              │
//!                              │ CheckStarted
//!                              ▼
//!                           Checking
//!                              │
//!                              ├──── CheckSucceededNoUpdate ──► Idle
//!                              │
//!                              ├──── CheckFailed ──► Failed
//!                              │
//!                              ├──── NativeCheckUpToDate /
//!                              │     NativeCheckLockContended ──► Idle
//!                              │
//!                              ├──── NativeCheckUpdated ──► Success
//!                              │
//!                              └──── CheckSucceededUpdateAvailable
//!                                                  │
//!                                                  ▼
//!                                              Installing
//!                                                  │
//!                                                  ├──── InstallSucceeded ──► Success
//!                                                  └──── InstallFailed ──► Failed
//!
//!                          (Success | Failed)
//!                              │
//!                              │ CheckStarted (next poll cycle)
//!                              ▼
//!                           Checking
//! ```
//!
//! Five `UpdaterPhase`s, nine `UpdateEvent`s (the `NpmJs` and the
//! `Native` flow built on the same phases). The transition
//! function returns either a new state or a typed
//! [`TransitionError`] when the caller tries an illegal step
//! (e.g. `CheckSucceededUpdateAvailable` from `Idle` without going
//! through `Checking` first).
//!
//! ## Why this matters
//!
//! Missing state transitions are a recurring bug class in this flow:
//!
//! * Reading a stale in-progress flag at the 30-minute poll boundary
//!   can trigger a double-install.
//! * Failing to clear the in-progress flag when a check/install
//!   throws leaves the updater stuck "in progress".
//!
//! Both of these are state-machine bugs. The Rust implementation forces the
//! caller to go through the typed transitions, so an equivalent bug
//! would show up at compile time as a missing match arm or as a
//! `TransitionError::IllegalEvent` at runtime.
//!
//! ## Caller-supplied effects
//!
//! Like `rebon-tui::state`'s cancel reducer (see that file's
//! "Cancel-commit semantics" doc block), this reducer is **pure**:
//! it never reads the clock or generates ids itself. The caller passes
//! timestamps in via the events.

use crate::installation_method::InstallMethod;

/// The install status, with wire strings
/// `"success" | "no_permissions" | "install_failed" | "in_progress"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstallStatus {
    Success,
    NoPermissions,
    InstallFailed,
    InProgress,
}

impl InstallStatus {
    /// The wire string for this status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::NoPermissions => "no_permissions",
            Self::InstallFailed => "install_failed",
            Self::InProgress => "in_progress",
        }
    }
}

/// One phase of the updater. The state machine moves between
/// these in response to [`UpdateEvent`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpdaterPhase {
    /// Default phase. Nothing in flight; no result to display.
    Idle,
    /// A check is in flight (network fetch + max-version
    /// check). Nothing is rendered during this phase if
    /// versions haven't been resolved yet.
    Checking,
    /// An install is in flight.
    /// The "Auto-updating…" indicator is shown.
    Installing,
    /// The most recent install attempt succeeded. The
    /// success banner is shown.
    Success,
    /// The most recent install attempt failed. The
    /// error banner is shown.
    Failed,
}

/// Which concrete updater flow is currently in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpdateFlow {
    /// The npm updater.
    NpmJs,
    /// The native updater.
    Native,
}

/// Full state of the updater at one point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateState {
    pub phase: UpdaterPhase,
    /// Which updater flow is currently in flight, if any.
    pub current_flow: Option<UpdateFlow>,
    /// Start timestamp for the in-flight check/install cycle, if
    /// any.
    pub started_at_ms: Option<u64>,
    /// Pending npm install method for the current run.
    pub pending_install_method: Option<InstallMethod>,
    /// The locally-installed version, if known.
    pub current_version: Option<String>,
    /// The latest version returned by the most recent check, if
    /// any.
    pub latest_version: Option<String>,
    /// The status of the most recent install attempt, if any.
    pub last_install_status: Option<InstallStatus>,
    /// The version that was being installed when the most recent
    /// install attempt completed (success or failure).
    pub last_install_target: Option<String>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            phase: UpdaterPhase::Idle,
            current_flow: None,
            started_at_ms: None,
            pending_install_method: None,
            current_version: None,
            latest_version: None,
            last_install_status: None,
            last_install_target: None,
        }
    }
}

impl UpdateState {
    /// New state in `Idle` phase with no known versions.
    pub fn new() -> Self {
        Self::default()
    }
}

/// One transition input — a typed step of the updater flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    /// A check has started. Carries both the updater flow and the
    /// exact start timestamp.
    CheckStarted {
        flow: UpdateFlow,
        started_at_ms: u64,
    },
    /// The `NpmJs` flow: the check completed and there is no update
    /// to install. Returns to Idle, but updates the known version
    /// pair.
    CheckSucceededNoUpdate {
        current_version: String,
        latest_version: String,
    },
    /// The `NpmJs` flow: the check found an update and selected the
    /// install method. Moves to Installing.
    CheckSucceededUpdateAvailable {
        current_version: String,
        latest_version: String,
        method: InstallMethod,
    },
    /// The `Native` flow: the update check finished with no update
    /// applied because the binary was already current.
    NativeCheckUpToDate {
        current_version: String,
        latest_version: String,
        finished_at_ms: u64,
    },
    /// The `Native` flow: the check applied an
    /// update successfully.
    NativeCheckUpdated {
        current_version: String,
        latest_version: String,
        finished_at_ms: u64,
    },
    /// The `Native` flow: an updater lock was detected and the check
    /// exited early without changing versions.
    NativeCheckLockContended { finished_at_ms: u64 },
    /// The `Native` flow: the check/update path threw.
    CheckFailed {
        finished_at_ms: u64,
        error_message: String,
    },
    /// The `NpmJs` flow: the install attempt completed successfully.
    InstallSucceeded { finished_at_ms: u64 },
    /// The `NpmJs` flow: the install attempt failed with a terminal
    /// status.
    InstallFailed {
        status: InstallStatus,
        finished_at_ms: u64,
    },
}

/// Reason a transition was rejected. The reducer returns this in
/// `Result::Err` so the caller can decide whether to log/ignore the
/// stale event or panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    /// The current phase doesn't allow this event. Carries the
    /// observed phase + event-name for diagnostics.
    IllegalEvent {
        phase: UpdaterPhase,
        event_name: &'static str,
    },
}

/// Output of one transition: the new state after the event was
/// applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionOutcome {
    pub state: UpdateState,
}

fn clear_in_flight(next: &mut UpdateState) {
    next.current_flow = None;
    next.started_at_ms = None;
    next.pending_install_method = None;
}

fn is_flow(state: &UpdateState, flow: UpdateFlow) -> bool {
    state.current_flow == Some(flow)
}

/// Reduce one event over the current state. Returns either a new
/// state or a typed
/// [`TransitionError`] if the event is illegal in the current
/// phase.
///
/// Pure function: no global reads, no clock reads, no I/O.
pub fn transition(
    state: &UpdateState,
    event: UpdateEvent,
) -> Result<TransitionOutcome, TransitionError> {
    use UpdateEvent::*;
    use UpdaterPhase::*;

    let mut next = state.clone();

    match (state.phase, event) {
        (
            Idle | Success | Failed,
            CheckStarted {
                flow,
                started_at_ms,
            },
        ) => {
            next.phase = Checking;
            next.current_flow = Some(flow);
            next.started_at_ms = Some(started_at_ms);
            next.pending_install_method = None;
        }

        (Checking | Installing, CheckStarted { .. }) => {
            return Err(TransitionError::IllegalEvent {
                phase: state.phase,
                event_name: "CheckStarted",
            });
        }

        (
            Checking,
            CheckSucceededNoUpdate {
                current_version,
                latest_version,
            },
        ) if is_flow(state, UpdateFlow::NpmJs) => {
            next.current_version = Some(current_version);
            next.latest_version = Some(latest_version);
            next.phase = Idle;
            clear_in_flight(&mut next);
        }

        (_, CheckSucceededNoUpdate { .. }) => {
            return Err(TransitionError::IllegalEvent {
                phase: state.phase,
                event_name: "CheckSucceededNoUpdate",
            });
        }

        (
            Checking,
            CheckSucceededUpdateAvailable {
                current_version,
                latest_version,
                method,
            },
        ) if is_flow(state, UpdateFlow::NpmJs) => {
            next.current_version = Some(current_version);
            next.latest_version = Some(latest_version);
            next.pending_install_method = Some(method);
            next.phase = Installing;
        }

        (_, CheckSucceededUpdateAvailable { .. }) => {
            return Err(TransitionError::IllegalEvent {
                phase: state.phase,
                event_name: "CheckSucceededUpdateAvailable",
            });
        }

        (
            Checking,
            NativeCheckUpToDate {
                current_version,
                latest_version,
                ..
            },
        ) if is_flow(state, UpdateFlow::Native) => {
            next.current_version = Some(current_version);
            next.latest_version = Some(latest_version);
            next.phase = Idle;
            clear_in_flight(&mut next);
        }

        (_, NativeCheckUpToDate { .. }) => {
            return Err(TransitionError::IllegalEvent {
                phase: state.phase,
                event_name: "NativeCheckUpToDate",
            });
        }

        (
            Checking,
            NativeCheckUpdated {
                current_version,
                latest_version,
                ..
            },
        ) if is_flow(state, UpdateFlow::Native) => {
            next.current_version = Some(current_version);
            next.latest_version = Some(latest_version.clone());
            next.last_install_status = Some(InstallStatus::Success);
            next.last_install_target = Some(latest_version);
            next.phase = Success;
            clear_in_flight(&mut next);
        }

        (_, NativeCheckUpdated { .. }) => {
            return Err(TransitionError::IllegalEvent {
                phase: state.phase,
                event_name: "NativeCheckUpdated",
            });
        }

        (Checking, NativeCheckLockContended { .. }) if is_flow(state, UpdateFlow::Native) => {
            next.phase = Idle;
            clear_in_flight(&mut next);
        }

        (_, NativeCheckLockContended { .. }) => {
            return Err(TransitionError::IllegalEvent {
                phase: state.phase,
                event_name: "NativeCheckLockContended",
            });
        }

        (Checking, CheckFailed { .. }) if is_flow(state, UpdateFlow::Native) => {
            next.last_install_status = Some(InstallStatus::InstallFailed);
            next.last_install_target = None;
            next.phase = Failed;
            clear_in_flight(&mut next);
        }

        (_, CheckFailed { .. }) => {
            return Err(TransitionError::IllegalEvent {
                phase: state.phase,
                event_name: "CheckFailed",
            });
        }

        (Installing, InstallSucceeded { .. }) if is_flow(state, UpdateFlow::NpmJs) => {
            next.last_install_status = Some(InstallStatus::Success);
            next.last_install_target = state.latest_version.clone();
            next.phase = Success;
            clear_in_flight(&mut next);
        }

        (_, InstallSucceeded { .. }) => {
            return Err(TransitionError::IllegalEvent {
                phase: state.phase,
                event_name: "InstallSucceeded",
            });
        }

        (Installing, InstallFailed { status, .. }) if is_flow(state, UpdateFlow::NpmJs) => {
            next.last_install_status = Some(status);
            next.last_install_target = state.latest_version.clone();
            next.phase = Failed;
            clear_in_flight(&mut next);
        }

        (_, InstallFailed { .. }) => {
            return Err(TransitionError::IllegalEvent {
                phase: state.phase,
                event_name: "InstallFailed",
            });
        }
    }

    Ok(TransitionOutcome { state: next })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> UpdateState {
        UpdateState::new()
    }

    fn checking_npm(started_at_ms: u64) -> UpdateState {
        UpdateState {
            phase: UpdaterPhase::Checking,
            current_flow: Some(UpdateFlow::NpmJs),
            started_at_ms: Some(started_at_ms),
            ..Default::default()
        }
    }

    fn checking_native(started_at_ms: u64) -> UpdateState {
        UpdateState {
            phase: UpdaterPhase::Checking,
            current_flow: Some(UpdateFlow::Native),
            started_at_ms: Some(started_at_ms),
            ..Default::default()
        }
    }

    fn installing_npm(
        current: &str,
        latest: &str,
        started_at_ms: u64,
        method: InstallMethod,
    ) -> UpdateState {
        UpdateState {
            phase: UpdaterPhase::Installing,
            current_flow: Some(UpdateFlow::NpmJs),
            started_at_ms: Some(started_at_ms),
            pending_install_method: Some(method),
            current_version: Some(current.into()),
            latest_version: Some(latest.into()),
            ..Default::default()
        }
    }

    fn success() -> UpdateState {
        UpdateState {
            phase: UpdaterPhase::Success,
            ..Default::default()
        }
    }

    fn failed() -> UpdateState {
        UpdateState {
            phase: UpdaterPhase::Failed,
            ..Default::default()
        }
    }

    #[test]
    fn npm_full_happy_path_idle_to_success() {
        let s0 = idle();
        let s1 = transition(
            &s0,
            UpdateEvent::CheckStarted {
                flow: UpdateFlow::NpmJs,
                started_at_ms: 1_000,
            },
        )
        .unwrap();
        assert_eq!(s1.state.phase, UpdaterPhase::Checking);
        assert_eq!(s1.state.current_flow, Some(UpdateFlow::NpmJs));
        assert_eq!(s1.state.started_at_ms, Some(1_000));

        let s2 = transition(
            &s1.state,
            UpdateEvent::CheckSucceededUpdateAvailable {
                current_version: "1.0.0".into(),
                latest_version: "2.0.0".into(),
                method: InstallMethod::Global,
            },
        )
        .unwrap();
        assert_eq!(s2.state.phase, UpdaterPhase::Installing);
        assert_eq!(s2.state.current_version.as_deref(), Some("1.0.0"));
        assert_eq!(s2.state.latest_version.as_deref(), Some("2.0.0"));
        assert_eq!(s2.state.pending_install_method, Some(InstallMethod::Global));

        let s3 = transition(
            &s2.state,
            UpdateEvent::InstallSucceeded {
                finished_at_ms: 6_000,
            },
        )
        .unwrap();
        assert_eq!(s3.state.phase, UpdaterPhase::Success);
        assert_eq!(s3.state.current_flow, None);
        assert_eq!(s3.state.started_at_ms, None);
        assert_eq!(s3.state.pending_install_method, None);
        assert_eq!(s3.state.last_install_status, Some(InstallStatus::Success));
        assert_eq!(s3.state.last_install_target.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn npm_check_succeeded_no_update_returns_to_idle_and_clears_inflight() {
        let s0 = checking_npm(1_000);
        let result = transition(
            &s0,
            UpdateEvent::CheckSucceededNoUpdate {
                current_version: "1.0.0".into(),
                latest_version: "1.0.0".into(),
            },
        )
        .unwrap();
        assert_eq!(result.state.phase, UpdaterPhase::Idle);
        assert_eq!(result.state.current_flow, None);
        assert_eq!(result.state.started_at_ms, None);
        assert_eq!(result.state.pending_install_method, None);
        assert_eq!(result.state.current_version.as_deref(), Some("1.0.0"));
        assert_eq!(result.state.latest_version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn npm_install_failed_records_failure() {
        let s0 = installing_npm("1.0.0", "2.0.0", 1_000, InstallMethod::Local);
        let result = transition(
            &s0,
            UpdateEvent::InstallFailed {
                status: InstallStatus::NoPermissions,
                finished_at_ms: 4_500,
            },
        )
        .unwrap();
        assert_eq!(result.state.phase, UpdaterPhase::Failed);
        assert_eq!(result.state.current_flow, None);
        assert_eq!(
            result.state.last_install_status,
            Some(InstallStatus::NoPermissions)
        );
        assert_eq!(result.state.last_install_target.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn native_start_transitions_to_checking() {
        let result = transition(
            &idle(),
            UpdateEvent::CheckStarted {
                flow: UpdateFlow::Native,
                started_at_ms: 100,
            },
        )
        .unwrap();
        assert_eq!(result.state.phase, UpdaterPhase::Checking);
        assert_eq!(result.state.current_flow, Some(UpdateFlow::Native));
        assert_eq!(result.state.started_at_ms, Some(100));
    }

    #[test]
    fn native_up_to_date_returns_to_idle_and_records_versions() {
        let s0 = checking_native(1_000);
        let result = transition(
            &s0,
            UpdateEvent::NativeCheckUpToDate {
                current_version: "1.0.0".into(),
                latest_version: "1.0.0".into(),
                finished_at_ms: 3_500,
            },
        )
        .unwrap();
        assert_eq!(result.state.phase, UpdaterPhase::Idle);
        assert_eq!(result.state.current_flow, None);
        assert_eq!(result.state.current_version.as_deref(), Some("1.0.0"));
        assert_eq!(result.state.latest_version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn native_updated_records_success_and_target() {
        let s0 = checking_native(1_000);
        let result = transition(
            &s0,
            UpdateEvent::NativeCheckUpdated {
                current_version: "1.0.0".into(),
                latest_version: "2.0.0".into(),
                finished_at_ms: 2_600,
            },
        )
        .unwrap();
        assert_eq!(result.state.phase, UpdaterPhase::Success);
        assert_eq!(
            result.state.last_install_status,
            Some(InstallStatus::Success)
        );
        assert_eq!(result.state.last_install_target.as_deref(), Some("2.0.0"));
        assert_eq!(result.state.current_flow, None);
    }

    #[test]
    fn native_lock_contention_returns_to_idle() {
        let s0 = checking_native(400);
        let result = transition(
            &s0,
            UpdateEvent::NativeCheckLockContended {
                finished_at_ms: 900,
            },
        )
        .unwrap();
        assert_eq!(result.state.phase, UpdaterPhase::Idle);
        assert_eq!(result.state.current_flow, None);
    }

    #[test]
    fn native_check_failed_records_failure() {
        let s0 = checking_native(100);
        let result = transition(
            &s0,
            UpdateEvent::CheckFailed {
                finished_at_ms: 700,
                error_message: "ECONNREFUSED: server down".into(),
            },
        )
        .unwrap();
        assert_eq!(result.state.phase, UpdaterPhase::Failed);
        assert_eq!(result.state.current_flow, None);
        assert_eq!(
            result.state.last_install_status,
            Some(InstallStatus::InstallFailed)
        );
    }

    #[test]
    fn check_started_from_success_is_legal_for_both_flows() {
        let npm = transition(
            &success(),
            UpdateEvent::CheckStarted {
                flow: UpdateFlow::NpmJs,
                started_at_ms: 0,
            },
        )
        .unwrap();
        assert_eq!(npm.state.phase, UpdaterPhase::Checking);

        let native = transition(
            &success(),
            UpdateEvent::CheckStarted {
                flow: UpdateFlow::Native,
                started_at_ms: 0,
            },
        )
        .unwrap();
        assert_eq!(native.state.phase, UpdaterPhase::Checking);
    }

    #[test]
    fn check_started_from_failed_is_legal_for_both_flows() {
        let npm = transition(
            &failed(),
            UpdateEvent::CheckStarted {
                flow: UpdateFlow::NpmJs,
                started_at_ms: 0,
            },
        )
        .unwrap();
        assert_eq!(npm.state.phase, UpdaterPhase::Checking);

        let native = transition(
            &failed(),
            UpdateEvent::CheckStarted {
                flow: UpdateFlow::Native,
                started_at_ms: 0,
            },
        )
        .unwrap();
        assert_eq!(native.state.phase, UpdaterPhase::Checking);
    }

    #[test]
    fn check_started_during_checking_is_illegal() {
        let err = transition(
            &checking_npm(0),
            UpdateEvent::CheckStarted {
                flow: UpdateFlow::NpmJs,
                started_at_ms: 1,
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            TransitionError::IllegalEvent {
                phase: UpdaterPhase::Checking,
                event_name: "CheckStarted",
            }
        );
    }

    #[test]
    fn native_result_during_npm_checking_is_illegal() {
        let err = transition(
            &checking_npm(0),
            UpdateEvent::NativeCheckUpdated {
                current_version: "1.0.0".into(),
                latest_version: "2.0.0".into(),
                finished_at_ms: 10,
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            TransitionError::IllegalEvent {
                phase: UpdaterPhase::Checking,
                event_name: "NativeCheckUpdated",
            }
        );
    }

    #[test]
    fn npm_result_during_native_checking_is_illegal() {
        let err = transition(
            &checking_native(0),
            UpdateEvent::CheckSucceededUpdateAvailable {
                current_version: "1.0.0".into(),
                latest_version: "2.0.0".into(),
                method: InstallMethod::Global,
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            TransitionError::IllegalEvent {
                phase: UpdaterPhase::Checking,
                event_name: "CheckSucceededUpdateAvailable",
            }
        );
    }

    #[test]
    fn install_succeeded_from_native_checking_is_illegal() {
        let err = transition(
            &checking_native(0),
            UpdateEvent::InstallSucceeded { finished_at_ms: 10 },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            TransitionError::IllegalEvent {
                phase: UpdaterPhase::Checking,
                event_name: "InstallSucceeded"
            }
        ));
    }

    #[test]
    fn install_failed_from_idle_is_illegal() {
        let err = transition(
            &idle(),
            UpdateEvent::InstallFailed {
                status: InstallStatus::InstallFailed,
                finished_at_ms: 0,
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            TransitionError::IllegalEvent {
                phase: UpdaterPhase::Idle,
                event_name: "InstallFailed"
            }
        ));
    }

    #[test]
    fn install_status_wire_strings_are_pinned() {
        assert_eq!(InstallStatus::Success.as_str(), "success");
        assert_eq!(InstallStatus::NoPermissions.as_str(), "no_permissions");
        assert_eq!(InstallStatus::InstallFailed.as_str(), "install_failed");
        assert_eq!(InstallStatus::InProgress.as_str(), "in_progress");
    }

    #[test]
    fn phase_event_legality_table() {
        fn state_for(name: &str) -> UpdateState {
            match name {
                "Idle" => idle(),
                "CheckingNpm" => checking_npm(0),
                "CheckingNative" => checking_native(0),
                "InstallingNpm" => installing_npm("1.0.0", "2.0.0", 0, InstallMethod::Global),
                "Success" => success(),
                "Failed" => failed(),
                _ => panic!("unknown state {name}"),
            }
        }

        fn dummy_event(name: &str) -> UpdateEvent {
            match name {
                "CheckStartedNpm" => UpdateEvent::CheckStarted {
                    flow: UpdateFlow::NpmJs,
                    started_at_ms: 0,
                },
                "CheckStartedNative" => UpdateEvent::CheckStarted {
                    flow: UpdateFlow::Native,
                    started_at_ms: 0,
                },
                "CheckSucceededNoUpdate" => UpdateEvent::CheckSucceededNoUpdate {
                    current_version: "1.0.0".into(),
                    latest_version: "1.0.0".into(),
                },
                "CheckSucceededUpdateAvailable" => UpdateEvent::CheckSucceededUpdateAvailable {
                    current_version: "1.0.0".into(),
                    latest_version: "2.0.0".into(),
                    method: InstallMethod::Global,
                },
                "NativeCheckUpToDate" => UpdateEvent::NativeCheckUpToDate {
                    current_version: "1.0.0".into(),
                    latest_version: "1.0.0".into(),
                    finished_at_ms: 0,
                },
                "NativeCheckUpdated" => UpdateEvent::NativeCheckUpdated {
                    current_version: "1.0.0".into(),
                    latest_version: "2.0.0".into(),
                    finished_at_ms: 0,
                },
                "NativeCheckLockContended" => {
                    UpdateEvent::NativeCheckLockContended { finished_at_ms: 0 }
                }
                "CheckFailed" => UpdateEvent::CheckFailed {
                    finished_at_ms: 0,
                    error_message: "x".into(),
                },
                "InstallSucceeded" => UpdateEvent::InstallSucceeded { finished_at_ms: 0 },
                "InstallFailed" => UpdateEvent::InstallFailed {
                    status: InstallStatus::InstallFailed,
                    finished_at_ms: 0,
                },
                _ => panic!("unknown event {name}"),
            }
        }

        let legality: &[(&str, &str, bool)] = &[
            ("Idle", "CheckStartedNpm", true),
            ("Idle", "CheckStartedNative", true),
            ("Idle", "CheckSucceededNoUpdate", false),
            ("Idle", "CheckSucceededUpdateAvailable", false),
            ("Idle", "NativeCheckUpToDate", false),
            ("Idle", "NativeCheckUpdated", false),
            ("Idle", "NativeCheckLockContended", false),
            ("Idle", "CheckFailed", false),
            ("Idle", "InstallSucceeded", false),
            ("Idle", "InstallFailed", false),
            ("CheckingNpm", "CheckStartedNpm", false),
            ("CheckingNpm", "CheckStartedNative", false),
            ("CheckingNpm", "CheckSucceededNoUpdate", true),
            ("CheckingNpm", "CheckSucceededUpdateAvailable", true),
            ("CheckingNpm", "NativeCheckUpToDate", false),
            ("CheckingNpm", "NativeCheckUpdated", false),
            ("CheckingNpm", "NativeCheckLockContended", false),
            ("CheckingNpm", "CheckFailed", false),
            ("CheckingNpm", "InstallSucceeded", false),
            ("CheckingNpm", "InstallFailed", false),
            ("CheckingNative", "CheckStartedNpm", false),
            ("CheckingNative", "CheckStartedNative", false),
            ("CheckingNative", "CheckSucceededNoUpdate", false),
            ("CheckingNative", "CheckSucceededUpdateAvailable", false),
            ("CheckingNative", "NativeCheckUpToDate", true),
            ("CheckingNative", "NativeCheckUpdated", true),
            ("CheckingNative", "NativeCheckLockContended", true),
            ("CheckingNative", "CheckFailed", true),
            ("CheckingNative", "InstallSucceeded", false),
            ("CheckingNative", "InstallFailed", false),
            ("InstallingNpm", "CheckStartedNpm", false),
            ("InstallingNpm", "CheckStartedNative", false),
            ("InstallingNpm", "CheckSucceededNoUpdate", false),
            ("InstallingNpm", "CheckSucceededUpdateAvailable", false),
            ("InstallingNpm", "NativeCheckUpToDate", false),
            ("InstallingNpm", "NativeCheckUpdated", false),
            ("InstallingNpm", "NativeCheckLockContended", false),
            ("InstallingNpm", "CheckFailed", false),
            ("InstallingNpm", "InstallSucceeded", true),
            ("InstallingNpm", "InstallFailed", true),
            ("Success", "CheckStartedNpm", true),
            ("Success", "CheckStartedNative", true),
            ("Success", "CheckSucceededNoUpdate", false),
            ("Success", "CheckSucceededUpdateAvailable", false),
            ("Success", "NativeCheckUpToDate", false),
            ("Success", "NativeCheckUpdated", false),
            ("Success", "NativeCheckLockContended", false),
            ("Success", "CheckFailed", false),
            ("Success", "InstallSucceeded", false),
            ("Success", "InstallFailed", false),
            ("Failed", "CheckStartedNpm", true),
            ("Failed", "CheckStartedNative", true),
            ("Failed", "CheckSucceededNoUpdate", false),
            ("Failed", "CheckSucceededUpdateAvailable", false),
            ("Failed", "NativeCheckUpToDate", false),
            ("Failed", "NativeCheckUpdated", false),
            ("Failed", "NativeCheckLockContended", false),
            ("Failed", "CheckFailed", false),
            ("Failed", "InstallSucceeded", false),
            ("Failed", "InstallFailed", false),
        ];

        for (state_name, event_name, expect_legal) in legality {
            let state = state_for(state_name);
            let result = transition(&state, dummy_event(event_name));
            if *expect_legal {
                assert!(
                    result.is_ok(),
                    "expected ({state_name}, {event_name}) to be legal, got {result:?}",
                );
            } else {
                assert!(
                    matches!(result, Err(TransitionError::IllegalEvent { .. })),
                    "expected ({state_name}, {event_name}) to be illegal, got {result:?}",
                );
            }
        }
    }
}
