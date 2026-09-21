//! Static "press Enter to continue" text.
//!
//! The consumer renders the literal copy "Press **Enter** to continue…"
//! with the permission color. There is no state and no formatting; the
//! row is a constant. We pin it as a `pub const` so the consumer can
//! render it however its terminal layer wants.

/// The exact display text of the prompt. The trailing single-character
/// ellipsis (U+2026) is part of the pinned copy.
pub const PRESS_ENTER_TO_CONTINUE: &str = "Press Enter to continue\u{2026}";

/// Returns the [`PRESS_ENTER_TO_CONTINUE`] text. Function form is
/// provided so call sites can read it as a getter even if the constant
/// is later promoted to a function (e.g. localization).
pub fn press_enter_to_continue_text() -> &'static str {
    PRESS_ENTER_TO_CONTINUE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_pinned() {
        // The text uses
        // the single-character `…` (U+2026), not three dots.
        assert_eq!(
            press_enter_to_continue_text(),
            "Press Enter to continue\u{2026}"
        );
    }

    #[test]
    fn ends_with_ellipsis_not_three_dots() {
        let text = press_enter_to_continue_text();
        assert!(text.ends_with('\u{2026}'));
        assert!(!text.ends_with("..."));
    }

    #[test]
    fn contains_enter_word() {
        // The literal word "Enter" is bolded.
        assert!(press_enter_to_continue_text().contains("Enter"));
    }

    #[test]
    fn ascii_prefix_is_press_space() {
        let text = press_enter_to_continue_text();
        assert!(text.starts_with("Press "));
    }
}
