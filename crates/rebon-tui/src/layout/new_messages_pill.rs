//! "New messages" pill label projection.
//!
//! The pill is a small control that shows:
//!
//! * `"Jump to bottom"` when the count is 0
//! * `"N new message"` / `"N new messages"` for a count of 1 / more
//!   than 1 (via [`plural_message`])
//!
//! The caller lays the label and the down arrow out itself — the wired call
//! site renders `" {arrow} {label} "` and passes `hover = false`.
//!
//! We expose [`pill_label`] (text only), [`PillDisplay`] (label + arrow +
//! background), and the [`PillBackground`] enum for the two
//! states. Click and hover handling stay with the consumer — the display
//! struct carries only what's needed to render.

/// Background color names, mirroring the two theme tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillBackground {
    /// `"userMessageBackground"` — default.
    Default,
    /// `"userMessageBackgroundHover"` — hover state.
    Hover,
}

impl PillBackground {
    pub fn token(self) -> &'static str {
        match self {
            Self::Default => "userMessageBackground",
            Self::Hover => "userMessageBackgroundHover",
        }
    }
}

/// The pill display shape: label text, arrow glyph, background state, and
/// the dim flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PillDisplay {
    pub background: PillBackground,
    pub label: String,
    /// The arrow glyph (`"↓"`). Pinned here; the
    /// consumer can substitute a non-unicode fallback if needed.
    pub arrow: &'static str,
    /// Whether the pill dim-colors its text (always `true` today).
    pub dim: bool,
}

/// Default down-arrow glyph — `"↓"` (U+2193).
pub const ARROW_DOWN: &str = "\u{2193}";

/// Singular `"message"` for a count of exactly 1, plural
/// `"messages"` for any other count, including 0.
pub fn plural_message(count: u64) -> &'static str {
    if count == 1 {
        "message"
    } else {
        "messages"
    }
}

/// Label projection. `0 → "Jump to bottom"`, otherwise `"N new <word>"`.
pub fn pill_label(count: u64) -> String {
    if count == 0 {
        "Jump to bottom".to_string()
    } else {
        format!("{count} new {}", plural_message(count))
    }
}

/// Full display projection. `hover = true` → Hover background.
pub fn project_pill(count: u64, hover: bool) -> PillDisplay {
    PillDisplay {
        background: if hover {
            PillBackground::Hover
        } else {
            PillBackground::Default
        },
        label: pill_label(count),
        arrow: ARROW_DOWN,
        dim: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_zero_is_jump_to_bottom() {
        assert_eq!(pill_label(0), "Jump to bottom");
    }

    #[test]
    fn label_one_is_singular() {
        assert_eq!(pill_label(1), "1 new message");
    }

    #[test]
    fn label_two_is_plural() {
        assert_eq!(pill_label(2), "2 new messages");
    }

    #[test]
    fn label_large_is_plural() {
        assert_eq!(pill_label(999), "999 new messages");
    }

    #[test]
    fn plural_message_singular_at_one() {
        assert_eq!(plural_message(1), "message");
    }

    #[test]
    fn plural_message_plural_at_zero() {
        // A count of 0 yields "messages": only exactly 1 is singular.
        // Pinned even though the label never reaches the helper with 0.
        assert_eq!(plural_message(0), "messages");
    }

    #[test]
    fn plural_message_plural_at_many() {
        assert_eq!(plural_message(5), "messages");
    }

    #[test]
    fn arrow_glyph_is_u2193() {
        assert_eq!(ARROW_DOWN, "\u{2193}");
    }

    #[test]
    fn hover_picks_hover_background() {
        let d = project_pill(1, true);
        assert_eq!(d.background, PillBackground::Hover);
        assert_eq!(d.background.token(), "userMessageBackgroundHover");
    }

    #[test]
    fn default_picks_default_background() {
        let d = project_pill(1, false);
        assert_eq!(d.background, PillBackground::Default);
        assert_eq!(d.background.token(), "userMessageBackground");
    }

    #[test]
    fn display_is_always_dim() {
        let d = project_pill(0, false);
        assert!(d.dim);
    }

    #[test]
    fn display_label_routes_through_pill_label() {
        assert_eq!(project_pill(0, false).label, "Jump to bottom");
        assert_eq!(project_pill(7, false).label, "7 new messages");
    }
}
