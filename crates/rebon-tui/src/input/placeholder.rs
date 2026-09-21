//! Placeholder-decision pure function.
//!
//! The function determines two things:
//!
//! 1. **`show_placeholder`** — should the consumer render the
//!    placeholder instead of the value? True iff the value is empty
//!    and a placeholder is configured (non-empty).
//! 2. **`rendered_placeholder`** — what styled representation the
//!    consumer should draw:
//!    * `None` — no placeholder (or an empty-string placeholder).
//!    * `HiddenEmpty` — voice-recording mode with
//!      `hide_placeholder_text` set and no visible cursor (blank).
//!    * `HiddenCursorInverseSpace` — voice-recording mode with
//!      `hide_placeholder_text` set and a visible cursor (one
//!      inverse-video space).
//!    * `Dim { text }` — standard dim placeholder (no cursor).
//!    * `FirstCharInverseRestDim { first, rest }` — first character
//!      drawn as inverse-cursor, the rest dim. Used when cursor is
//!      visible.
//!
//! No styling is applied here: the caller receives owned `String`s
//! plus a [`PlaceholderStyle`] tag and decides how to draw them, so
//! this module stays free of any terminal-rendering dependency.
//!
//! The caller feeds it a [`PlaceholderInput`] built from the input's
//! placeholder text, current value, cursor and focus flags.

/// Inputs for [`render_placeholder`].
#[derive(Debug, Clone)]
pub struct PlaceholderInput<'a> {
    /// The configured placeholder string,
    /// or `None` if the consumer did not provide one.
    pub placeholder: Option<&'a str>,
    /// The current value of the text input.
    pub value: &'a str,
    /// Whether to show the terminal cursor at all.
    pub show_cursor: bool,
    /// Whether this input is focused inside the app.
    pub focus: bool,
    /// Whether the terminal window currently has OS-level focus.
    /// Defaults to `true`.
    pub terminal_focus: bool,
    /// Voice-recording short-circuit: suppress placeholder text but
    /// keep the cursor.
    pub hide_placeholder_text: bool,
}

impl<'a> Default for PlaceholderInput<'a> {
    fn default() -> Self {
        Self {
            placeholder: None,
            value: "",
            show_cursor: false,
            focus: false,
            terminal_focus: true,
            hide_placeholder_text: false,
        }
    }
}

/// Style tag describing what the renderer should draw for the
/// placeholder. The actual ANSI escapes live in the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceholderStyle {
    /// No placeholder at all (a no-op for the caller).
    None,
    /// Empty placeholder, no cursor: `hide_placeholder_text` is set
    /// but the cursor is not visible, so nothing at all is drawn.
    HiddenEmpty,
    /// Empty placeholder + a single inverse-space cursor: both
    /// `hide_placeholder_text` and the cursor-visibility conjunction
    /// (`show_cursor && focus && terminal_focus`) hold.
    HiddenCursorInverseSpace,
    /// Plain dim placeholder, no cursor overlay: the text is dimmed on
    /// its own, because `show_cursor && focus && terminal_focus` is
    /// false.
    Dim {
        /// The placeholder text the renderer should dim.
        text: String,
    },
    /// First char inverse, rest dim: `first` is the placeholder's
    /// first character and `rest` is everything after it.
    FirstCharInverseRestDim {
        /// First character of the placeholder (rendered inverse).
        first: char,
        /// Remainder of the placeholder after the first character
        /// (rendered dim). May be empty if the placeholder was a
        /// single character.
        rest: String,
    },
}

/// Complete output of [`render_placeholder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderDecision {
    /// Whether to display the placeholder at all. True iff `value` is
    /// empty and a non-empty placeholder is configured.
    pub show_placeholder: bool,
    /// What the renderer should draw for the placeholder itself.
    /// See [`PlaceholderStyle`].
    pub rendered_placeholder: PlaceholderStyle,
}

/// Compute the placeholder decision. Pure function.
pub fn render_placeholder(input: &PlaceholderInput<'_>) -> PlaceholderDecision {
    // An empty-string placeholder counts as "no placeholder", so
    // `Some("")` yields `show_placeholder = false`.
    let placeholder_truthy = matches!(input.placeholder, Some(s) if !s.is_empty());
    let show_placeholder = input.value.is_empty() && placeholder_truthy;
    let cursor_visible = input.show_cursor && input.focus && input.terminal_focus;

    // An empty-string placeholder skips rendering entirely and
    // `rendered_placeholder` stays `None`. We branch on
    // `placeholder_truthy` to implement that.
    let rendered_placeholder = if !placeholder_truthy {
        PlaceholderStyle::None
    } else {
        let placeholder = input.placeholder.expect("truthy branch");
        if input.hide_placeholder_text {
            if cursor_visible {
                PlaceholderStyle::HiddenCursorInverseSpace
            } else {
                PlaceholderStyle::HiddenEmpty
            }
        } else if cursor_visible {
            // We only reach this branch when `placeholder_truthy`
            // (non-empty), so we can unconditionally split off the
            // first char; the empty-string case is already excluded
            // by the outer `placeholder_truthy` check above.
            let mut chars = placeholder.chars();
            let first = chars.next().expect("placeholder_truthy => non-empty");
            let rest: String = chars.collect();
            PlaceholderStyle::FirstCharInverseRestDim { first, rest }
        } else {
            PlaceholderStyle::Dim {
                text: placeholder.to_string(),
            }
        }
    };

    PlaceholderDecision {
        show_placeholder,
        rendered_placeholder,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>() -> PlaceholderInput<'a> {
        PlaceholderInput::default()
    }

    // ------- show_placeholder decision ------------------------------------

    #[test]
    fn show_placeholder_false_when_value_non_empty() {
        let mut p = input();
        p.placeholder = Some("type here");
        p.value = "x";
        let d = render_placeholder(&p);
        assert!(!d.show_placeholder);
    }

    #[test]
    fn show_placeholder_false_when_no_placeholder_configured() {
        let p = input(); // value="" placeholder=None
        let d = render_placeholder(&p);
        assert!(!d.show_placeholder);
    }

    #[test]
    fn show_placeholder_true_when_value_empty_and_placeholder_set() {
        let mut p = input();
        p.placeholder = Some("type here");
        p.value = "";
        let d = render_placeholder(&p);
        assert!(d.show_placeholder);
    }

    #[test]
    fn show_placeholder_false_with_empty_string_placeholder() {
        // An empty-string placeholder counts as "no placeholder" —
        // show_placeholder is FALSE, and rendered_placeholder stays
        // None since the outer non-empty check also fails.
        let mut p = input();
        p.placeholder = Some("");
        p.value = "";
        let d = render_placeholder(&p);
        assert!(!d.show_placeholder);
        assert_eq!(d.rendered_placeholder, PlaceholderStyle::None);
    }

    // ------- rendered_placeholder variants -------------------------------

    #[test]
    fn rendered_none_when_no_placeholder() {
        let p = input();
        let d = render_placeholder(&p);
        assert_eq!(d.rendered_placeholder, PlaceholderStyle::None);
    }

    #[test]
    fn rendered_dim_when_cursor_invisible() {
        let mut p = input();
        p.placeholder = Some("hello");
        // show_cursor=false -> cursor invisible -> dim-only
        let d = render_placeholder(&p);
        assert_eq!(
            d.rendered_placeholder,
            PlaceholderStyle::Dim {
                text: "hello".into()
            }
        );
    }

    #[test]
    fn rendered_dim_when_not_focused() {
        let mut p = input();
        p.placeholder = Some("hello");
        p.show_cursor = true;
        p.focus = false;
        p.terminal_focus = true;
        let d = render_placeholder(&p);
        assert_eq!(
            d.rendered_placeholder,
            PlaceholderStyle::Dim {
                text: "hello".into()
            }
        );
    }

    #[test]
    fn rendered_dim_when_terminal_unfocused() {
        let mut p = input();
        p.placeholder = Some("hello");
        p.show_cursor = true;
        p.focus = true;
        p.terminal_focus = false;
        let d = render_placeholder(&p);
        assert_eq!(
            d.rendered_placeholder,
            PlaceholderStyle::Dim {
                text: "hello".into()
            }
        );
    }

    #[test]
    fn rendered_first_inverse_rest_dim_when_cursor_visible() {
        let mut p = input();
        p.placeholder = Some("hello");
        p.show_cursor = true;
        p.focus = true;
        p.terminal_focus = true;
        let d = render_placeholder(&p);
        assert_eq!(
            d.rendered_placeholder,
            PlaceholderStyle::FirstCharInverseRestDim {
                first: 'h',
                rest: "ello".into(),
            }
        );
    }

    #[test]
    fn rendered_first_char_with_single_char_placeholder() {
        let mut p = input();
        p.placeholder = Some("x");
        p.show_cursor = true;
        p.focus = true;
        let d = render_placeholder(&p);
        assert_eq!(
            d.rendered_placeholder,
            PlaceholderStyle::FirstCharInverseRestDim {
                first: 'x',
                rest: "".into(),
            }
        );
    }

    #[test]
    fn rendered_first_char_respects_multibyte_cjk() {
        let mut p = input();
        p.placeholder = Some("日本語");
        p.show_cursor = true;
        p.focus = true;
        let d = render_placeholder(&p);
        assert_eq!(
            d.rendered_placeholder,
            PlaceholderStyle::FirstCharInverseRestDim {
                first: '日',
                rest: "本語".into(),
            }
        );
    }

    #[test]
    fn rendered_none_when_placeholder_is_empty_string() {
        // The outer non-empty check gates entry, so an empty-string
        // placeholder is not entered at all and `rendered_placeholder`
        // stays `None`; the first-char/rest split below never runs
        // for an empty placeholder.
        let mut p = input();
        p.placeholder = Some("");
        p.show_cursor = true;
        p.focus = true;
        let d = render_placeholder(&p);
        assert_eq!(d.rendered_placeholder, PlaceholderStyle::None);
    }

    // ------- hide_placeholder_text branch (voice recording) ----------------

    #[test]
    fn hide_placeholder_text_with_cursor_gives_inverse_space() {
        let mut p = input();
        p.placeholder = Some("ignored");
        p.hide_placeholder_text = true;
        p.show_cursor = true;
        p.focus = true;
        let d = render_placeholder(&p);
        assert_eq!(
            d.rendered_placeholder,
            PlaceholderStyle::HiddenCursorInverseSpace
        );
    }

    #[test]
    fn hide_placeholder_text_without_cursor_gives_hidden_empty() {
        let mut p = input();
        p.placeholder = Some("ignored");
        p.hide_placeholder_text = true;
        p.show_cursor = false;
        let d = render_placeholder(&p);
        assert_eq!(d.rendered_placeholder, PlaceholderStyle::HiddenEmpty);
    }

    #[test]
    fn hide_placeholder_text_with_unfocused_input_gives_hidden_empty() {
        let mut p = input();
        p.placeholder = Some("ignored");
        p.hide_placeholder_text = true;
        p.show_cursor = true;
        p.focus = false;
        let d = render_placeholder(&p);
        assert_eq!(d.rendered_placeholder, PlaceholderStyle::HiddenEmpty);
    }

    #[test]
    fn hide_placeholder_text_with_unfocused_terminal_gives_hidden_empty() {
        let mut p = input();
        p.placeholder = Some("ignored");
        p.hide_placeholder_text = true;
        p.show_cursor = true;
        p.focus = true;
        p.terminal_focus = false;
        let d = render_placeholder(&p);
        assert_eq!(d.rendered_placeholder, PlaceholderStyle::HiddenEmpty);
    }

    // ------- combined show_placeholder + rendered_placeholder -----------

    #[test]
    fn value_non_empty_still_builds_rendered_placeholder() {
        // rendered_placeholder is always built regardless of whether
        // show_placeholder will be true — only `show_placeholder`
        // gates the display.
        let mut p = input();
        p.placeholder = Some("hint");
        p.value = "some text";
        p.show_cursor = true;
        p.focus = true;
        let d = render_placeholder(&p);
        assert!(!d.show_placeholder); // because value non-empty
        assert_eq!(
            d.rendered_placeholder,
            PlaceholderStyle::FirstCharInverseRestDim {
                first: 'h',
                rest: "int".into(),
            }
        );
    }
}
