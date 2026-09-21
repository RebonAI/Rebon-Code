//! Context-window pressure badge.
//!
//! The projection reads the current token usage plus the effective
//! context window, computes a [`TokenWarningState`], picks one of four
//! display modes based on the caller-supplied feature flags, and emits a
//! single warning row.
//!
//! ## Pure logic covered here
//!
//! 1. The buffer-token constants (`AUTOCOMPACT`, `WARNING`, `ERROR`,
//!    `MANUAL_COMPACT`).
//! 2. [`calculate_token_warning_state`] — the percent-left, above
//!    warning, above error, above auto-compact, and at-blocking-limit
//!    computations.
//! 3. [`token_warning_layout`] — the dispatch into the four display
//!    branches:
//!    * **Hidden** when below warning or suppressed.
//!    * **Collapse mode** placeholder.
//!    * **Reactive-only / autocompact label**: dim text with
//!      `"<X>% context used"` or `"<Y>% until auto-compact"`.
//!    * **Standard warning**: themed text with
//!      `"Context low (<percent_left>% remaining) · Run /compact to
//!      compact & continue"` (or with the upgrade message suffix).

/// Tokens kept free below the context window before auto-compact triggers.
pub const AUTOCOMPACT_BUFFER_TOKENS: u64 = 13_000;
/// The warning shows once usage is within this many tokens of the threshold.
pub const WARNING_THRESHOLD_BUFFER_TOKENS: u64 = 20_000;
/// The error state starts once usage is within this many tokens of the threshold.
pub const ERROR_THRESHOLD_BUFFER_TOKENS: u64 = 20_000;
/// Usage within this many tokens of the context window is at the blocking limit.
pub const MANUAL_COMPACT_BUFFER_TOKENS: u64 = 3_000;

/// Result of [`calculate_token_warning_state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenWarningState {
    /// Percentage of the threshold still free:
    /// `max(0, round((threshold - token_usage) / threshold * 100))`.
    pub percent_left: u32,
    /// `token_usage >= threshold - WARNING_THRESHOLD_BUFFER_TOKENS`.
    pub is_above_warning_threshold: bool,
    /// `token_usage >= threshold - ERROR_THRESHOLD_BUFFER_TOKENS`.
    pub is_above_error_threshold: bool,
    /// `is_auto_compact_enabled && token_usage >= auto_compact_threshold`.
    pub is_above_auto_compact_threshold: bool,
    /// `token_usage >= effective_context_window -
    /// MANUAL_COMPACT_BUFFER_TOKENS`, unless a blocking-limit override
    /// supplies a different limit (the caller resolves that).
    pub is_at_blocking_limit: bool,
}

/// Compute the warning state. The consumer pre-resolves the effective
/// context window (from the model plus the active SDK beta flags) and
/// whether auto-compact is enabled. Optional `blocking_limit_override`
/// replaces the computed blocking limit.
pub fn calculate_token_warning_state(
    token_usage: u64,
    effective_context_window: u64,
    is_auto_compact_enabled: bool,
    blocking_limit_override: Option<u64>,
) -> TokenWarningState {
    let auto_compact_threshold = effective_context_window.saturating_sub(AUTOCOMPACT_BUFFER_TOKENS);
    let threshold = if is_auto_compact_enabled {
        auto_compact_threshold
    } else {
        effective_context_window
    };

    // percent_left = max(0, round((threshold - token_usage) / threshold * 100))
    let percent_left = if threshold == 0 {
        0
    } else if token_usage >= threshold {
        0
    } else {
        let pct = ((threshold - token_usage) as f64 / threshold as f64) * 100.0;
        // `f64::round` rounds half away from zero, which is the right
        // behaviour for this non-negative percentage.
        pct.round().max(0.0) as u32
    };

    let warning_threshold = threshold.saturating_sub(WARNING_THRESHOLD_BUFFER_TOKENS);
    let error_threshold = threshold.saturating_sub(ERROR_THRESHOLD_BUFFER_TOKENS);

    let is_above_warning_threshold = token_usage >= warning_threshold;
    let is_above_error_threshold = token_usage >= error_threshold;

    let is_above_auto_compact_threshold =
        is_auto_compact_enabled && token_usage >= auto_compact_threshold;

    let default_blocking_limit =
        effective_context_window.saturating_sub(MANUAL_COMPACT_BUFFER_TOKENS);
    let blocking_limit = blocking_limit_override.unwrap_or(default_blocking_limit);
    let is_at_blocking_limit = token_usage >= blocking_limit;

    TokenWarningState {
        percent_left,
        is_above_warning_threshold,
        is_above_error_threshold,
        is_above_auto_compact_threshold,
        is_at_blocking_limit,
    }
}

/// Inputs the layout reads. The consumer pre-resolves all flags so the
/// projection is pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenWarningInputs {
    /// Current token usage.
    pub token_usage: u64,
    /// Pre-resolved effective context window for the model.
    pub effective_context_window: u64,
    /// Whether auto-compact is enabled.
    pub is_auto_compact_enabled: bool,
    /// Whether the user has currently suppressed the warning.
    pub suppress_warning: bool,
    /// Whether the auto-compact warning is shown when above the warning
    /// threshold. The consumer caches this decision and feeds it as an
    /// explicit flag.
    pub show_auto_compact_warning: bool,
    /// Switches the percent calculation to `100 - display_percent_left`;
    /// the consumer resolves the feature flags behind it.
    pub reactive_only_mode: bool,
    /// Switches to the [`TokenWarningMode::Collapse`] branch; the
    /// consumer resolves the feature flags behind it.
    pub collapse_mode: bool,
    /// Optional pre-resolved upgrade message, shown as a suffix when
    /// present.
    pub upgrade_message: Option<String>,
    /// Optional override for the blocking limit.
    pub blocking_limit_override: Option<u64>,
}

/// Color slot for the rendered row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenWarningRowColor {
    /// Dimmed — used for the auto-compact label and the collapse-progress
    /// label.
    Dim,
    /// `'warning'` theme key.
    Warning,
    /// `'error'` theme key.
    Error,
}

/// Top-level layout mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenWarningMode {
    /// Hidden (below threshold or suppressed).
    Hidden,
    /// Collapse-mode placeholder. The label itself belongs to the
    /// consumer, which renders it from its own collapse state; we surface
    /// the branch with the resolved upgrade message.
    Collapse {
        /// Pre-resolved upgrade message (may be `None`).
        upgrade_message: Option<String>,
    },
    /// Auto-compact label (dim, no warning color).
    AutoCompactLabel {
        /// Pre-formatted label text.
        text: String,
    },
    /// Standard "Context low" warning row.
    ContextLow {
        /// Pre-formatted text.
        text: String,
        /// Color for the row.
        color: TokenWarningRowColor,
    },
}

/// Pre-built layout shape returned by [`token_warning_layout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenWarningLayout {
    /// Resolved warning state for downstream wiring.
    pub state: TokenWarningState,
    /// Branch decision.
    pub mode: TokenWarningMode,
}

/// Build the warning layout from the pre-resolved inputs.
pub fn token_warning_layout(inputs: &TokenWarningInputs) -> TokenWarningLayout {
    let state = calculate_token_warning_state(
        inputs.token_usage,
        inputs.effective_context_window,
        inputs.is_auto_compact_enabled,
        inputs.blocking_limit_override,
    );

    if !state.is_above_warning_threshold || inputs.suppress_warning {
        return TokenWarningLayout {
            state,
            mode: TokenWarningMode::Hidden,
        };
    }

    if inputs.collapse_mode {
        return TokenWarningLayout {
            state,
            mode: TokenWarningMode::Collapse {
                upgrade_message: inputs.upgrade_message.clone(),
            },
        };
    }

    // The reactive-only mode overrides the displayed percentage (collapse
    // mode would too, if it hadn't already returned above): recompute
    // percent_left from the actual context window instead of the
    // auto-compact-adjusted one.
    let display_percent_left = if inputs.reactive_only_mode {
        let window = inputs.effective_context_window;
        let pct = if window == 0 || inputs.token_usage >= window {
            0
        } else {
            (((window - inputs.token_usage) as f64 / window as f64) * 100.0)
                .round()
                .max(0.0) as u32
        };
        pct
    } else {
        state.percent_left
    };

    if inputs.show_auto_compact_warning {
        let label = if inputs.reactive_only_mode {
            format!(
                "{}% context used",
                100u32.saturating_sub(display_percent_left)
            )
        } else {
            format!("{display_percent_left}% until auto-compact")
        };
        let text = match &inputs.upgrade_message {
            Some(msg) => format!("{label} \u{00b7} {msg}"),
            None => label,
        };
        return TokenWarningLayout {
            state,
            mode: TokenWarningMode::AutoCompactLabel { text },
        };
    }

    let color = if state.is_above_error_threshold {
        TokenWarningRowColor::Error
    } else {
        TokenWarningRowColor::Warning
    };

    let text = match &inputs.upgrade_message {
        Some(msg) => format!(
            "Context low ({}% remaining) \u{00b7} {msg}",
            state.percent_left
        ),
        None => format!(
            "Context low ({}% remaining) \u{00b7} Run /compact to compact & continue",
            state.percent_left
        ),
    };

    TokenWarningLayout {
        state,
        mode: TokenWarningMode::ContextLow { text, color },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_inputs(token_usage: u64) -> TokenWarningInputs {
        TokenWarningInputs {
            token_usage,
            effective_context_window: 200_000,
            is_auto_compact_enabled: true,
            suppress_warning: false,
            show_auto_compact_warning: false,
            reactive_only_mode: false,
            collapse_mode: false,
            upgrade_message: None,
            blocking_limit_override: None,
        }
    }

    #[test]
    fn percent_left_zero_at_full_context() {
        let s = calculate_token_warning_state(200_000, 200_000, true, None);
        assert_eq!(s.percent_left, 0);
    }

    #[test]
    fn percent_left_high_at_low_usage() {
        // 1k tokens against 200k window with autocompact enabled:
        // threshold = 200k - 13k = 187k; pct_left = round((187 - 1)/187 * 100)
        let s = calculate_token_warning_state(1_000, 200_000, true, None);
        assert!(s.percent_left >= 99);
    }

    #[test]
    fn warning_threshold_below_is_false() {
        // threshold = 187k; warning = 187k - 20k = 167k; below 167k → false
        let s = calculate_token_warning_state(160_000, 200_000, true, None);
        assert!(!s.is_above_warning_threshold);
    }

    #[test]
    fn warning_threshold_at_is_true() {
        let s = calculate_token_warning_state(167_000, 200_000, true, None);
        assert!(s.is_above_warning_threshold);
    }

    #[test]
    fn warning_threshold_above_is_true() {
        let s = calculate_token_warning_state(170_000, 200_000, true, None);
        assert!(s.is_above_warning_threshold);
    }

    #[test]
    fn error_threshold_matches_warning_when_buffers_equal() {
        // WARNING_THRESHOLD_BUFFER_TOKENS == ERROR_THRESHOLD_BUFFER_TOKENS
        // == 20_000.
        let s = calculate_token_warning_state(167_000, 200_000, true, None);
        assert_eq!(s.is_above_error_threshold, s.is_above_warning_threshold);
    }

    #[test]
    fn auto_compact_threshold_only_when_enabled() {
        // threshold = 187k; token_usage = 187k → above
        let s = calculate_token_warning_state(187_000, 200_000, true, None);
        assert!(s.is_above_auto_compact_threshold);
        // disabled → never above
        let s = calculate_token_warning_state(187_000, 200_000, false, None);
        assert!(!s.is_above_auto_compact_threshold);
    }

    #[test]
    fn blocking_limit_at_default() {
        // default = 200k - 3k = 197k
        let s = calculate_token_warning_state(196_999, 200_000, true, None);
        assert!(!s.is_at_blocking_limit);
        let s = calculate_token_warning_state(197_000, 200_000, true, None);
        assert!(s.is_at_blocking_limit);
    }

    #[test]
    fn blocking_limit_override_takes_precedence() {
        let s = calculate_token_warning_state(150_000, 200_000, true, Some(150_000));
        assert!(s.is_at_blocking_limit);
        let s = calculate_token_warning_state(149_000, 200_000, true, Some(150_000));
        assert!(!s.is_at_blocking_limit);
    }

    #[test]
    fn percent_left_clamps_at_zero_when_over_threshold() {
        let s = calculate_token_warning_state(250_000, 200_000, true, None);
        assert_eq!(s.percent_left, 0);
    }

    #[test]
    fn layout_hidden_when_below_warning_threshold() {
        let layout = token_warning_layout(&base_inputs(100_000));
        assert_eq!(layout.mode, TokenWarningMode::Hidden);
    }

    #[test]
    fn layout_hidden_when_suppressed() {
        let mut inputs = base_inputs(170_000);
        inputs.suppress_warning = true;
        let layout = token_warning_layout(&inputs);
        assert_eq!(layout.mode, TokenWarningMode::Hidden);
    }

    #[test]
    fn layout_shows_context_low_warning() {
        let layout = token_warning_layout(&base_inputs(170_000));
        match &layout.mode {
            TokenWarningMode::ContextLow { text, color } => {
                assert!(text.contains("Context low"));
                assert!(text.contains("Run /compact"));
                // 170k → percent_left ≈ 9
                assert_eq!(*color, TokenWarningRowColor::Error); // also above error
            }
            other => panic!("expected ContextLow, got {other:?}"),
        }
    }

    #[test]
    fn layout_uses_warning_color_when_below_error_threshold() {
        // "Above warning but below error" is unreachable: the two buffers
        // are equal, so the thresholds coincide and no token count can
        // land between them. We pin the documented behaviour: once any
        // threshold is crossed, the row is error colored.
        let layout = token_warning_layout(&base_inputs(167_000));
        match layout.mode {
            TokenWarningMode::ContextLow { color, .. } => {
                assert_eq!(color, TokenWarningRowColor::Error);
            }
            other => panic!("expected ContextLow, got {other:?}"),
        }
    }

    #[test]
    fn layout_includes_upgrade_message_suffix() {
        let mut inputs = base_inputs(170_000);
        inputs.upgrade_message = Some("upgrade plz".into());
        let layout = token_warning_layout(&inputs);
        match layout.mode {
            TokenWarningMode::ContextLow { text, .. } => {
                assert!(text.contains("upgrade plz"));
                assert!(!text.contains("Run /compact"));
            }
            other => panic!("expected ContextLow, got {other:?}"),
        }
    }

    #[test]
    fn layout_collapse_mode_returns_collapse_branch() {
        let mut inputs = base_inputs(170_000);
        inputs.collapse_mode = true;
        let layout = token_warning_layout(&inputs);
        match layout.mode {
            TokenWarningMode::Collapse { upgrade_message } => {
                assert_eq!(upgrade_message, None);
            }
            other => panic!("expected Collapse, got {other:?}"),
        }
    }

    #[test]
    fn layout_collapse_mode_carries_upgrade_message() {
        let mut inputs = base_inputs(170_000);
        inputs.collapse_mode = true;
        inputs.upgrade_message = Some("upgrade".into());
        let layout = token_warning_layout(&inputs);
        match layout.mode {
            TokenWarningMode::Collapse { upgrade_message } => {
                assert_eq!(upgrade_message.as_deref(), Some("upgrade"));
            }
            other => panic!("expected Collapse, got {other:?}"),
        }
    }

    #[test]
    fn layout_show_auto_compact_warning_is_dim_label() {
        let mut inputs = base_inputs(170_000);
        inputs.show_auto_compact_warning = true;
        let layout = token_warning_layout(&inputs);
        match layout.mode {
            TokenWarningMode::AutoCompactLabel { text } => {
                assert!(text.contains("until auto-compact"));
            }
            other => panic!("expected AutoCompactLabel, got {other:?}"),
        }
    }

    #[test]
    fn layout_reactive_only_inverts_label() {
        let mut inputs = base_inputs(170_000);
        inputs.show_auto_compact_warning = true;
        inputs.reactive_only_mode = true;
        let layout = token_warning_layout(&inputs);
        match layout.mode {
            TokenWarningMode::AutoCompactLabel { text } => {
                assert!(text.contains("context used"));
                assert!(!text.contains("until auto-compact"));
            }
            other => panic!("expected AutoCompactLabel, got {other:?}"),
        }
    }

    #[test]
    fn layout_auto_compact_label_includes_upgrade_suffix() {
        let mut inputs = base_inputs(170_000);
        inputs.show_auto_compact_warning = true;
        inputs.upgrade_message = Some("upgrade".into());
        let layout = token_warning_layout(&inputs);
        match layout.mode {
            TokenWarningMode::AutoCompactLabel { text } => {
                assert!(text.contains("upgrade"));
                assert!(text.contains('\u{00b7}'));
            }
            other => panic!("expected AutoCompactLabel, got {other:?}"),
        }
    }

    #[test]
    fn buffer_constants_are_pinned() {
        assert_eq!(AUTOCOMPACT_BUFFER_TOKENS, 13_000);
        assert_eq!(WARNING_THRESHOLD_BUFFER_TOKENS, 20_000);
        assert_eq!(ERROR_THRESHOLD_BUFFER_TOKENS, 20_000);
        assert_eq!(MANUAL_COMPACT_BUFFER_TOKENS, 3_000);
    }

    #[test]
    fn layout_state_is_attached_for_consumer_inspection() {
        let layout = token_warning_layout(&base_inputs(170_000));
        assert!(layout.state.is_above_warning_threshold);
    }
}
