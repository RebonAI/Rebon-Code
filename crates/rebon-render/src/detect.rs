//! Markdown-syntax fast-path detection.
//!
//! ## Behavior
//!
//! [`has_markdown_syntax`] answers whether the first 500 bytes of a string
//! match this pattern (quoted to show the trailing space; no multiline
//! flag):
//!
//! ```text
//! "[#*`|[>\-_~]|\n\n|^\d+\. |\n\d+\. "
//! ```
//!
//! Only a prefix is sampled because markdown, when present, usually shows
//! up early (headers, code fences, lists), while long tool outputs are
//! mostly plain-text tails.
//!
//! ## Why a hand-rolled scan instead of `regex`
//!
//! The pattern has three pieces:
//!
//! 1. **Single-character markers** (`#*`|[>\-_~`) — set membership
//!    test, no anchoring.
//! 2. **Blank line** (`\n\n`) — two-byte literal.
//! 3. **Ordered list** (`^\d+\. ` or `\n\d+\. `) — anchored to start
//!    of string OR newline, then one-or-more digits, then `. `.
//!
//! Pulling in the `regex` crate for one function (this crate's
//! only "string scanning" hot path) would inflate the crate's
//! dependency floor by ~1MB of compile output. The hand-rolled
//! scan walks the (truncated) buffer once, byte-by-byte, and exits
//! on the first match. It gives the same single-pass guarantee a
//! compiled regex would (a regex engine compiles the alternation
//! into an automaton; we're doing it by hand).
//!
//! The sample window is 500 **bytes**, cut back to a code-point
//! boundary, not 500 characters. The two differ only for inputs whose
//! first 500 characters straddle a multi-byte UTF-8 sequence; the result
//! is the same for all ASCII inputs (the dominant case for "is this
//! prose vs markdown") and for any input whose markdown syntax appears
//! within the first 500 bytes, which is the assumption sampling makes.

/// Sample window: inputs longer than this are scanned only up to it.
/// See module docs on the byte/char difference.
const SAMPLE_WINDOW_BYTES: usize = 500;

/// Returns `true` if `s` contains a character or sequence that the
/// markdown lexer would treat as a syntax marker, per the pattern in the
/// module docs.
///
/// # Match set
///
/// The function is a one-shot byte scan. It returns at the first
/// matching position and does not allocate. The match-set is:
///
/// * Single-byte markers — `#`, `*`, `` ` ``, `|`, `[`, `>`, `-`, `_`, `~`
/// * `\n\n` (blank line)
/// * `^\d+\. ` — at least one ASCII digit at offset 0 followed by
///   `. `
/// * `\n\d+\. ` — newline, then at least one ASCII digit, then `. `
///
/// The pattern's `^` anchor means "start of string" (not start of line)
/// because it has no multiline flag: only offset 0 counts as "start";
/// mid-string ordered list items must be preceded by a literal `\n`.
pub fn has_markdown_syntax(s: &str) -> bool {
    // Truncate to a UTF-8-safe prefix of at most SAMPLE_WINDOW_BYTES.
    // `is_char_boundary` keeps the slice valid even if the window
    // would otherwise cut a multi-byte sequence.
    let sample = if s.len() > SAMPLE_WINDOW_BYTES {
        let mut end = SAMPLE_WINDOW_BYTES;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    } else {
        s
    };

    let bytes = sample.as_bytes();
    let n = bytes.len();
    if n == 0 {
        return false;
    }

    // Ordered-list anchor at offset 0. Matches `^\d+\. `.
    if bytes[0].is_ascii_digit() && matches_ordered_list_marker(bytes, 0) {
        return true;
    }

    let mut i = 0;
    while i < n {
        let b = bytes[i];

        // Single-byte set membership.
        match b {
            b'#' | b'*' | b'`' | b'|' | b'[' | b'>' | b'-' | b'_' | b'~' => return true,
            _ => {}
        }

        if b == b'\n' {
            // \n\n — blank line.
            if i + 1 < n && bytes[i + 1] == b'\n' {
                return true;
            }
            // \n\d+\. — newline-anchored ordered list.
            if i + 1 < n
                && bytes[i + 1].is_ascii_digit()
                && matches_ordered_list_marker(bytes, i + 1)
            {
                return true;
            }
        }

        i += 1;
    }

    false
}

/// Helper for the `\d+\. ` portion (one-or-more digits, then literal
/// `. `). Caller has already verified that `bytes[start]` is a digit.
fn matches_ordered_list_marker(bytes: &[u8], start: usize) -> bool {
    let mut j = start;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    // Need a `. ` (period + space) immediately after the digit run.
    j + 1 < bytes.len() && bytes[j] == b'.' && bytes[j + 1] == b' '
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Single-character marker coverage. Each marker is tested in
    // isolation so a regression that drops any one of them shows up
    // as a clear table delta.
    // -----------------------------------------------------------------

    #[test]
    fn detects_each_single_byte_marker() {
        let cases: &[(char, &str)] = &[
            ('#', "# Heading"),
            ('*', "*emphasis*"),
            ('`', "use `code`"),
            ('|', "a | b"),
            ('[', "see [link]"),
            ('>', "> quote"),
            ('-', "- list"),
            ('_', "an _italic_ word"),
            ('~', "~strikethrough~"),
        ];
        for (marker, text) in cases {
            assert!(
                has_markdown_syntax(text),
                "marker {marker:?} should be detected in {text:?}",
            );
        }
    }

    // -----------------------------------------------------------------
    // Blank line + ordered list anchoring. These are the load-bearing
    // multi-character cases — exactly the ones that motivate the
    // single-pass scan.
    // -----------------------------------------------------------------

    #[test]
    fn detects_blank_line() {
        assert!(has_markdown_syntax("paragraph one\n\nparagraph two"));
    }

    #[test]
    fn does_not_detect_single_newline_alone() {
        // A single \n is not a marker — only \n\n is.
        assert!(!has_markdown_syntax("line one\nline two"));
    }

    #[test]
    fn detects_ordered_list_at_start_of_string() {
        // ^\d+\. anchored to offset 0.
        assert!(has_markdown_syntax("1. first item"));
        assert!(has_markdown_syntax("42. answer"));
    }

    #[test]
    fn detects_ordered_list_after_newline() {
        // \n\d+\. — newline-anchored.
        assert!(has_markdown_syntax("intro\n1. step one"));
    }

    #[test]
    fn does_not_detect_ordered_list_mid_line() {
        // Mid-line digits + period must NOT fire. The pattern has
        // `^` (start-of-string) and `\n` anchors, no `m` flag.
        assert!(!has_markdown_syntax("the price is 9. dollars"));
    }

    #[test]
    fn requires_period_then_space_for_ordered_list() {
        // `1.foo` (no space) — not a list.
        assert!(!has_markdown_syntax("v1.0 release"));
        // `1 .foo` (space before period) — not a list.
        assert!(!has_markdown_syntax("the value 1 .foo"));
    }

    #[test]
    fn requires_at_least_one_digit_for_ordered_list() {
        // `\d+` is one-or-more, so `. foo` alone doesn't fire on the
        // ordered-list branch. But `.` is not in the marker set, and
        // the `. foo` has no other markers either, so the whole
        // string is plain text.
        assert!(!has_markdown_syntax(". no leading digit"));
    }

    // -----------------------------------------------------------------
    // Plain text — the negative cases. These are inputs that should
    // take the fast path and skip lexer entirely.
    // -----------------------------------------------------------------

    #[test]
    fn empty_string_is_not_markdown() {
        assert!(!has_markdown_syntax(""));
    }

    #[test]
    fn pure_prose_is_not_markdown() {
        assert!(!has_markdown_syntax(
            "the quick brown fox jumps over the lazy dog"
        ));
    }

    #[test]
    fn unicode_prose_with_no_markers_is_not_markdown() {
        // CJK + emoji, no markers. Tests that the byte-window
        // truncation doesn't accidentally fire on multi-byte content.
        assert!(!has_markdown_syntax("你好世界 こんにちは 🌍"));
    }

    // -----------------------------------------------------------------
    // Sample-window truncation behaviour. Markdown markers appearing
    // AFTER the first 500 chars are deliberately not detected —
    // they're considered "rare enough that the cost of a full scan
    // isn't worth it".
    // -----------------------------------------------------------------

    #[test]
    fn marker_after_500_byte_window_is_not_detected() {
        // 600 bytes of plain text, then a `#`. The 500-byte sample
        // window does not see the `#`.
        let mut s = String::with_capacity(700);
        for _ in 0..600 {
            s.push('a');
        }
        s.push('#');
        assert!(!has_markdown_syntax(&s));
    }

    #[test]
    fn marker_at_byte_499_is_detected() {
        // The 500th byte (index 499) is still inside the window.
        let mut s = String::with_capacity(600);
        for _ in 0..499 {
            s.push('a');
        }
        s.push('#');
        for _ in 0..100 {
            s.push('a');
        }
        assert!(has_markdown_syntax(&s));
    }

    #[test]
    fn truncation_keeps_utf8_boundary() {
        // 498 ASCII bytes + a 4-byte emoji. The window cap is 500
        // but 500 falls inside the emoji's bytes. The truncation must
        // back up to 498 to stay on a char boundary; no panic.
        let mut s = String::with_capacity(600);
        for _ in 0..498 {
            s.push('a');
        }
        s.push('🌍'); // 4 bytes
        for _ in 0..100 {
            s.push('a');
        }
        // No markers anywhere — must return false WITHOUT panicking.
        assert!(!has_markdown_syntax(&s));
    }

    // -----------------------------------------------------------------
    // table. One source of truth for "what gets routed to
    // the lexer". A renamed marker shows up as a clear row diff.
    // -----------------------------------------------------------------

    #[test]
    fn classification_table() {
        let cases: &[(&str, bool)] = &[
            ("", false),
            ("plain prose", false),
            ("# heading", true),
            ("**bold**", true),
            ("`code`", true),
            ("a | b | c", true),
            ("[link](url)", true),
            ("> quote", true),
            ("- bullet", true),
            ("under_score word", true),
            ("~strike~", true),
            ("a\n\nb", true),
            ("a\nb", false),
            ("1. step", true),
            ("1.foo", false),
            ("v1.0", false),
            ("intro\n1. step", true),
            ("intro\n1.foo", false),
            ("the value 9. dollars", false),
            ("你好世界", false),
            ("CJK with marker # in it", true),
        ];
        for (input, expected) in cases {
            assert_eq!(
                has_markdown_syntax(input),
                *expected,
                "table entry failed for {input:?}",
            );
        }
    }
}
