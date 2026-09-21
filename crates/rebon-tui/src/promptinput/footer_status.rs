//! Composes `crate::status` widget outputs for the prompt input surface.
//!
//! ## Scope — where each widget lives
//!
//! The sibling `crate::status` module holds independent widgets. Only a
//! subset of them is drawn inside the prompt input surface; the rest
//! lives elsewhere (message rows,
//! onboarding, etc.). This module **only** composes the
//! widgets that belong to the prompt input surface:
//!
//! ### Footer right — notification column
//!
//! The right side of the prompt footer draws an ordered column:
//!
//! | # | Widget                | Visibility condition                       |
//! |---|---------------------- |--------------------------------------------|
//! | 1 | IDE status indicator  | *(not from `crate::status`)*               |
//! | 2 | Notification queue    | *(see effort notification below)*          |
//! | 3 | Overage warning       | *(not from `crate::status`)*               |
//! | 4 | API key slow warning  | *(not from `crate::status`)*               |
//! | 5 | Auth status           | *(not from `crate::status`)*               |
//! | 6 | Debug mode            | *(not from `crate::status`)*               |
//! | 7 | Token count (verbose) | *(not from `crate::status`)*               |
//! | 8 | **Token warning**     | `!is_brief_only` (external) + above threshold (internal) |
//! | 9 | Auto updater          | *(not from `crate::status`)*               |
//! |10 | Voice error           | *(not from `crate::status`)*               |
//! |11 | Sandbox footer hint   | *(not from `crate::status`)*               |
//!
//! ### Notification queue
//!
//! The **effort indicator** is not drawn directly. Instead,
//! [`get_effort_notification_text`] is called and the
//! result pushed into the notification queue (key `"effort-level"`,
//! priority `"high"`, timeout 12 000 ms). The notification queue is
//! drawn at position 2 of the notification column.
//!
//! ### Footer left
//!
//! The left side draws the status line (from `crate::status::status_line`)
//! when all of these are true:
//! * `mode == "prompt"`
//! * `has_height` (terminal has enough height)
//! * `!exit_message_showing`
//! * `!is_pasting`
//! * `status_line_should_display(&settings, assistant_mode_enabled, assistant_mode_active)`
//!
//! ### Widgets NOT in the prompt footer
//!
//! These `crate::status` widgets are drawn by other surfaces.
//! Consumers should use `crate::status` directly for them:
//!
//! * **`session_background_hint`** → sibling of the prompt input
//! * **`status_notices`** → logo / message area
//! * **`bash_mode_progress`** → message area
//! * **`tool_use_loader`** → assistant tool-use and advisor message rows
//! * **`press_enter_to_continue`** → static text row; no consumer wired up yet

use crate::status::status_line::{status_line_should_display, StatusLineSettings};
use crate::status::token_warning::{
    token_warning_layout, TokenWarningInputs, TokenWarningLayout, TokenWarningMode,
    TokenWarningState,
};
use rebon_types::{effort_indicator::get_effort_notification_text, ReasoningEffort};

// ── Notification area (right side) ───────────────────────────────

/// A single visible status item in the footer notification area.
/// Variants appear in the same render order as the notification column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterNotificationItem {
    /// Position 8 — token warning badge. Emitted only when brief-only
    /// mode is off and the warning mode is not `Hidden`.
    TokenWarning(TokenWarningLayout),
}

/// Inputs for [`resolve_footer_notifications`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterNotificationsInput {
    /// Token warning inputs (usage, thresholds, feature flags).
    pub token_warning: TokenWarningInputs,
    /// Whether brief-only mode is active. When true, the token warning
    /// badge is not emitted at all.
    pub is_brief_only: bool,
}

/// Resolved notification-area status layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterNotificationsLayout {
    /// Visible status items in render order. The consumer iterates
    /// this vec and draws each item at the appropriate position
    /// within the notification column.
    pub items: Vec<FooterNotificationItem>,
    /// Token warning state — always computed even when the widget is
    /// hidden or not mounted, because downstream logic
    /// (e.g. `is_at_blocking_limit` gating) reads it.
    pub token_warning_state: TokenWarningState,
}

/// Resolves the `crate::status` notification-area widgets. Items in
/// [`FooterNotificationsLayout::items`] appear in notification-column
/// render order; only visible items are included.
pub fn resolve_footer_notifications(input: &FooterNotificationsInput) -> FooterNotificationsLayout {
    let tw_layout = token_warning_layout(&input.token_warning);
    let mut items = Vec::new();

    // Position 8: token warning — emitted only when `!is_brief_only`
    // and the warning mode is not `Hidden`.
    if !input.is_brief_only && !matches!(tw_layout.mode, TokenWarningMode::Hidden) {
        items.push(FooterNotificationItem::TokenWarning(tw_layout.clone()));
    }

    FooterNotificationsLayout {
        items,
        token_warning_state: tw_layout.state,
    }
}

// ── Effort notification (notification queue) ─────────────────────

/// Inputs for [`resolve_effort_notification`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffortNotificationInput {
    /// Resolved effort level. `None` when the model doesn't support
    /// effort or effort is not configured.
    pub effort_level: Option<ReasoningEffort>,
    /// Whether brief mode owns the gap. When true, the effort
    /// notification is suppressed because the local client's effort
    /// doesn't reflect the connected agent's. When true the resolved
    /// text is `None` instead of the effort text.
    pub brief_owns_gap: bool,
}

/// Resolved effort notification for the notification queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortNotificationLayout {
    /// Notification text (e.g. `"● high · /effort"`). `None` when
    /// suppressed or unsupported. When `Some`, the consumer should
    /// push to the notification queue with:
    /// * key: `"effort-level"`
    /// * priority: `"high"`
    /// * timeout: `12_000` ms
    pub text: Option<String>,
}

/// Notification queue key for the effort notification.
pub const EFFORT_NOTIFICATION_KEY: &str = "effort-level";

/// Timeout for the effort notification in the queue (ms).
pub const EFFORT_NOTIFICATION_TIMEOUT_MS: u64 = 12_000;

/// Resolves the effort notification text for the notification queue:
/// `None` when `brief_owns_gap` is set, otherwise the text derived from
/// `effort_level`.
pub fn resolve_effort_notification(input: EffortNotificationInput) -> EffortNotificationLayout {
    let text = if input.brief_owns_gap {
        None
    } else {
        get_effort_notification_text(input.effort_level)
    };
    EffortNotificationLayout { text }
}

// ── StatusLine visibility (footer left side) ─────────────────────

/// Inputs for [`resolve_status_line_visibility`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLineVisibilityInput {
    /// Current prompt mode string (e.g. `"prompt"`, `"bash"`).
    pub mode: String,
    /// Whether the terminal has enough height for the status line.
    pub has_height: bool,
    /// Whether the exit message is showing.
    pub exit_message_showing: bool,
    /// Whether a paste is in progress.
    pub is_pasting: bool,
    /// Status line settings (contains `status_line: Option<StatusLineConfig>`).
    pub settings: StatusLineSettings,
    /// Whether assistant mode is available in this build.
    pub assistant_mode_enabled: bool,
    /// Whether assistant mode is currently active.
    pub assistant_mode_active: bool,
}

/// Whether the status line should be drawn on the footer's left side: the
/// mode is `"prompt"`, there is enough height, the exit message is not
/// showing, no paste is in progress, and the status-line settings together
/// with the two assistant-mode flags say to display it.
pub fn resolve_status_line_visibility(input: &StatusLineVisibilityInput) -> bool {
    input.mode == "prompt"
        && input.has_height
        && !input.exit_message_showing
        && !input.is_pasting
        && status_line_should_display(
            &input.settings,
            input.assistant_mode_enabled,
            input.assistant_mode_active,
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::token_warning::TokenWarningMode;

    fn base_notifications_input() -> FooterNotificationsInput {
        FooterNotificationsInput {
            token_warning: TokenWarningInputs {
                token_usage: 50_000,
                effective_context_window: 200_000,
                is_auto_compact_enabled: true,
                suppress_warning: false,
                show_auto_compact_warning: false,
                reactive_only_mode: false,
                collapse_mode: false,
                upgrade_message: None,
                blocking_limit_override: None,
            },
            is_brief_only: false,
        }
    }

    // ── notification area: render order ───────────────────────────

    #[test]
    fn empty_items_when_all_below_threshold() {
        let layout = resolve_footer_notifications(&base_notifications_input());
        assert!(layout.items.is_empty());
    }

    // ── notification area: token warning visibility ──────────────

    #[test]
    fn token_warning_hidden_when_below_threshold() {
        let layout = resolve_footer_notifications(&base_notifications_input());
        assert!(!layout
            .items
            .iter()
            .any(|i| matches!(i, FooterNotificationItem::TokenWarning(_))));
    }

    #[test]
    fn token_warning_visible_when_above_threshold() {
        let mut input = base_notifications_input();
        input.token_warning.token_usage = 170_000;
        let layout = resolve_footer_notifications(&input);
        assert!(layout
            .items
            .iter()
            .any(|i| matches!(i, FooterNotificationItem::TokenWarning(_))));
    }

    #[test]
    fn token_warning_not_mounted_when_brief_only() {
        let mut input = base_notifications_input();
        input.token_warning.token_usage = 170_000;
        input.is_brief_only = true;
        let layout = resolve_footer_notifications(&input);
        assert!(!layout
            .items
            .iter()
            .any(|i| matches!(i, FooterNotificationItem::TokenWarning(_))));
    }

    #[test]
    fn token_warning_hidden_when_suppressed() {
        let mut input = base_notifications_input();
        input.token_warning.token_usage = 170_000;
        input.token_warning.suppress_warning = true;
        let layout = resolve_footer_notifications(&input);
        assert!(layout.items.is_empty());
    }

    #[test]
    fn token_warning_state_always_available_even_when_brief() {
        let mut input = base_notifications_input();
        input.token_warning.token_usage = 170_000;
        input.is_brief_only = true;
        let layout = resolve_footer_notifications(&input);
        assert!(layout.token_warning_state.is_above_warning_threshold);
    }

    #[test]
    fn token_warning_collapse_mode_is_visible() {
        let mut input = base_notifications_input();
        input.token_warning.token_usage = 170_000;
        input.token_warning.collapse_mode = true;
        let layout = resolve_footer_notifications(&input);
        assert_eq!(layout.items.len(), 1);
        let FooterNotificationItem::TokenWarning(tw) = &layout.items[0];
        assert!(matches!(tw.mode, TokenWarningMode::Collapse { .. }));
    }

    #[test]
    fn token_warning_auto_compact_label_is_visible() {
        let mut input = base_notifications_input();
        input.token_warning.token_usage = 170_000;
        input.token_warning.show_auto_compact_warning = true;
        let layout = resolve_footer_notifications(&input);
        assert_eq!(layout.items.len(), 1);
        let FooterNotificationItem::TokenWarning(tw) = &layout.items[0];
        assert!(matches!(tw.mode, TokenWarningMode::AutoCompactLabel { .. }));
    }

    // ── effort notification ──────────────────────────────────────

    #[test]
    fn effort_notification_none_when_no_level() {
        let layout = resolve_effort_notification(EffortNotificationInput {
            effort_level: None,
            brief_owns_gap: false,
        });
        assert_eq!(layout.text, None);
    }

    #[test]
    fn effort_notification_suppressed_when_brief_owns_gap() {
        let layout = resolve_effort_notification(EffortNotificationInput {
            effort_level: Some(ReasoningEffort::High),
            brief_owns_gap: true,
        });
        assert_eq!(layout.text, None);
    }

    #[test]
    fn effort_notification_low() {
        let layout = resolve_effort_notification(EffortNotificationInput {
            effort_level: Some(ReasoningEffort::Low),
            brief_owns_gap: false,
        });
        let text = layout.text.expect("should have text");
        assert!(text.contains("low"));
        assert!(text.contains("/effort"));
    }

    #[test]
    fn effort_notification_medium() {
        let layout = resolve_effort_notification(EffortNotificationInput {
            effort_level: Some(ReasoningEffort::Medium),
            brief_owns_gap: false,
        });
        let text = layout.text.unwrap();
        assert!(text.contains("medium"));
        assert!(text.contains("/effort"));
    }

    #[test]
    fn effort_notification_high() {
        let layout = resolve_effort_notification(EffortNotificationInput {
            effort_level: Some(ReasoningEffort::High),
            brief_owns_gap: false,
        });
        let text = layout.text.unwrap();
        assert!(text.contains("high"));
        assert!(text.contains("/effort"));
    }

    #[test]
    fn effort_notification_xhigh() {
        let layout = resolve_effort_notification(EffortNotificationInput {
            effort_level: Some(ReasoningEffort::XHigh),
            brief_owns_gap: false,
        });
        let text = layout.text.unwrap();
        assert!(text.contains("xhigh"));
        assert!(text.contains("/effort"));
    }

    #[test]
    fn effort_notification_constants() {
        assert_eq!(EFFORT_NOTIFICATION_KEY, "effort-level");
        assert_eq!(EFFORT_NOTIFICATION_TIMEOUT_MS, 12_000);
    }

    // ── status line visibility ───────────────────────────────────

    fn base_status_line_input() -> StatusLineVisibilityInput {
        use crate::status::status_line::StatusLineConfig;
        StatusLineVisibilityInput {
            mode: "prompt".into(),
            has_height: true,
            exit_message_showing: false,
            is_pasting: false,
            settings: StatusLineSettings {
                status_line: Some(StatusLineConfig {
                    command: "echo status".into(),
                    padding: None,
                }),
            },
            assistant_mode_enabled: false,
            assistant_mode_active: false,
        }
    }

    #[test]
    fn status_line_visible_in_default_prompt_state() {
        assert!(resolve_status_line_visibility(&base_status_line_input()));
    }

    #[test]
    fn status_line_hidden_in_bash_mode() {
        let mut input = base_status_line_input();
        input.mode = "bash".into();
        assert!(!resolve_status_line_visibility(&input));
    }

    #[test]
    fn status_line_hidden_when_short_terminal() {
        let mut input = base_status_line_input();
        input.has_height = false;
        assert!(!resolve_status_line_visibility(&input));
    }

    #[test]
    fn status_line_hidden_when_exit_message_showing() {
        let mut input = base_status_line_input();
        input.exit_message_showing = true;
        assert!(!resolve_status_line_visibility(&input));
    }

    #[test]
    fn status_line_hidden_when_pasting() {
        let mut input = base_status_line_input();
        input.is_pasting = true;
        assert!(!resolve_status_line_visibility(&input));
    }

    #[test]
    fn status_line_hidden_when_no_status_line_config() {
        let mut input = base_status_line_input();
        input.settings.status_line = None;
        assert!(!resolve_status_line_visibility(&input));
    }

    #[test]
    fn status_line_hidden_when_assistant_mode_enabled_and_active() {
        let mut input = base_status_line_input();
        input.assistant_mode_enabled = true;
        input.assistant_mode_active = true;
        assert!(!resolve_status_line_visibility(&input));
    }

    #[test]
    fn status_line_visible_when_assistant_mode_enabled_but_not_active() {
        let mut input = base_status_line_input();
        input.assistant_mode_enabled = true;
        input.assistant_mode_active = false;
        assert!(resolve_status_line_visibility(&input));
    }

    #[test]
    fn status_line_visible_when_assistant_mode_active_but_not_enabled() {
        let mut input = base_status_line_input();
        input.assistant_mode_enabled = false;
        input.assistant_mode_active = true;
        assert!(resolve_status_line_visibility(&input));
    }

    #[test]
    fn status_line_requires_all_conditions_true() {
        assert!(resolve_status_line_visibility(&base_status_line_input()));

        for mutator in [
            |i: &mut StatusLineVisibilityInput| i.mode = "bash".into(),
            |i: &mut StatusLineVisibilityInput| i.has_height = false,
            |i: &mut StatusLineVisibilityInput| i.exit_message_showing = true,
            |i: &mut StatusLineVisibilityInput| i.is_pasting = true,
            |i: &mut StatusLineVisibilityInput| i.settings.status_line = None,
            |i: &mut StatusLineVisibilityInput| {
                i.assistant_mode_enabled = true;
                i.assistant_mode_active = true;
            },
        ] {
            let mut modified = base_status_line_input();
            mutator(&mut modified);
            assert!(
                !resolve_status_line_visibility(&modified),
                "should be hidden"
            );
        }
    }

    // ── composite: all areas ─────────────────────────────────────

    #[test]
    fn all_areas_resolve_independently() {
        let notifications = resolve_footer_notifications(&FooterNotificationsInput {
            token_warning: TokenWarningInputs {
                token_usage: 170_000,
                effective_context_window: 200_000,
                is_auto_compact_enabled: true,
                suppress_warning: false,
                show_auto_compact_warning: false,
                reactive_only_mode: false,
                collapse_mode: false,
                upgrade_message: None,
                blocking_limit_override: None,
            },
            is_brief_only: false,
        });

        let effort = resolve_effort_notification(EffortNotificationInput {
            effort_level: Some(ReasoningEffort::High),
            brief_owns_gap: false,
        });

        let status_line = resolve_status_line_visibility(&base_status_line_input());

        assert_eq!(notifications.items.len(), 1);
        assert!(effort.text.is_some());
        assert!(status_line);
    }
}
