//! Effort-level glyphs and the `/effort` notification text.
//!
//! [`effort_level_to_symbol`] maps each [`ReasoningEffort`] to the glyph
//! defined here; [`get_effort_notification_text`] renders the
//! `"<level> · /effort"` toast, or `None` when the model has no effort knob.
//!
//! The level type is [`ReasoningEffort`], not a local enum, so the picker, the
//! config, and the indicator share one vocabulary. Whether a model supports
//! effort, and which level is shown, is the consumer's decision: it passes the
//! resolved level (or `None`) in.

use crate::ReasoningEffort;

/// `EFFORT_LOW` glyph. U+25CB.
pub const EFFORT_LOW: &str = "\u{25cb}";
/// `EFFORT_MEDIUM` glyph. U+25D0.
pub const EFFORT_MEDIUM: &str = "\u{25d0}";
/// `EFFORT_HIGH` glyph. U+25CF.
pub const EFFORT_HIGH: &str = "\u{25cf}";
/// `EFFORT_XHIGH` glyph. U+25C9.
pub const EFFORT_XHIGH: &str = "\u{25c9}";
/// `EFFORT_MAX` glyph — gpt-5.6+ max effort. U+2B24.
pub const EFFORT_MAX: &str = "\u{2b24}";

/// Glyph for an effort level. [`ReasoningEffort`] is exhaustive, so
/// there is no fallback arm; consumers should map unknown strings to
/// `ReasoningEffort::High` before calling.
pub fn effort_level_to_symbol(level: ReasoningEffort) -> &'static str {
    match level {
        ReasoningEffort::Low => EFFORT_LOW,
        ReasoningEffort::Medium => EFFORT_MEDIUM,
        ReasoningEffort::High => EFFORT_HIGH,
        ReasoningEffort::XHigh => EFFORT_XHIGH,
        ReasoningEffort::Max => EFFORT_MAX,
    }
}

/// Provider family that determines user-facing vocabulary.
///
/// Anthropic models use "effort" (`/effort`), OpenAI-compatible models
/// use "thinking" (`/effort` still, but labelled "thinking level").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffortProviderKind {
    /// Anthropic first-party — uses "effort" vocabulary.
    Anthropic,
    /// OpenAI-compatible (chat completions or responses API) — uses
    /// "thinking" vocabulary.
    OpenAi,
}

impl EffortProviderKind {
    /// User-facing label for the concept: `"effort"` or `"thinking"`.
    pub fn label(self) -> &'static str {
        match self {
            EffortProviderKind::Anthropic => "effort",
            EffortProviderKind::OpenAi => "thinking",
        }
    }
}

/// Effort toast text. Takes the *resolved* level wrapped in `Option`
/// (the consumer decides whether the model supports effort). Returns:
///
/// * `None` if the model has no effort support.
/// * `Some("<level> · /effort")` otherwise.
pub fn get_effort_notification_text(level: Option<ReasoningEffort>) -> Option<String> {
    let level = level?;
    Some(format!("{} \u{00b7} /effort", level.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_low_is_open_circle() {
        assert_eq!(effort_level_to_symbol(ReasoningEffort::Low), "\u{25cb}");
    }

    #[test]
    fn symbol_medium_is_half_circle() {
        assert_eq!(effort_level_to_symbol(ReasoningEffort::Medium), "\u{25d0}");
    }

    #[test]
    fn symbol_high_is_filled_circle() {
        assert_eq!(effort_level_to_symbol(ReasoningEffort::High), "\u{25cf}");
    }

    #[test]
    fn symbol_xhigh_is_fisheye() {
        assert_eq!(effort_level_to_symbol(ReasoningEffort::XHigh), "\u{25c9}");
    }

    #[test]
    fn notification_low() {
        assert_eq!(
            get_effort_notification_text(Some(ReasoningEffort::Low)),
            Some("low \u{00b7} /effort".into())
        );
    }

    #[test]
    fn notification_medium() {
        assert_eq!(
            get_effort_notification_text(Some(ReasoningEffort::Medium)),
            Some("medium \u{00b7} /effort".into())
        );
    }

    #[test]
    fn notification_high() {
        assert_eq!(
            get_effort_notification_text(Some(ReasoningEffort::High)),
            Some("high \u{00b7} /effort".into())
        );
    }

    #[test]
    fn notification_xhigh() {
        assert_eq!(
            get_effort_notification_text(Some(ReasoningEffort::XHigh)),
            Some("xhigh \u{00b7} /effort".into())
        );
    }

    #[test]
    fn notification_returns_none_when_unsupported() {
        assert_eq!(get_effort_notification_text(None), None);
    }

    #[test]
    fn label_text_matches_lowercase() {
        assert_eq!(ReasoningEffort::Low.as_str(), "low");
        assert_eq!(ReasoningEffort::Medium.as_str(), "medium");
        assert_eq!(ReasoningEffort::High.as_str(), "high");
        assert_eq!(ReasoningEffort::XHigh.as_str(), "xhigh");
    }

    #[test]
    fn notification_uses_middle_dot_separator() {
        let text = get_effort_notification_text(Some(ReasoningEffort::High)).unwrap();
        assert!(text.contains(" \u{00b7} "));
    }

    #[test]
    fn notification_ends_with_slash_effort() {
        let text = get_effort_notification_text(Some(ReasoningEffort::Medium)).unwrap();
        assert!(text.ends_with("/effort"));
    }

    #[test]
    fn glyph_constants_are_distinct() {
        // Defensive: a copy-paste during a model launch could collapse
        // two glyphs into one. Pin them.
        let glyphs = [
            EFFORT_LOW,
            EFFORT_MEDIUM,
            EFFORT_HIGH,
            EFFORT_XHIGH,
            EFFORT_MAX,
        ];
        for (i, a) in glyphs.iter().enumerate() {
            for (j, b) in glyphs.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }
}
