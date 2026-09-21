//! One display-width policy for the whole render stack.
//!
//! Terminals disagree about the *East Asian Ambiguous* characters — the
//! block that holds `—` (U+2014), `·` (U+00B7), the curly quotes, the box
//! drawing characters and a few hundred more. Unicode says their width
//! depends on context: one cell in a Western context, two in a CJK one.
//! `unicode-width` answers one cell ([`UnicodeWidthStr::width`]) or two
//! ([`UnicodeWidthStr::width_cjk`]) and leaves the choice to the caller.
//!
//! That choice has to be made once, for everything. A renderer that
//! measures a character as one cell while the terminal paints two loses
//! track of where the cursor is: the next partial redraw lands a column
//! short, overwrites the wrong cell, and cannot erase what it left behind
//! — `· lingers 9 min` painted over `stopped` comes out as
//! `· ingers 9 min      ed`. The mismatch, not the character, is the bug,
//! so the fix is a single setting that everything measuring a string for
//! the terminal reads.
//!
//! The default is narrow, which is what `unicode-width` alone did and
//! what every non-terminal caller (a desktop app, background workers)
//! wants. A terminal UI resolves its terminal's real answer during
//! startup and calls [`set_ambiguous_wide`] before its first frame;
//! nothing else touches it.
//!
//! # Usage
//!
//! [`WidthStr`] and [`WidthChar`] are drop-in replacements for
//! `unicode_width`'s traits — same `width` method, policy applied:
//!
//! ```
//! use rebon_width::WidthStr;
//!
//! assert_eq!("ab".width(), 2);
//! ```

use std::sync::atomic::{AtomicBool, Ordering};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Whether ambiguous-width characters take two cells.
///
/// Relaxed ordering throughout: this is set once, before the first frame,
/// and read on the render path. No other state is published with it, so
/// there is nothing for an acquire/release pair to order.
static AMBIGUOUS_WIDE: AtomicBool = AtomicBool::new(false);

/// Declare how this process's terminal paints ambiguous-width characters.
///
/// Call it once, before anything measures a string for that terminal.
pub fn set_ambiguous_wide(wide: bool) {
    AMBIGUOUS_WIDE.store(wide, Ordering::Relaxed);
}

/// The policy in force. `false` — one cell — unless a terminal said
/// otherwise.
pub fn ambiguous_is_wide() -> bool {
    AMBIGUOUS_WIDE.load(Ordering::Relaxed)
}

/// The width of `text` in terminal cells, under the current policy.
pub fn str_width(text: &str) -> usize {
    if ambiguous_is_wide() {
        UnicodeWidthStr::width_cjk(text)
    } else {
        UnicodeWidthStr::width(text)
    }
}

/// The width of `ch` in terminal cells, under the current policy.
/// `None` for control characters, as `unicode-width` reports them.
pub fn char_width(ch: char) -> Option<usize> {
    if ambiguous_is_wide() {
        UnicodeWidthChar::width_cjk(ch)
    } else {
        UnicodeWidthChar::width(ch)
    }
}

/// Returns `true` for characters that many terminals render as two cells wide
/// despite `unicode-width` reporting one.
pub fn is_likely_wide_in_terminal(ch: char) -> bool {
    // U+23F5..=U+23F7 (⏵⏶⏷) stay excluded: terminals generally render
    // those geometric triangles as one cell, so padding them adds a gap.
    matches!(
        ch,
        '\u{23F8}'
            ..='\u{23FA}' | // Transport/media symbols (⏸⏹⏺)
        '\u{25B6}' |               // ▶ BLACK RIGHT-POINTING TRIANGLE
        '\u{2714}' // ✔ HEAVY CHECK MARK
    )
}

/// Pad symbols for terminals whose rendering disagrees with `unicode-width`.
pub fn pad_wide_symbol(symbols: &str) -> String {
    let extra = symbols
        .chars()
        .filter(|&ch| is_likely_wide_in_terminal(ch))
        .count();
    format!("{symbols}{}", " ".repeat(extra))
}

/// Width of one character as [`truncate_to_width`] and
/// [`truncate_to_ellipsis`] measure it.
///
/// Deliberately a wider set than [`is_likely_wide_in_terminal`], which
/// leaves the geometric triangles U+23F5..=U+23F7 out because padding
/// them opens a gap. Cutting a row one cell early costs nothing, so
/// this side counts them. The two rules sit together so the difference
/// is visible rather than hidden a crate apart.
///
/// Public because a surface that cuts its own rows a different way — say
/// one that truncates a path with no ellipsis, or a filename from the
/// left — still has to measure a character the same way, and a second
/// copy of this table would be a second answer.
pub fn terminal_char_width(ch: char) -> usize {
    if matches!(
        ch,
        '\u{23F5}'
            ..='\u{23FA}' | // Transport/media symbols (⏵⏶⏷⏸⏹⏺)
        '\u{25B6}' |               // ▶ BLACK RIGHT-POINTING TRIANGLE
        '\u{2714}' // ✔ HEAVY CHECK MARK
    ) {
        2
    } else {
        char_width(ch).unwrap_or(0)
    }
}

/// Truncate a string to the given display width, appending `...`
/// when truncation occurred.
///
/// This is a width question and nothing else: a panel that formats its own
/// rows needs the same answer the painter would give, and a second copy of
/// this would be a second answer.
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let width = str_width(text);
    if width <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = terminal_char_width(ch);
        if used + ch_width + 3 > max_width {
            break;
        }
        used += ch_width;
        out.push(ch);
    }
    out.push_str("...");
    out
}

/// Truncate a string to the given display width, appending `…` (a single
/// cell) when truncation occurred.
///
/// The one-character-ellipsis sibling of [`truncate_to_width`], which
/// spends three cells on `...`. Both are width questions; which one a
/// surface wants is a look, not a rule.
///
/// Two rules are load-bearing, and both take the stricter answer:
///
/// * **How wide a character is.** The same rule [`truncate_to_width`]
///   uses, which is the widest, so a row is cut a cell early rather than a
///   cell late. Cutting early costs a space; cutting late overruns the
///   column and the next partial redraw lands wrong.
/// * **Zero width.** Nothing fits in no columns, so this returns the empty
///   string. `…` is itself one cell wide, so returning the marker would
///   overflow.
pub fn truncate_to_ellipsis(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if str_width(text) <= max_width {
        return text.to_string();
    }
    // One column for the marker; the rest is content.
    let content_width = max_width - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = terminal_char_width(ch);
        if used + ch_width > content_width {
            break;
        }
        used += ch_width;
        out.push(ch);
    }
    out.push('\u{2026}');
    out
}

/// Display width of a string, under the process-wide policy.
///
/// Named after `unicode_width::UnicodeWidthStr` so swapping the import is
/// the whole edit at a call site.
pub trait WidthStr {
    /// Width in terminal cells.
    fn width(&self) -> usize;
}

impl WidthStr for str {
    fn width(&self) -> usize {
        str_width(self)
    }
}

/// Display width of a character, under the process-wide policy.
pub trait WidthChar {
    /// Width in terminal cells, or `None` for a control character.
    fn width(self) -> Option<usize>;
}

impl WidthChar for char {
    fn width(self) -> Option<usize> {
        char_width(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The setting is process-wide, so the tests that flip it run as one
    /// test — two of them in parallel would read each other's policy.
    #[test]
    fn ambiguous_characters_follow_the_policy_and_nothing_else_moves() {
        // Both of the characters terminal session rows are built from, and
        // a CJK quote, which is ambiguous too.
        let ambiguous = "—·“";
        // Unambiguous neighbours: ASCII is always one, a CJK ideograph is
        // always two, and a combining mark is always zero.
        let settled = "a漢\u{0301}";

        set_ambiguous_wide(false);
        assert!(!ambiguous_is_wide());
        assert_eq!(str_width(ambiguous), 3);
        assert_eq!(char_width('—'), Some(1));
        assert_eq!(str_width(settled), 3);

        set_ambiguous_wide(true);
        assert!(ambiguous_is_wide());
        assert_eq!(str_width(ambiguous), 6);
        assert_eq!(char_width('—'), Some(2));
        assert_eq!(str_width(settled), 3, "only the ambiguous block moves");

        // The traits read the same setting as the functions.
        assert_eq!(WidthStr::width(ambiguous), 6);
        assert_eq!(WidthChar::width('·'), Some(2));

        set_ambiguous_wide(false);
        assert_eq!(WidthStr::width(ambiguous), 3);
        assert_eq!(WidthChar::width('·'), Some(1));
    }

    #[test]
    fn terminal_wide_exceptions_match_the_pinned_set() {
        for ch in ['\u{23F8}', '\u{23F9}', '\u{23FA}', '\u{25B6}', '\u{2714}'] {
            assert!(is_likely_wide_in_terminal(ch), "{ch}");
        }
        for ch in [
            '\u{23F5}', '\u{23F6}', '\u{23F7}', '\u{23FB}', '\u{25B5}', '\u{25B7}', '\u{2713}',
            '\u{2715}', 'a', '漢',
        ] {
            assert!(!is_likely_wide_in_terminal(ch), "{ch}");
        }
    }

    #[test]
    fn terminal_symbol_padding_adds_one_space_per_exception() {
        assert_eq!(pad_wide_symbol("a⏸▶✔z"), "a⏸▶✔z   ");
    }

    #[test]
    fn terminal_symbol_padding_leaves_other_characters_unchanged() {
        assert_eq!(pad_wide_symbol("a⏵漢"), "a⏵漢");
    }

    #[test]
    fn a_control_character_has_no_width() {
        assert_eq!(char_width('\u{1}'), None);
    }

    #[test]
    fn truncation_keeps_short_text_and_marks_what_it_cut() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("hello world", 8), "hello...");
        assert_eq!(truncate_to_width("hello", 0), "");
        // Below the ellipsis's own width there is only room for dots.
        assert_eq!(truncate_to_width("hello", 2), "..");
    }

    /// The two wide-character rules disagree on purpose, and truncation
    /// is the side that counts the triangles.
    #[test]
    fn truncation_counts_a_triangle_the_padding_rule_leaves_alone() {
        assert!(!is_likely_wide_in_terminal('\u{23F5}'));
        assert_eq!(terminal_char_width('\u{23F5}'), 2);
        assert_eq!(terminal_char_width('a'), 1);
    }

    /// A double-width character is measured in cells, not in chars.
    #[test]
    fn truncation_measures_wide_characters_in_cells() {
        assert_eq!(truncate_to_width("漢字漢字", 8), "漢字漢字");
        assert_eq!(truncate_to_width("漢字漢字", 7), "漢字...");
    }

    /// The one-cell ellipsis spends one column on the marker, not three,
    /// so it keeps two more characters than its `...` sibling at the
    /// same width.
    #[test]
    fn the_one_cell_ellipsis_keeps_what_the_three_dot_one_cuts() {
        assert_eq!(truncate_to_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_to_ellipsis("hello", 5), "hello");
        assert_eq!(truncate_to_ellipsis("hello world", 8), "hello w…");
    }

    /// Nothing fits in no columns, and the `…` marker is itself one cell
    /// wide, so neither the input nor the marker may come back.
    #[test]
    fn zero_width_truncates_to_nothing_rather_than_overflowing() {
        assert_eq!(truncate_to_ellipsis("hello", 0), "");
        assert_eq!(truncate_to_ellipsis("", 0), "");
        // One column holds the marker and nothing else.
        assert_eq!(truncate_to_ellipsis("hello", 1), "…");
    }

    /// Once a row is being cut, `⏵` costs two cells — the wide rule,
    /// not the narrower padding set `is_likely_wide_in_terminal`
    /// answers for. Using the padding set here keeps one character too
    /// many and overruns the column.
    ///
    /// The fits-check above it stays on the plain policy, which is the
    /// same asymmetry [`truncate_to_width`] has: deciding *whether* to
    /// cut asks the terminal's own width, deciding *where* asks the
    /// pessimistic one.
    #[test]
    fn the_ellipsis_form_counts_a_triangle_as_two_cells_once_it_cuts() {
        // Five by the plain policy, so nothing is cut.
        assert_eq!(truncate_to_ellipsis("\u{23F5}abcd", 5), "\u{23F5}abcd");
        // Cutting to four leaves three columns: the triangle takes two
        // of them, so only one character follows it.
        assert_eq!(truncate_to_ellipsis("\u{23F5}abcd", 4), "\u{23F5}a…");
        // The narrower rule would keep two and overrun by one.
        assert_ne!(truncate_to_ellipsis("\u{23F5}abcd", 4), "\u{23F5}ab…");
    }

    /// Wide characters are measured in cells here too, and a cut that
    /// would land mid-character stops before it.
    #[test]
    fn the_ellipsis_form_measures_wide_characters_in_cells() {
        assert_eq!(truncate_to_ellipsis("漢字漢字", 8), "漢字漢字");
        assert_eq!(truncate_to_ellipsis("漢字漢字", 7), "漢字漢…");
        assert_eq!(truncate_to_ellipsis("漢字漢字", 6), "漢字…");
    }
}
