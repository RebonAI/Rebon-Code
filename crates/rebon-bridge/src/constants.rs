//! Constants shared across the bridge: the default session timeout, the
//! disconnect message and the outbound-only error copy.
//!
//! They sit in their own module so a caller can import the string copy without
//! dragging in the messaging and registry types.

/// Default per-session timeout — 24 hours expressed in milliseconds.
pub const DEFAULT_SESSION_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

/// Message shown when the user disconnects Remote Control.
pub const REMOTE_CONTROL_DISCONNECTED_MSG: &str = "Remote Control disconnected.";

/// Error returned for mutable control requests received while the
/// session is in outbound-only mode.
pub const OUTBOUND_ONLY_ERROR: &str =
    "This session is outbound-only. Enable Remote Control locally to allow inbound control.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timeout_is_24h() {
        assert_eq!(DEFAULT_SESSION_TIMEOUT_MS, 86_400_000);
    }

    #[test]
    fn outbound_only_error_mentions_remote_control() {
        assert!(OUTBOUND_ONLY_ERROR.contains("outbound-only"));
        assert!(OUTBOUND_ONLY_ERROR.contains("Remote Control"));
    }

    #[test]
    fn remote_control_disconnected_msg_is_short_and_terminal() {
        assert_eq!(
            REMOTE_CONTROL_DISCONNECTED_MSG,
            "Remote Control disconnected."
        );
    }
}
