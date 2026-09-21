//! Reconnect helpers and state machine.
//!
//! This module provides:
//! * The two pure formatters
//!   [`handle_reconnect_result`] and [`handle_reconnect_error`].
//! * The Reconnect dialog state machine (idle → reconnecting → result).
//!
//! The helper branches on [`ReconnectClientKind`]:
//!
//! * `Connected` → `"Reconnected to {name}."` (success)
//! * `NeedsAuth` → `"{name} requires authentication. Use the 'Authenticate' option."` (failure)
//! * `Failed` → `"Failed to reconnect to {name}."` (failure)
//! * `Other` → `"Unknown result when reconnecting to {name}."` (failure)
//!
//! [`handle_reconnect_error`] wraps a `&str` error message in
//! `"Error reconnecting to {name}: {msg}"`.

/// A reconnect result message + success flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectResult {
    /// The user-facing message to display in a banner / toast.
    pub message: String,
    /// Whether the reconnect attempt ended in the connected state.
    pub success: bool,
}

/// The reconnect-result variants that the reconnect helper
/// branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectClientKind {
    Connected,
    NeedsAuth,
    Failed,
    /// Any other variant (`pending`, `disabled`, or a future type)
    /// falls through to the default branch.
    Other,
}

/// Format a reconnect-attempt result for a given server name.
///
/// Returns the exact user-facing strings, including the punctuation
/// (trailing `.`, quoted `'Authenticate'`, etc.).
pub fn handle_reconnect_result(kind: ReconnectClientKind, server_name: &str) -> ReconnectResult {
    match kind {
        ReconnectClientKind::Connected => ReconnectResult {
            message: format!("Reconnected to {server_name}."),
            success: true,
        },
        ReconnectClientKind::NeedsAuth => ReconnectResult {
            message: format!(
                "{server_name} requires authentication. Use the 'Authenticate' option."
            ),
            success: false,
        },
        ReconnectClientKind::Failed => ReconnectResult {
            message: format!("Failed to reconnect to {server_name}."),
            success: false,
        },
        ReconnectClientKind::Other => ReconnectResult {
            message: format!("Unknown result when reconnecting to {server_name}."),
            success: false,
        },
    }
}

/// Wrap a reconnect-attempt error into a user-facing string.
///
/// Takes a `&str` error message (the consumer converts its own
/// error type to a string first) and wraps it in the banner text.
pub fn handle_reconnect_error(error_message: &str, server_name: &str) -> String {
    format!("Error reconnecting to {server_name}: {error_message}")
}

// ---------------------------------------------------------------------------
// Reconnect state machine
// ---------------------------------------------------------------------------

/// High-level state of the reconnect dialog.
///
/// The dialog opens Idle, transitions to
/// `Reconnecting` on Enter, then to `Success` or `Error` once the
/// async attempt resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconnectState {
    /// The dialog is open and waiting for the user to press Enter.
    Idle,
    /// The dialog has dispatched the reconnect attempt and is waiting
    /// for the result.
    Reconnecting,
    /// The reconnect attempt has resolved.
    Resolved {
        /// The flattened user-facing result.
        result: ReconnectResult,
    },
}

impl Default for ReconnectState {
    fn default() -> Self {
        Self::Idle
    }
}

/// Events that drive [`ReconnectState`] transitions: the user
/// confirms (`StartReconnect`), the async attempt resolves or errors
/// (`AttemptResolved` / `AttemptErrored`), or the user dismisses
/// (`Dismiss`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconnectEvent {
    /// The user pressed Enter / Confirm in the idle dialog.
    StartReconnect,
    /// The async attempt resolved with a typed client kind.
    AttemptResolved {
        kind: ReconnectClientKind,
        server_name: String,
    },
    /// The async attempt threw an error.
    AttemptErrored {
        error_message: String,
        server_name: String,
    },
    /// The user dismissed the dialog (closes the UI — caller handles).
    Dismiss,
}

impl ReconnectState {
    /// Apply an event and return the next state. Unknown-sequence
    /// events are no-ops; each transition checks the current state.
    pub fn apply(self, event: ReconnectEvent) -> Self {
        match (self, event) {
            (Self::Idle, ReconnectEvent::StartReconnect) => Self::Reconnecting,
            (Self::Reconnecting, ReconnectEvent::AttemptResolved { kind, server_name }) => {
                Self::Resolved {
                    result: handle_reconnect_result(kind, &server_name),
                }
            }
            (
                Self::Reconnecting,
                ReconnectEvent::AttemptErrored {
                    error_message,
                    server_name,
                },
            ) => Self::Resolved {
                result: ReconnectResult {
                    message: handle_reconnect_error(&error_message, &server_name),
                    success: false,
                },
            },
            (_, ReconnectEvent::Dismiss) => Self::Idle,
            (state, _) => state, // out-of-order events ignored
        }
    }

    /// Whether the dialog should show a spinner.
    pub fn is_in_progress(&self) -> bool {
        matches!(self, Self::Reconnecting)
    }

    /// Whether the dialog is showing a resolved result (so the
    /// consumer can arm the Enter-to-dismiss keybinding).
    pub fn is_resolved(&self) -> bool {
        matches!(self, Self::Resolved { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- handle_reconnect_result table ---

    #[test]
    fn result_connected_is_success() {
        let r = handle_reconnect_result(ReconnectClientKind::Connected, "linear");
        assert_eq!(r.message, "Reconnected to linear.");
        assert!(r.success);
    }

    #[test]
    fn result_needs_auth_is_failure_with_authenticate_quote() {
        let r = handle_reconnect_result(ReconnectClientKind::NeedsAuth, "github");
        assert_eq!(
            r.message,
            "github requires authentication. Use the 'Authenticate' option."
        );
        assert!(!r.success);
        // The literal single-quotes around `Authenticate` are load-
        // bearing; pin them here so a refactor to curly quotes or
        // backticks triggers a CI failure.
        assert!(r.message.contains("'Authenticate'"));
    }

    #[test]
    fn result_failed_is_failure() {
        let r = handle_reconnect_result(ReconnectClientKind::Failed, "slack");
        assert_eq!(r.message, "Failed to reconnect to slack.");
        assert!(!r.success);
    }

    #[test]
    fn result_other_variant_uses_unknown_branch() {
        let r = handle_reconnect_result(ReconnectClientKind::Other, "foo");
        assert_eq!(r.message, "Unknown result when reconnecting to foo.");
        assert!(!r.success);
    }

    #[test]
    fn result_empty_server_name_still_formats() {
        let r = handle_reconnect_result(ReconnectClientKind::Connected, "");
        assert_eq!(r.message, "Reconnected to .");
        assert!(r.success);
    }

    #[test]
    fn result_server_name_with_special_chars() {
        // Periods, colons, and dots pass through — the formatter is
        // not sanitising.
        let r = handle_reconnect_result(ReconnectClientKind::Failed, "my.server:8080");
        assert_eq!(r.message, "Failed to reconnect to my.server:8080.");
    }

    // --- handle_reconnect_error ---

    #[test]
    fn error_wraps_message_with_server_name() {
        assert_eq!(
            handle_reconnect_error("ETIMEDOUT", "linear"),
            "Error reconnecting to linear: ETIMEDOUT",
        );
    }

    #[test]
    fn error_preserves_whitespace_in_message() {
        // Whitespace in the error message is preserved verbatim.
        assert_eq!(
            handle_reconnect_error("  oh no  ", "s"),
            "Error reconnecting to s:   oh no  ",
        );
    }

    #[test]
    fn error_empty_message_still_formats() {
        assert_eq!(handle_reconnect_error("", "s"), "Error reconnecting to s: ",);
    }

    // --- ReconnectState state machine ---

    #[test]
    fn state_default_is_idle() {
        assert_eq!(ReconnectState::default(), ReconnectState::Idle);
    }

    #[test]
    fn state_idle_to_reconnecting_on_start() {
        let s = ReconnectState::Idle.apply(ReconnectEvent::StartReconnect);
        assert_eq!(s, ReconnectState::Reconnecting);
        assert!(s.is_in_progress());
        assert!(!s.is_resolved());
    }

    #[test]
    fn state_reconnecting_to_resolved_on_success() {
        let s = ReconnectState::Reconnecting.apply(ReconnectEvent::AttemptResolved {
            kind: ReconnectClientKind::Connected,
            server_name: "linear".to_string(),
        });
        match s {
            ReconnectState::Resolved { result } => {
                assert!(result.success);
                assert_eq!(result.message, "Reconnected to linear.");
            }
            _ => panic!("expected Resolved"),
        }
    }

    #[test]
    fn state_reconnecting_to_resolved_on_failed() {
        let s = ReconnectState::Reconnecting.apply(ReconnectEvent::AttemptResolved {
            kind: ReconnectClientKind::Failed,
            server_name: "slack".to_string(),
        });
        match s {
            ReconnectState::Resolved { result } => {
                assert!(!result.success);
                assert_eq!(result.message, "Failed to reconnect to slack.");
            }
            _ => panic!("expected Resolved"),
        }
    }

    #[test]
    fn state_reconnecting_to_resolved_on_error() {
        let s = ReconnectState::Reconnecting.apply(ReconnectEvent::AttemptErrored {
            error_message: "ECONNREFUSED".to_string(),
            server_name: "linear".to_string(),
        });
        match s {
            ReconnectState::Resolved { result } => {
                assert!(!result.success);
                assert_eq!(result.message, "Error reconnecting to linear: ECONNREFUSED");
            }
            _ => panic!("expected Resolved"),
        }
    }

    #[test]
    fn state_ignores_out_of_order_start() {
        // Starting reconnect while reconnecting is a no-op.
        let s = ReconnectState::Reconnecting.apply(ReconnectEvent::StartReconnect);
        assert_eq!(s, ReconnectState::Reconnecting);
    }

    #[test]
    fn state_ignores_resolved_event_while_idle() {
        // A late-arriving promise should not affect an idle dialog.
        let s = ReconnectState::Idle.apply(ReconnectEvent::AttemptResolved {
            kind: ReconnectClientKind::Connected,
            server_name: "x".to_string(),
        });
        assert_eq!(s, ReconnectState::Idle);
    }

    #[test]
    fn state_dismiss_from_any_state_returns_to_idle() {
        assert_eq!(
            ReconnectState::Idle.apply(ReconnectEvent::Dismiss),
            ReconnectState::Idle
        );
        assert_eq!(
            ReconnectState::Reconnecting.apply(ReconnectEvent::Dismiss),
            ReconnectState::Idle
        );
        let resolved = ReconnectState::Resolved {
            result: ReconnectResult {
                message: "x".into(),
                success: true,
            },
        };
        assert_eq!(
            resolved.apply(ReconnectEvent::Dismiss),
            ReconnectState::Idle
        );
    }

    #[test]
    fn reconnect_result_table() {
        // Cross-check the full 4x3 table: kind × server-name-variants.
        let cases = [
            (
                ReconnectClientKind::Connected,
                "linear",
                "Reconnected to linear.",
                true,
            ),
            (
                ReconnectClientKind::NeedsAuth,
                "github",
                "github requires authentication. Use the 'Authenticate' option.",
                false,
            ),
            (
                ReconnectClientKind::Failed,
                "slack",
                "Failed to reconnect to slack.",
                false,
            ),
            (
                ReconnectClientKind::Other,
                "unknown-server",
                "Unknown result when reconnecting to unknown-server.",
                false,
            ),
        ];
        for (kind, name, expected_msg, expected_success) in cases {
            let r = handle_reconnect_result(kind, name);
            assert_eq!(r.message, expected_msg);
            assert_eq!(r.success, expected_success);
        }
    }
}
