//! MCP server connection status + lifecycle transitions.
//!
//! Models the MCP server connection status as a discriminated union
//! plus the lifecycle transitions [`transition_status`] applies to it.
//!
//! There are five status variants:
//!
//! | Variant | Purpose |
//! |---|---|
//! | `'pending'` | dispatched but not yet resolved; may carry reconnect counters |
//! | `'connected'` | live, can call tools |
//! | `'failed'` | last attempt failed (terminal until retry) |
//! | `'needs-auth'` | OAuth required (transport-level) |
//! | `'disabled'` | user / config explicitly disabled the server |
//!
//! Runtime-supported transitions:
//!
//! ```text
//! Pending           → Connected   (handshake succeeded)
//! Pending           → Failed      (handshake failed)
//! Pending           → NeedsAuth   (server returned 401 / 403)
//! Any but Disabled  → Pending     (reconnect dispatched)
//! Any               → Disabled    (user toggled off)
//! Disabled          → Pending     (user toggled on)
//! Failed | NeedsAuth | Connected → Connected  (late handshake success)
//! Failed | NeedsAuth | Connected → Failed     (late handshake failure)
//! Failed | Connected             → NeedsAuth  (late auth challenge)
//! ```
//!
//! Illegal transitions return an **error** so bugs in the consumer's
//! dispatch chain surface deterministically instead of being dropped.

use crate::runtime::config::ScopedMcpServerConfig;

/// The current status of an MCP server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McpServerStatus {
    /// Dispatched but not yet resolved. May be a fresh connect or
    /// a reconnect attempt.
    Pending,
    /// Connected — tools / prompts / resources are callable.
    Connected,
    /// The last handshake attempt failed. Terminal until the user
    /// triggers a reconnect.
    Failed,
    /// The server returned an auth challenge. The consumer should
    /// show the Authenticate menu.
    NeedsAuth,
    /// User / config explicitly disabled the server. No handshake
    /// is running.
    Disabled,
}

impl McpServerStatus {
    /// The wire string — used in the serialized `type` field.
    pub fn as_str(&self) -> &'static str {
        match self {
            McpServerStatus::Pending => "pending",
            McpServerStatus::Connected => "connected",
            McpServerStatus::Failed => "failed",
            McpServerStatus::NeedsAuth => "needs-auth",
            McpServerStatus::Disabled => "disabled",
        }
    }

    /// Parse a wire string back into the enum.
    pub fn from_str(s: &str) -> Option<McpServerStatus> {
        match s {
            "pending" => Some(McpServerStatus::Pending),
            "connected" => Some(McpServerStatus::Connected),
            "failed" => Some(McpServerStatus::Failed),
            "needs-auth" => Some(McpServerStatus::NeedsAuth),
            "disabled" => Some(McpServerStatus::Disabled),
            _ => None,
        }
    }

    /// All five variants in persisted declaration order.
    pub const ALL: [McpServerStatus; 5] = [
        McpServerStatus::Pending,
        McpServerStatus::Connected,
        McpServerStatus::Failed,
        McpServerStatus::NeedsAuth,
        McpServerStatus::Disabled,
    ];

    /// Whether the status is terminal (no pending work). `Pending`
    /// is the only non-terminal state.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, McpServerStatus::Pending)
    }

    /// Whether tool calls can be dispatched in this state.
    pub fn can_call_tools(&self) -> bool {
        matches!(self, McpServerStatus::Connected)
    }

    /// Whether the UI should show an error banner.
    pub fn is_error(&self) -> bool {
        matches!(self, McpServerStatus::Failed | McpServerStatus::NeedsAuth)
    }
}

/// A snapshot of a server — its name, current status, config, and
/// optional auxiliary state (error message, reconnect attempt count,
/// etc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSnapshot {
    pub name: String,
    pub status: McpServerStatus,
    pub config: ScopedMcpServerConfig,
    /// The error message from the last failed attempt (or from a
    /// Connected → Failed transition). None when `status != Failed`.
    pub error: Option<String>,
    /// The current reconnect attempt counter (1-indexed for the
    /// first reconnect). Only meaningful in `Pending`.
    pub reconnect_attempt: Option<u32>,
    pub max_reconnect_attempts: Option<u32>,
}

/// An event that can drive a status transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpStatusEvent {
    /// Handshake completed successfully — move to Connected.
    HandshakeSucceeded,
    /// Handshake failed with a descriptive error — move to Failed.
    HandshakeFailed { error: String },
    /// Transport reported an auth challenge — move to NeedsAuth.
    AuthChallenge,
    /// User or settings toggle disabled the server — move to Disabled.
    Disable,
    /// User or settings toggle re-enabled the server — move to
    /// Pending (so the reconnect loop picks it up).
    Enable,
    /// User triggered a reconnect — move to Pending with the
    /// supplied attempt counter.
    Reconnect {
        reconnect_attempt: u32,
        max_reconnect_attempts: u32,
    },
}

/// Errors returned by [`transition_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusTransitionError {
    /// The event is illegal from the current state (e.g. a
    /// `HandshakeSucceeded` from `Disabled`).
    IllegalTransition {
        from: McpServerStatus,
        event: &'static str,
    },
}

/// Apply an event and return the next status plus any side-effect
/// mutations to the snapshot's auxiliary fields.
///
/// This function explicitly rejects illegal transitions (e.g.
/// `HandshakeSucceeded` from `Disabled`). That surfaces ordering
/// bugs in the consumer's dispatch chain — the tradeoff is a small
/// amount of defensive code at the call site.
pub fn transition_status(
    snapshot: &McpServerSnapshot,
    event: McpStatusEvent,
) -> Result<McpServerSnapshot, StatusTransitionError> {
    let next = match (snapshot.status, &event) {
        // Pending → Connected / Failed / NeedsAuth
        (McpServerStatus::Pending, McpStatusEvent::HandshakeSucceeded) => McpServerSnapshot {
            status: McpServerStatus::Connected,
            error: None,
            reconnect_attempt: None,
            max_reconnect_attempts: None,
            ..snapshot.clone()
        },
        (McpServerStatus::Pending, McpStatusEvent::HandshakeFailed { error }) => {
            McpServerSnapshot {
                status: McpServerStatus::Failed,
                error: Some(error.clone()),
                reconnect_attempt: None,
                max_reconnect_attempts: None,
                ..snapshot.clone()
            }
        }
        (McpServerStatus::Pending, McpStatusEvent::AuthChallenge) => McpServerSnapshot {
            status: McpServerStatus::NeedsAuth,
            error: None,
            reconnect_attempt: None,
            max_reconnect_attempts: None,
            ..snapshot.clone()
        },
        // Any non-disabled → Pending on Reconnect
        (
            _,
            McpStatusEvent::Reconnect {
                reconnect_attempt,
                max_reconnect_attempts,
            },
        ) if snapshot.status != McpServerStatus::Disabled => McpServerSnapshot {
            status: McpServerStatus::Pending,
            error: None,
            reconnect_attempt: Some(*reconnect_attempt),
            max_reconnect_attempts: Some(*max_reconnect_attempts),
            ..snapshot.clone()
        },
        // Disabled → Pending on Enable
        (McpServerStatus::Disabled, McpStatusEvent::Enable) => McpServerSnapshot {
            status: McpServerStatus::Pending,
            error: None,
            reconnect_attempt: None,
            max_reconnect_attempts: None,
            ..snapshot.clone()
        },
        // Any → Disabled on Disable
        (_, McpStatusEvent::Disable) => McpServerSnapshot {
            status: McpServerStatus::Disabled,
            error: None,
            reconnect_attempt: None,
            max_reconnect_attempts: None,
            ..snapshot.clone()
        },
        // Connected/Failed/NeedsAuth → Connected is also legal via
        // auto-reconnect; a HandshakeSucceeded can arrive without an
        // explicit Pending step.
        (
            McpServerStatus::Failed | McpServerStatus::NeedsAuth | McpServerStatus::Connected,
            McpStatusEvent::HandshakeSucceeded,
        ) => McpServerSnapshot {
            status: McpServerStatus::Connected,
            error: None,
            reconnect_attempt: None,
            max_reconnect_attempts: None,
            ..snapshot.clone()
        },
        (
            McpServerStatus::Failed | McpServerStatus::NeedsAuth | McpServerStatus::Connected,
            McpStatusEvent::HandshakeFailed { error },
        ) => McpServerSnapshot {
            status: McpServerStatus::Failed,
            error: Some(error.clone()),
            ..snapshot.clone()
        },
        (McpServerStatus::Failed | McpServerStatus::Connected, McpStatusEvent::AuthChallenge) => {
            McpServerSnapshot {
                status: McpServerStatus::NeedsAuth,
                error: None,
                ..snapshot.clone()
            }
        }
        // Illegal transitions
        (McpServerStatus::Disabled, _) => {
            return Err(StatusTransitionError::IllegalTransition {
                from: McpServerStatus::Disabled,
                event: event_name(&event),
            });
        }
        (from, _) => {
            return Err(StatusTransitionError::IllegalTransition {
                from,
                event: event_name(&event),
            });
        }
    };
    Ok(next)
}

fn event_name(e: &McpStatusEvent) -> &'static str {
    match e {
        McpStatusEvent::HandshakeSucceeded => "HandshakeSucceeded",
        McpStatusEvent::HandshakeFailed { .. } => "HandshakeFailed",
        McpStatusEvent::AuthChallenge => "AuthChallenge",
        McpStatusEvent::Disable => "Disable",
        McpStatusEvent::Enable => "Enable",
        McpStatusEvent::Reconnect { .. } => "Reconnect",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::config::{McpStdioServerConfig, ServerConfigKind};

    fn sample_snapshot(status: McpServerStatus) -> McpServerSnapshot {
        McpServerSnapshot {
            name: "linear".into(),
            status,
            config: ScopedMcpServerConfig {
                config: ServerConfigKind::Stdio(McpStdioServerConfig {
                    command: "node".into(),
                    args: vec![],
                    env: None,
                }),
                scope: crate::runtime::config::ConfigScope::User,
                plugin_source: None,
            },
            error: None,
            reconnect_attempt: None,
            max_reconnect_attempts: None,
        }
    }

    // --- as_str / from_str round-trips ---

    #[test]
    fn status_wire_strings_are_pinned() {
        assert_eq!(McpServerStatus::Pending.as_str(), "pending");
        assert_eq!(McpServerStatus::Connected.as_str(), "connected");
        assert_eq!(McpServerStatus::Failed.as_str(), "failed");
        assert_eq!(McpServerStatus::NeedsAuth.as_str(), "needs-auth");
        assert_eq!(McpServerStatus::Disabled.as_str(), "disabled");
    }

    #[test]
    fn status_from_str_round_trips_all() {
        for status in McpServerStatus::ALL {
            assert_eq!(McpServerStatus::from_str(status.as_str()), Some(status));
        }
    }

    #[test]
    fn status_from_str_unknown_is_none() {
        assert_eq!(McpServerStatus::from_str("connecting"), None); // close but not a variant
        assert_eq!(McpServerStatus::from_str("Connected"), None); // case-sensitive
        assert_eq!(McpServerStatus::from_str(""), None);
    }

    #[test]
    fn status_all_has_five_entries() {
        assert_eq!(McpServerStatus::ALL.len(), 5);
    }

    // --- is_terminal / can_call_tools / is_error ---

    #[test]
    fn is_terminal_pin() {
        assert!(!McpServerStatus::Pending.is_terminal());
        assert!(McpServerStatus::Connected.is_terminal());
        assert!(McpServerStatus::Failed.is_terminal());
        assert!(McpServerStatus::NeedsAuth.is_terminal());
        assert!(McpServerStatus::Disabled.is_terminal());
    }

    #[test]
    fn only_connected_can_call_tools() {
        for status in McpServerStatus::ALL {
            assert_eq!(
                status.can_call_tools(),
                status == McpServerStatus::Connected
            );
        }
    }

    #[test]
    fn failed_and_needs_auth_are_errors() {
        assert!(McpServerStatus::Failed.is_error());
        assert!(McpServerStatus::NeedsAuth.is_error());
        assert!(!McpServerStatus::Connected.is_error());
        assert!(!McpServerStatus::Pending.is_error());
        assert!(!McpServerStatus::Disabled.is_error());
    }

    // --- transition_status happy paths ---

    #[test]
    fn pending_to_connected_on_handshake_success() {
        let s = sample_snapshot(McpServerStatus::Pending);
        let next = transition_status(&s, McpStatusEvent::HandshakeSucceeded).unwrap();
        assert_eq!(next.status, McpServerStatus::Connected);
        assert!(next.error.is_none());
        assert!(next.reconnect_attempt.is_none());
    }

    #[test]
    fn pending_to_failed_on_handshake_fail_preserves_error() {
        let s = sample_snapshot(McpServerStatus::Pending);
        let next = transition_status(
            &s,
            McpStatusEvent::HandshakeFailed {
                error: "ECONNREFUSED".into(),
            },
        )
        .unwrap();
        assert_eq!(next.status, McpServerStatus::Failed);
        assert_eq!(next.error.as_deref(), Some("ECONNREFUSED"));
    }

    #[test]
    fn pending_to_needs_auth_on_challenge() {
        let s = sample_snapshot(McpServerStatus::Pending);
        let next = transition_status(&s, McpStatusEvent::AuthChallenge).unwrap();
        assert_eq!(next.status, McpServerStatus::NeedsAuth);
    }

    #[test]
    fn reconnect_sets_counters() {
        let s = sample_snapshot(McpServerStatus::Failed);
        let next = transition_status(
            &s,
            McpStatusEvent::Reconnect {
                reconnect_attempt: 2,
                max_reconnect_attempts: 5,
            },
        )
        .unwrap();
        assert_eq!(next.status, McpServerStatus::Pending);
        assert_eq!(next.reconnect_attempt, Some(2));
        assert_eq!(next.max_reconnect_attempts, Some(5));
        // Error cleared because the reconnect attempt is fresh.
        assert!(next.error.is_none());
    }

    #[test]
    fn reconnect_from_connected_is_legal() {
        // Reconnect from any non-disabled state is allowed (e.g. the
        // user forces a refresh on a healthy connection).
        let s = sample_snapshot(McpServerStatus::Connected);
        let next = transition_status(
            &s,
            McpStatusEvent::Reconnect {
                reconnect_attempt: 1,
                max_reconnect_attempts: 3,
            },
        )
        .unwrap();
        assert_eq!(next.status, McpServerStatus::Pending);
    }

    #[test]
    fn disable_from_any_state() {
        for status in [
            McpServerStatus::Pending,
            McpServerStatus::Connected,
            McpServerStatus::Failed,
            McpServerStatus::NeedsAuth,
        ] {
            let s = sample_snapshot(status);
            let next = transition_status(&s, McpStatusEvent::Disable).unwrap();
            assert_eq!(next.status, McpServerStatus::Disabled);
        }
    }

    #[test]
    fn enable_from_disabled_goes_pending() {
        let s = sample_snapshot(McpServerStatus::Disabled);
        let next = transition_status(&s, McpStatusEvent::Enable).unwrap();
        assert_eq!(next.status, McpServerStatus::Pending);
    }

    // --- transition_status illegal paths ---

    #[test]
    fn illegal_enable_from_connected() {
        let s = sample_snapshot(McpServerStatus::Connected);
        let result = transition_status(&s, McpStatusEvent::Enable);
        assert!(matches!(
            result,
            Err(StatusTransitionError::IllegalTransition {
                from: McpServerStatus::Connected,
                event: "Enable"
            })
        ));
    }

    #[test]
    fn illegal_handshake_succeeded_from_disabled() {
        let s = sample_snapshot(McpServerStatus::Disabled);
        let result = transition_status(&s, McpStatusEvent::HandshakeSucceeded);
        assert!(matches!(
            result,
            Err(StatusTransitionError::IllegalTransition {
                from: McpServerStatus::Disabled,
                event: "HandshakeSucceeded"
            })
        ));
    }

    #[test]
    fn illegal_reconnect_from_disabled() {
        let s = sample_snapshot(McpServerStatus::Disabled);
        let result = transition_status(
            &s,
            McpStatusEvent::Reconnect {
                reconnect_attempt: 1,
                max_reconnect_attempts: 3,
            },
        );
        assert!(matches!(
            result,
            Err(StatusTransitionError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn auto_recover_connected_from_failed() {
        // A HandshakeSucceeded can arrive straight from Failed when
        // auto-reconnect wins a race; pin this as legal so we don't
        // reject it.
        let s = sample_snapshot(McpServerStatus::Failed);
        let next = transition_status(&s, McpStatusEvent::HandshakeSucceeded).unwrap();
        assert_eq!(next.status, McpServerStatus::Connected);
    }

    #[test]
    fn auto_recover_from_needs_auth_after_user_auth() {
        let s = sample_snapshot(McpServerStatus::NeedsAuth);
        let next = transition_status(&s, McpStatusEvent::HandshakeSucceeded).unwrap();
        assert_eq!(next.status, McpServerStatus::Connected);
    }

    #[test]
    fn handshake_fail_from_connected_moves_to_failed() {
        // Mid-session disconnect.
        let s = sample_snapshot(McpServerStatus::Connected);
        let next = transition_status(
            &s,
            McpStatusEvent::HandshakeFailed {
                error: "EPIPE".into(),
            },
        )
        .unwrap();
        assert_eq!(next.status, McpServerStatus::Failed);
        assert_eq!(next.error.as_deref(), Some("EPIPE"));
    }

    #[test]
    fn pending_does_not_accept_enable() {
        let s = sample_snapshot(McpServerStatus::Pending);
        let result = transition_status(&s, McpStatusEvent::Enable);
        assert!(matches!(
            result,
            Err(StatusTransitionError::IllegalTransition {
                from: McpServerStatus::Pending,
                event: "Enable"
            })
        ));
    }

    #[test]
    fn snapshot_preserves_name_and_config() {
        let s = sample_snapshot(McpServerStatus::Pending);
        let next = transition_status(&s, McpStatusEvent::HandshakeSucceeded).unwrap();
        assert_eq!(next.name, "linear");
        assert_eq!(next.config.scope, crate::runtime::config::ConfigScope::User);
    }
}
