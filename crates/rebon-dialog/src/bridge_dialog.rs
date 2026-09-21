//! Remote Control bridge status dialog, plus its pure display
//! helpers.
//!
//! ## What is implemented
//!
//! * The fixed title (`Remote Control`).
//! * The status state machine (`get_bridge_status`) — four branches
//!   (failed / reconnecting / active / connecting).
//! * The connect-url and session-url builders.
//! * The idle / active / failed footer text.
//! * The display_url branch (`session_active ? session_url : connect_url`).
//! * The context-suffix projection (`{repo_name} · {branch_name}`).
//! * The verbose `Environment: …` and `Session: …` rows.
//! * The `d` keybinding for disconnect (with the explicit-startup
//!   config-write branch).
//! * The space-toggle QR-code branch.
//! * The fixed footer hint (`d to disconnect · space for QR code · …`).
//! * The `TOOL_DISPLAY_EXPIRY_MS` and `SHIMMER_INTERVAL_MS` constants.
//! * The `abbreviate_activity` truncate-to-30 helper.
//!
//! ## Outbound seam
//!
//! The QR-code rendering, reading the bridge state, the OSC-8
//! hyperlinks, and the `format_duration` helper are all consumer-side.

use crate::common::truncate_chars;

/// The dialog title.
pub const TITLE: &str = "Remote Control";

/// The footer hint line.
pub const FOOTER_HINT: &str = "d to disconnect · space for QR code · Enter/Esc to close";

/// Footer text shown when the bridge failed.
pub const FAILED_FOOTER_TEXT: &str = "Something went wrong, please try again";

/// How long a tool display stays fresh, in milliseconds.
pub const TOOL_DISPLAY_EXPIRY_MS: u64 = 30_000;

/// The shimmer animation interval, in milliseconds.
pub const SHIMMER_INTERVAL_MS: u64 = 150;

/// Bridge status state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeStatusLabel {
    /// "Remote Control failed"
    Failed,
    /// "Remote Control reconnecting"
    Reconnecting,
    /// "Remote Control active"
    Active,
    /// "Remote Control connecting…"
    Connecting,
}

impl BridgeStatusLabel {
    /// String representation. Pinned literal labels.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Failed => "Remote Control failed",
            Self::Reconnecting => "Remote Control reconnecting",
            Self::Active => "Remote Control active",
            Self::Connecting => "Remote Control connecting\u{2026}",
        }
    }
}

/// Color/role for the status pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeStatusColor {
    /// Red — used for the failed branch.
    Error,
    /// Yellow — used for the reconnecting / connecting branches.
    Warning,
    /// Green — used for the active branch.
    Success,
}

/// Result of [`get_bridge_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeStatusInfo {
    /// The label discriminant.
    pub label: BridgeStatusLabel,
    /// The color discriminant.
    pub color: BridgeStatusColor,
}

/// Pre-built bridge state input.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BridgeStateInput {
    /// The bridge's last error, if any.
    pub error: Option<String>,
    /// Whether the bridge is connected.
    pub connected: bool,
    /// Whether a remote session is active.
    pub session_active: bool,
    /// Whether the bridge is reconnecting.
    pub reconnecting: bool,
    /// URL for connecting to this environment.
    pub connect_url: Option<String>,
    /// URL of the active remote session.
    pub session_url: Option<String>,
    /// Registered environment id.
    pub environment_id: Option<String>,
    /// Bridge session id.
    pub session_id: Option<String>,
    /// Whether Remote Control was enabled explicitly at startup.
    pub explicit: bool,
    /// Whether the verbose environment/session rows are shown.
    pub verbose: bool,
}

/// Compute the bridge status label + color.
pub fn get_bridge_status(input: &BridgeStateInput) -> BridgeStatusInfo {
    if input.error.is_some() {
        return BridgeStatusInfo {
            label: BridgeStatusLabel::Failed,
            color: BridgeStatusColor::Error,
        };
    }
    if input.reconnecting {
        return BridgeStatusInfo {
            label: BridgeStatusLabel::Reconnecting,
            color: BridgeStatusColor::Warning,
        };
    }
    if input.session_active || input.connected {
        return BridgeStatusInfo {
            label: BridgeStatusLabel::Active,
            color: BridgeStatusColor::Success,
        };
    }
    BridgeStatusInfo {
        label: BridgeStatusLabel::Connecting,
        color: BridgeStatusColor::Warning,
    }
}

/// Truncate an activity summary to 30 characters.
pub fn abbreviate_activity(summary: &str) -> String {
    truncate_chars(summary, 30)
}

/// Build the connect URL (`{base_url}/code?bridge={environment_id}`).
pub fn build_bridge_connect_url(base_url: &str, environment_id: &str) -> String {
    format!("{base_url}/code?bridge={environment_id}")
}

/// Appends the bridge query parameter. The session URL itself is
/// built by the caller from the session id and ingress URL, with a `cse_` → `session_` prefix swap.
pub fn build_bridge_session_url(remote_session_url: &str, environment_id: &str) -> String {
    format!("{remote_session_url}?bridge={environment_id}")
}

/// Translate `cse_*` session ids to `session_*`.
pub fn translate_session_id(session_id: &str) -> String {
    if let Some(rest) = session_id.strip_prefix("cse_") {
        format!("session_{rest}")
    } else {
        session_id.to_string()
    }
}

/// Build the idle-state footer text.
pub fn build_idle_footer_text(url: &str) -> String {
    format!("Code everywhere with the Rebon app or {url}")
}

/// Build the active-state footer text.
pub fn build_active_footer_text(url: &str) -> String {
    format!("Continue coding in the Rebon app or {url}")
}

/// Pick the URL to display: the session URL when active, else the
/// connect URL.
pub fn display_url(input: &BridgeStateInput) -> Option<&str> {
    if input.session_active {
        input.session_url.as_deref()
    } else {
        input.connect_url.as_deref()
    }
}

/// Build the footer text for the dialog.
pub fn footer_text(input: &BridgeStateInput) -> Option<String> {
    if input.error.is_some() {
        return Some(FAILED_FOOTER_TEXT.to_string());
    }
    let url = display_url(input)?;
    if input.session_active {
        Some(build_active_footer_text(url))
    } else {
        Some(build_idle_footer_text(url))
    }
}

/// Build the context suffix string (` · {repo_name} · {branch_name}`).
pub fn build_context_suffix(repo_name: &str, branch_name: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !repo_name.is_empty() {
        parts.push(repo_name);
    }
    if !branch_name.is_empty() {
        parts.push(branch_name);
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(" \u{00B7} {}", parts.join(" \u{00B7} "))
}

/// Action emitted by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeDialogAction {
    /// User pressed Enter / Escape — close the dialog.
    Close,
    /// User pressed `d` — disconnect. If `explicit` was set, also
    /// write `remote_control_at_startup = false` to global config.
    Disconnect {
        /// True iff the consumer should also write
        /// `remote_control_at_startup = false` to the global config.
        clear_startup_config: bool,
    },
    /// User pressed Space — toggle the QR-code visibility.
    ToggleQr,
    /// Ignored input.
    Ignore,
}

/// Reducer for a key event.
pub fn handle_key(input: &BridgeStateInput, key: char) -> BridgeDialogAction {
    match key {
        'd' => BridgeDialogAction::Disconnect {
            clear_startup_config: input.explicit,
        },
        ' ' => BridgeDialogAction::ToggleQr,
        '\n' | '\r' | '\u{1b}' => BridgeDialogAction::Close,
        _ => BridgeDialogAction::Ignore,
    }
}

/// Cancel handler — same as `Close` (dismissing closes the dialog).
pub fn handle_cancel() -> BridgeDialogAction {
    BridgeDialogAction::Close
}

/// Compute the glimmer index for the reverse-sweep shimmer animation.
pub fn compute_glimmer_index(tick: u64, message_width: usize) -> i64 {
    let cycle_length = (message_width as i64) + 20;
    if cycle_length == 0 {
        return 0;
    }
    let modulo = (tick as i64).rem_euclid(cycle_length);
    (message_width as i64) + 10 - modulo
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(
        error: Option<&str>,
        connected: bool,
        active: bool,
        reconnecting: bool,
    ) -> BridgeStateInput {
        BridgeStateInput {
            error: error.map(String::from),
            connected,
            session_active: active,
            reconnecting,
            ..BridgeStateInput::default()
        }
    }

    #[test]
    fn title_and_footer_pinned() {
        assert_eq!(TITLE, "Remote Control");
        assert!(FOOTER_HINT.contains("d to disconnect"));
        assert_eq!(FAILED_FOOTER_TEXT, "Something went wrong, please try again");
    }

    #[test]
    fn timing_constants_pinned() {
        assert_eq!(TOOL_DISPLAY_EXPIRY_MS, 30_000);
        assert_eq!(SHIMMER_INTERVAL_MS, 150);
    }

    #[test]
    fn label_as_str_table() {
        assert_eq!(BridgeStatusLabel::Failed.as_str(), "Remote Control failed");
        assert_eq!(
            BridgeStatusLabel::Reconnecting.as_str(),
            "Remote Control reconnecting"
        );
        assert_eq!(BridgeStatusLabel::Active.as_str(), "Remote Control active");
        assert_eq!(
            BridgeStatusLabel::Connecting.as_str(),
            "Remote Control connecting\u{2026}"
        );
    }

    #[test]
    fn get_bridge_status_failed() {
        let s = state(Some("oops"), true, true, true);
        let info = get_bridge_status(&s);
        assert_eq!(info.label, BridgeStatusLabel::Failed);
        assert_eq!(info.color, BridgeStatusColor::Error);
    }

    #[test]
    fn get_bridge_status_reconnecting() {
        let s = state(None, true, true, true);
        let info = get_bridge_status(&s);
        assert_eq!(info.label, BridgeStatusLabel::Reconnecting);
        assert_eq!(info.color, BridgeStatusColor::Warning);
    }

    #[test]
    fn get_bridge_status_active_via_session() {
        let s = state(None, false, true, false);
        let info = get_bridge_status(&s);
        assert_eq!(info.label, BridgeStatusLabel::Active);
        assert_eq!(info.color, BridgeStatusColor::Success);
    }

    #[test]
    fn get_bridge_status_active_via_connected() {
        let s = state(None, true, false, false);
        let info = get_bridge_status(&s);
        assert_eq!(info.label, BridgeStatusLabel::Active);
    }

    #[test]
    fn get_bridge_status_connecting() {
        let s = state(None, false, false, false);
        let info = get_bridge_status(&s);
        assert_eq!(info.label, BridgeStatusLabel::Connecting);
        assert_eq!(info.color, BridgeStatusColor::Warning);
    }

    #[test]
    fn abbreviate_activity_short() {
        assert_eq!(abbreviate_activity("hello"), "hello");
    }

    #[test]
    fn abbreviate_activity_long_truncates() {
        let s = "this is a very long activity summary that exceeds 30 chars";
        let abbrev = abbreviate_activity(s);
        assert_eq!(abbrev.chars().count(), 30);
        assert!(abbrev.ends_with("…"));
    }

    #[test]
    fn build_bridge_connect_url() {
        assert_eq!(
            super::build_bridge_connect_url("https://bridge.example", "env-1"),
            "https://bridge.example/code?bridge=env-1"
        );
    }

    #[test]
    fn build_bridge_session_url() {
        assert_eq!(
            super::build_bridge_session_url("https://bridge.example/session/x", "env-1"),
            "https://bridge.example/session/x?bridge=env-1"
        );
    }

    #[test]
    fn translate_session_id_table() {
        assert_eq!(translate_session_id("cse_abc"), "session_abc");
        assert_eq!(translate_session_id("session_xyz"), "session_xyz");
        assert_eq!(translate_session_id("plain"), "plain");
    }

    #[test]
    fn build_idle_footer_pinned() {
        assert_eq!(
            build_idle_footer_text("https://x"),
            "Code everywhere with the Rebon app or https://x"
        );
    }

    #[test]
    fn build_active_footer_pinned() {
        assert_eq!(
            build_active_footer_text("https://x"),
            "Continue coding in the Rebon app or https://x"
        );
    }

    #[test]
    fn display_url_session_active_uses_session() {
        let s = BridgeStateInput {
            session_active: true,
            session_url: Some("session-url".into()),
            connect_url: Some("connect-url".into()),
            ..BridgeStateInput::default()
        };
        assert_eq!(display_url(&s), Some("session-url"));
    }

    #[test]
    fn display_url_idle_uses_connect() {
        let s = BridgeStateInput {
            session_active: false,
            session_url: Some("session-url".into()),
            connect_url: Some("connect-url".into()),
            ..BridgeStateInput::default()
        };
        assert_eq!(display_url(&s), Some("connect-url"));
    }

    #[test]
    fn footer_text_failed_branch() {
        let s = BridgeStateInput {
            error: Some("nope".into()),
            ..BridgeStateInput::default()
        };
        assert_eq!(footer_text(&s).as_deref(), Some(FAILED_FOOTER_TEXT));
    }

    #[test]
    fn footer_text_idle_branch() {
        let s = BridgeStateInput {
            connect_url: Some("u".into()),
            ..BridgeStateInput::default()
        };
        assert_eq!(
            footer_text(&s).as_deref(),
            Some("Code everywhere with the Rebon app or u")
        );
    }

    #[test]
    fn footer_text_active_branch() {
        let s = BridgeStateInput {
            session_active: true,
            session_url: Some("u".into()),
            ..BridgeStateInput::default()
        };
        assert_eq!(
            footer_text(&s).as_deref(),
            Some("Continue coding in the Rebon app or u")
        );
    }

    #[test]
    fn footer_text_no_url_no_text() {
        let s = BridgeStateInput::default();
        assert_eq!(footer_text(&s), None);
    }

    #[test]
    fn build_context_suffix_empty() {
        assert_eq!(build_context_suffix("", ""), "");
    }

    #[test]
    fn build_context_suffix_repo_only() {
        assert_eq!(build_context_suffix("repo", ""), " \u{00B7} repo");
    }

    #[test]
    fn build_context_suffix_branch_only() {
        assert_eq!(build_context_suffix("", "main"), " \u{00B7} main");
    }

    #[test]
    fn build_context_suffix_both() {
        assert_eq!(
            build_context_suffix("repo", "main"),
            " \u{00B7} repo \u{00B7} main"
        );
    }

    #[test]
    fn handle_key_d_disconnects() {
        let s = BridgeStateInput {
            explicit: true,
            ..BridgeStateInput::default()
        };
        assert_eq!(
            handle_key(&s, 'd'),
            BridgeDialogAction::Disconnect {
                clear_startup_config: true
            }
        );
    }

    #[test]
    fn handle_key_d_no_explicit_no_clear() {
        let s = BridgeStateInput::default();
        assert_eq!(
            handle_key(&s, 'd'),
            BridgeDialogAction::Disconnect {
                clear_startup_config: false
            }
        );
    }

    #[test]
    fn handle_key_space_toggles_qr() {
        let s = BridgeStateInput::default();
        assert_eq!(handle_key(&s, ' '), BridgeDialogAction::ToggleQr);
    }

    #[test]
    fn handle_key_enter_closes() {
        let s = BridgeStateInput::default();
        assert_eq!(handle_key(&s, '\n'), BridgeDialogAction::Close);
        assert_eq!(handle_key(&s, '\r'), BridgeDialogAction::Close);
    }

    #[test]
    fn handle_key_escape_closes() {
        let s = BridgeStateInput::default();
        assert_eq!(handle_key(&s, '\u{1b}'), BridgeDialogAction::Close);
    }

    #[test]
    fn handle_key_other_ignored() {
        let s = BridgeStateInput::default();
        assert_eq!(handle_key(&s, 'x'), BridgeDialogAction::Ignore);
        assert_eq!(handle_key(&s, '0'), BridgeDialogAction::Ignore);
    }

    #[test]
    fn handle_cancel_closes() {
        assert_eq!(handle_cancel(), BridgeDialogAction::Close);
    }

    #[test]
    fn compute_glimmer_index_basic() {
        // tick 0 with message width 10 → cycle = 30, mod = 0, result = 20
        assert_eq!(compute_glimmer_index(0, 10), 20);
        // tick 5 → 20 - 5 = 15
        assert_eq!(compute_glimmer_index(5, 10), 15);
        // tick 30 → 0 (wraps)
        assert_eq!(compute_glimmer_index(30, 10), 20);
    }

    #[test]
    fn compute_glimmer_index_zero_width() {
        assert_eq!(compute_glimmer_index(5, 0), 5);
    }
}
