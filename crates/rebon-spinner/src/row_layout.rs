//! Progressive width-gating layout for the spinner status line.
//!
//! A sequence of optional pieces has to fit into the available
//! terminal width:
//!
//! 1. The spinner glyph (always present, takes 2 cells).
//! 2. The shimmered message (always present, takes the message width
//! plus 2 cells with an extra trailing space).
//! 3. `(thinking · 12s · 1.2k tokens)` — three optional pieces
//! separated by ` · `, each shown only if it fits.
//!
//! The order of decisions:
//!
//! 1. `available_space = columns - message_width - 5`. The 5 = 2 (glyph)
//! + 1 (space after glyph) + 1 (space after message) + 1 (paren).
//! 2. The thinking piece if `wants_thinking` and `available_space >
//! `thinking_width`.
//! 3. If thinking doesn't fit but thinking is active and there's
//! an effort suffix, try the bare "thinking" (without effort) at
//! `THINKING_BARE_WIDTH` cells.
//! 4. The timer if `wants_timer_and_tokens` and `available_space >
//! `used_after_thinking + timer_width`.
//! 5. The token count if `wants_timer_and_tokens` and `total_tokens > 0`
//! and it fits past the timer.
//!
//! Every fit test is a strict `>`, never `>=`.

/// The value of `THINKING_BARE_WIDTH`: the visual width of just
/// `"thinking"`, 8 cells.
pub const THINKING_BARE_WIDTH: usize = 8;

/// The visual width of the ` · ` separator: 3 cells.
const SEP_WIDTH: usize = 3;

/// The result of [`progressive_status_layout`]: which pieces are
/// shown and the (possibly shrunk) `thinking_text` width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowLayout {
    /// Whether the thinking text is rendered.
    pub show_thinking: bool,
    /// Whether the elapsed-time text is rendered.
    pub show_timer: bool,
    /// Whether the token-count text is rendered.
    pub show_tokens: bool,
    /// True when only `thinking` is shown — the consumer wraps it in
    /// extra parens.
    pub thinking_only: bool,
    /// The width the thinking text actually consumes after a possible
    /// shrink. Equal to the input `thinking_width_value` unless we
    /// shrunk to `THINKING_BARE_WIDTH`.
    pub thinking_text_width: usize,
    /// True when the thinking text was shrunk from "thinking{effort}"
    /// to bare "thinking".
    pub shrunk_to_bare_thinking: bool,
}

/// Inputs to [`progressive_status_layout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowLayoutInputs {
    /// Terminal width in columns.
    pub columns: usize,
    /// The precomputed message width, equal to the bare verb width
    /// plus 2 (the render padding the message adds around the text).
    /// The layout subtracts `5` from `(columns - message_width)` to
    /// compute `available_space`, i.e.
    /// `available_space = columns - message_width - 5`.
    ///
    /// Consumers must add the `+ 2` themselves before passing this in
    /// — passing the bare verb width will produce an off-by-2
    /// available space and silently break the progressive width gating.
    pub message_width: usize,
    /// Whether the parent wants a thinking piece at all.
    pub wants_thinking: bool,
    /// True while the displayed status is active thinking.
    pub thinking_is_active: bool,
    /// Visual width of the full thinking text
    /// (`thinking` plus the effort suffix, or `thought for Ns`).
    pub thinking_width: usize,
    /// Whether an effort suffix is present (the shrink fallback only
    /// kicks in when the suffix can be dropped).
    pub has_effort_suffix: bool,
    /// Whether the timer and token pieces are wanted: verbose mode, or
    /// running teammates, or an effective elapsed time past the
    /// show-tokens threshold.
    pub wants_timer_and_tokens: bool,
    /// Visual width of the elapsed-time text.
    pub timer_width: usize,
    /// Visual width of the token-count text.
    pub tokens_width: usize,
    /// Total tokens to display. The token piece is hidden when
    /// `total_tokens == 0`.
    pub total_tokens: u64,
    /// Whether a spinner suffix is present — affects `thinking_only`.
    pub has_spinner_suffix: bool,
}

/// Decide which status pieces fit in the row, in priority order.
pub fn progressive_status_layout(input: RowLayoutInputs) -> RowLayout {
    // available_space = columns - message_width - 5
    // where `message_width` is the bare verb width plus 2.
    // The caller pre-adds the `+ 2`, so we just subtract 5 here. The
    // 5 = 2 (glyph) + 1 (space after glyph) + 1 (space after message) +
    // 1 (paren).
    let available_space = input
        .columns
        .saturating_sub(input.message_width)
        .saturating_sub(5);

    // Show full thinking if it fits.
    let mut show_thinking = input.wants_thinking && available_space > input.thinking_width;
    let mut thinking_text_width = input.thinking_width;
    let mut shrunk_to_bare = false;

    // Try the bare-thinking shrink if the full text doesn't fit and
    // an effort suffix is present.
    if !show_thinking
        && input.wants_thinking
        && input.thinking_is_active
        && input.has_effort_suffix
        && available_space > THINKING_BARE_WIDTH
    {
        show_thinking = true;
        thinking_text_width = THINKING_BARE_WIDTH;
        shrunk_to_bare = true;
    }

    let used_after_thinking = if show_thinking {
        thinking_text_width + SEP_WIDTH
    } else {
        0
    };

    let show_timer =
        input.wants_timer_and_tokens && available_space > used_after_thinking + input.timer_width;

    let used_after_timer = used_after_thinking
        + if show_timer {
            input.timer_width + SEP_WIDTH
        } else {
            0
        };

    let show_tokens = input.wants_timer_and_tokens
        && input.total_tokens > 0
        && available_space > used_after_timer + input.tokens_width;

    let thinking_only = show_thinking
        && input.thinking_is_active
        && !input.has_spinner_suffix
        && !show_timer
        && !show_tokens;

    RowLayout {
        show_thinking,
        show_timer,
        show_tokens,
        thinking_only,
        thinking_text_width,
        shrunk_to_bare_thinking: shrunk_to_bare,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> RowLayoutInputs {
        RowLayoutInputs {
            columns: 100,
            message_width: 20,
            wants_thinking: false,
            thinking_is_active: false,
            thinking_width: 0,
            has_effort_suffix: false,
            wants_timer_and_tokens: false,
            timer_width: 0,
            tokens_width: 0,
            total_tokens: 0,
            has_spinner_suffix: false,
        }
    }

    #[test]
    fn nothing_to_show_returns_all_false() {
        let r = progressive_status_layout(base());
        assert!(!r.show_thinking);
        assert!(!r.show_timer);
        assert!(!r.show_tokens);
        assert!(!r.thinking_only);
    }

    #[test]
    fn thinking_fits_when_room() {
        let mut i = base();
        i.wants_thinking = true;
        i.thinking_is_active = true;
        i.thinking_width = 20;
        let r = progressive_status_layout(i);
        assert!(r.show_thinking);
        assert_eq!(r.thinking_text_width, 20);
        assert!(!r.shrunk_to_bare_thinking);
    }

    #[test]
    fn thinking_does_not_fit_no_shrink() {
        let mut i = base();
        i.columns = 30;
        i.message_width = 20;
        i.wants_thinking = true;
        i.thinking_is_active = true;
        // available_space = 30 - 20 - 5 = 5; thinking_width = 20.
        i.thinking_width = 20;
        // No effort suffix to shrink.
        let r = progressive_status_layout(i);
        assert!(!r.show_thinking);
    }

    #[test]
    fn thinking_shrinks_to_bare_with_effort_suffix() {
        let mut i = base();
        i.columns = 40;
        i.message_width = 20;
        i.wants_thinking = true;
        i.thinking_is_active = true;
        i.has_effort_suffix = true;
        // available = 40 - 20 - 5 = 15; full thinking_width = 20 (no fit);
        // bare width = 8 < 15 → shrink succeeds.
        i.thinking_width = 20;
        let r = progressive_status_layout(i);
        assert!(r.show_thinking);
        assert!(r.shrunk_to_bare_thinking);
        assert_eq!(r.thinking_text_width, THINKING_BARE_WIDTH);
    }

    #[test]
    fn shrink_only_when_thinking_active() {
        let mut i = base();
        i.columns = 40;
        i.message_width = 20;
        i.wants_thinking = true;
        i.thinking_is_active = false; // 'thought for Xs' state
        i.has_effort_suffix = true;
        i.thinking_width = 20;
        let r = progressive_status_layout(i);
        // Cannot shrink — only the bare 'thinking' state is allowed
        // to drop the suffix.
        assert!(!r.show_thinking);
    }

    #[test]
    fn shrink_only_when_effort_suffix_present() {
        let mut i = base();
        i.columns = 40;
        i.message_width = 20;
        i.wants_thinking = true;
        i.thinking_is_active = true;
        i.has_effort_suffix = false; // no suffix to drop
        i.thinking_width = 20;
        let r = progressive_status_layout(i);
        assert!(!r.show_thinking);
    }

    #[test]
    fn timer_fits_when_room_after_thinking() {
        let mut i = base();
        i.wants_timer_and_tokens = true;
        i.timer_width = 5;
        let r = progressive_status_layout(i);
        assert!(r.show_timer);
    }

    #[test]
    fn timer_hidden_when_no_room() {
        let mut i = base();
        i.columns = 26; // available = 26 - 20 - 5 = 1
        i.wants_timer_and_tokens = true;
        i.timer_width = 5;
        let r = progressive_status_layout(i);
        assert!(!r.show_timer);
    }

    #[test]
    fn tokens_hidden_when_zero_total() {
        let mut i = base();
        i.wants_timer_and_tokens = true;
        i.tokens_width = 12;
        i.total_tokens = 0;
        let r = progressive_status_layout(i);
        assert!(!r.show_tokens);
    }

    #[test]
    fn tokens_shown_when_total_positive_and_room() {
        let mut i = base();
        i.wants_timer_and_tokens = true;
        i.tokens_width = 12;
        i.total_tokens = 1500;
        let r = progressive_status_layout(i);
        assert!(r.show_tokens);
    }

    #[test]
    fn thinking_only_when_no_other_pieces() {
        let mut i = base();
        i.wants_thinking = true;
        i.thinking_is_active = true;
        i.thinking_width = 8;
        // No timer / tokens / suffix.
        let r = progressive_status_layout(i);
        assert!(r.thinking_only);
    }

    #[test]
    fn thinking_only_false_when_timer_shown() {
        let mut i = base();
        i.wants_thinking = true;
        i.thinking_is_active = true;
        i.thinking_width = 8;
        i.wants_timer_and_tokens = true;
        i.timer_width = 5;
        let r = progressive_status_layout(i);
        assert!(!r.thinking_only);
    }

    #[test]
    fn thinking_only_false_when_tokens_shown() {
        let mut i = base();
        i.wants_thinking = true;
        i.thinking_is_active = true;
        i.thinking_width = 8;
        i.wants_timer_and_tokens = true;
        i.total_tokens = 500;
        i.tokens_width = 12;
        let r = progressive_status_layout(i);
        assert!(!r.thinking_only);
    }

    #[test]
    fn thinking_only_false_with_spinner_suffix() {
        let mut i = base();
        i.wants_thinking = true;
        i.thinking_is_active = true;
        i.thinking_width = 8;
        i.has_spinner_suffix = true;
        let r = progressive_status_layout(i);
        assert!(!r.thinking_only);
    }

    #[test]
    fn thinking_only_false_when_thinking_not_active() {
        let mut i = base();
        i.wants_thinking = true;
        i.thinking_is_active = false; // 'thought for...' state
        i.thinking_width = 14;
        let r = progressive_status_layout(i);
        assert!(!r.thinking_only);
    }

    #[test]
    fn message_width_includes_two_cell_padding() {
        // Parity pin: the field carries the bare verb width plus the
        // 2-cell render padding. Caller passes glimmer + 2 →
        // available_space = columns - (glimmer + 2) - 5.
        //
        // Verb "thinking" has display width 8, so a caller would pass
        // message_width = 8 + 2 = 10. With columns = 30, that gives:
        // available_space = 30 - (8 + 2) - 5 = 15
        //
        // We pin the same number here to catch any future drift between
        // the in-function comment and the actual subtraction.
        let mut i = base();
        i.columns = 30;
        i.message_width = 10; // glimmerMessageWidth=8 + 2 padding
        i.wants_thinking = true;
        i.thinking_is_active = true;
        i.thinking_width = 14; // doesn't fit in available 15? 14 < 15 -> fits
        let r = progressive_status_layout(i);
        // available = 30 - 10 - 5 = 15; thinking_width 14 < 15, fits.
        assert!(
            r.show_thinking,
            "thinking_width 14 must fit in available 15"
        );
        // And one cell more must NOT fit (the comparison is strict `>`).
        let mut j = i;
        j.thinking_width = 15;
        let r2 = progressive_status_layout(j);
        assert!(
            !r2.show_thinking,
            "thinking_width 15 must NOT fit (strict >)"
        );
    }

    #[test]
    fn extreme_narrow_terminal_drops_everything() {
        let mut i = base();
        i.columns = 25;
        i.wants_thinking = true;
        i.thinking_is_active = true;
        i.thinking_width = 30;
        i.wants_timer_and_tokens = true;
        i.timer_width = 5;
        i.total_tokens = 100;
        i.tokens_width = 12;
        let r = progressive_status_layout(i);
        assert!(!r.show_thinking || r.shrunk_to_bare_thinking);
        // Available space = 25 - 20 - 5 = 0. Nothing fits.
    }
}
