//! Strip the four reserved prompt-wrapper XML tags before lexing.
//!
//! ## Behavior
//!
//! The reserved tag set is the module-level `TAGS` slice:
//! `commit_analysis`, `context`, `function_analysis`, `pr_analysis`.
//! [`strip_prompt_xml_tags`] walks the input with a byte cursor. While
//! the current byte is not `<` it copies the whole run up to the next
//! `<` in a single `push_str`; at a `<` it calls the private `strip_one`
//! helper. `strip_one` tries each tag of `TAGS` in order, requires a
//! literal `<tag>` opener, and then looks for the matching `</tag>`
//! close. If the opener matches no reserved tag, or the close is
//! missing, the attempt fails and the caller copies the `<` verbatim and
//! advances one byte, so the next `<` gets a fresh attempt. A successful
//! strip consumes one trailing newline when present. The assembled
//! string is `.trim()`-ed before it is returned.
//!
//! Callers apply this to content before lexing; the streaming path
//! applies it on every update so boundary tracking stays aligned
//! with the non-streaming path.
//!
//! ## Load-bearing properties
//!
//! Written as a regex, the strip would be
//! `<(commit_analysis|context|function_analysis|pr_analysis)>.*?</\1>\n?`
//! with the `g` and `s` flags. The walker keeps each property that
//! pattern carries:
//!
//! * `g` — replace **all** non-overlapping matches.
//! * `s` — `.` matches `\n` (so multi-line tag bodies strip).
//! * `\1` — backreference: the closing tag must match the opening tag
//!   name. `<context>...</commit_analysis>` does NOT strip.
//! * `\n?` after `\1>` — eat one trailing newline if present, so the
//!   stripped output doesn't leave a blank line behind.
//!
//! And the final `.trim()` strips leading/trailing whitespace after
//! replacement.
//!
//! It is a hand-rolled walker, not a regex pull-in, for the same reason as
//! `detect.rs`: this crate intentionally avoids `regex` to keep the
//! dependency floor low.

const TAGS: &[&str] = &[
    "commit_analysis",
    "context",
    "function_analysis",
    "pr_analysis",
];

/// Remove every `<tag>...</tag>` block whose name is one of
/// `commit_analysis`, `context`, `function_analysis`, `pr_analysis`,
/// then trim leading/trailing whitespace, as documented in the module
/// docs.
///
/// * Tag bodies may span multiple lines (`s` flag in the equivalent
///   regex).
/// * Closing tag name must match opening tag name (regex `\1`
///   backreference).
/// * One trailing `\n` is consumed if present (regex `\n?`).
/// * The replace pass is non-overlapping and left-to-right (regex `g`
///   flag): once a region is removed, scanning continues from the
///   character after the removed region.
/// * Final `.trim()` removes leading/trailing Unicode whitespace.
pub fn strip_prompt_xml_tags(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let bytes = content.as_bytes();
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        if bytes[cursor] != b'<' {
            // Fast path — copy non-`<` runs in one go.
            let next_lt = bytes[cursor..]
                .iter()
                .position(|&b| b == b'<')
                .map(|p| cursor + p)
                .unwrap_or(bytes.len());
            out.push_str(&content[cursor..next_lt]);
            cursor = next_lt;
            continue;
        }

        // We're at a `<`. See if it opens one of the reserved tags.
        let stripped = match strip_one(content, cursor) {
            Some((advance_to, _consumed)) => {
                cursor = advance_to;
                true
            }
            None => false,
        };
        if !stripped {
            // Not a reserved opener — copy the `<` literally and
            // advance one byte. The next `<` (if any) gets a fresh
            // attempt.
            out.push('<');
            cursor += 1;
        }
    }

    out.trim().to_string()
}

/// Try to strip a reserved `<tag>...</tag>` block starting at
/// `start`. Returns `Some((cursor_after_strip, byte_count_removed))`
/// on success, or `None` if the bytes at `start` do not open one of
/// the reserved tags or the block is malformed (no matching close).
fn strip_one(content: &str, start: usize) -> Option<(usize, usize)> {
    let bytes = content.as_bytes();
    debug_assert_eq!(bytes[start], b'<');

    for tag in TAGS {
        let open = format!("<{tag}>");
        if !content[start..].starts_with(&open) {
            continue;
        }
        let body_start = start + open.len();
        let close = format!("</{tag}>");
        let close_rel = content[body_start..].find(&close)?;
        let mut block_end = body_start + close_rel + close.len();
        // Eat one trailing \n if present, matching the `\n?` tail in
        // the equivalent regex.
        if block_end < bytes.len() && bytes[block_end] == b'\n' {
            block_end += 1;
        }
        return Some((block_end, block_end - start));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_each_reserved_tag_in_isolation() {
        for tag in TAGS {
            let input = format!("before <{tag}>body</{tag}> after");
            let out = strip_prompt_xml_tags(&input);
            assert_eq!(out, "before  after", "tag {tag} should strip to leave gap");
        }
    }

    #[test]
    fn does_not_strip_unrelated_tag() {
        // <tick>...</tick> is not in the reserved set — must survive.
        let input = "before <tick>body</tick> after";
        let out = strip_prompt_xml_tags(input);
        assert_eq!(out, "before <tick>body</tick> after");
    }

    #[test]
    fn strips_multiple_non_overlapping_blocks() {
        let input = "<context>a</context>middle<commit_analysis>b</commit_analysis>";
        let out = strip_prompt_xml_tags(input);
        assert_eq!(out, "middle");
    }

    #[test]
    fn strips_multiline_body() {
        // Equivalent of the `s` (dot-all) flag in the equivalent regex.
        let input = "x <context>line1\nline2\nline3</context> y";
        let out = strip_prompt_xml_tags(input);
        assert_eq!(out, "x  y");
    }

    #[test]
    fn consumes_one_trailing_newline_after_close() {
        // The equivalent regex has `\n?` after the close — strip exactly one.
        // Match consumes `<context>x</context>\n` (note the trailing
        // newline INSIDE the match), so the replacement leaves only
        // the bytes that were not part of the match: `before\n` and
        // `after`. trim() does not touch interior whitespace.
        let input = "before\n<context>x</context>\nafter";
        let out = strip_prompt_xml_tags(input);
        assert_eq!(out, "before\nafter");
    }

    #[test]
    fn does_not_consume_two_trailing_newlines() {
        // Only ONE \n is eaten — the second survives as a blank line.
        let input = "<context>x</context>\n\nafter";
        let out = strip_prompt_xml_tags(input);
        // Stripped: "" + "\nafter" → "\nafter" → trim → "after".
        assert_eq!(out, "after");
    }

    #[test]
    fn open_with_no_close_is_not_stripped() {
        // The equivalent regex would not match — there's no closing tag, so
        // the entire `<context>...` is preserved.
        let input = "before <context>no closing tag here";
        let out = strip_prompt_xml_tags(input);
        assert_eq!(out, "before <context>no closing tag here");
    }

    #[test]
    fn mismatched_close_tag_does_not_strip() {
        // `\1` backreference: <context> ... </commit_analysis> does
        // NOT match. The equivalent regex would skip this entirely, and
        // `strip_one` only matches `</same>`.
        let input = "<context>x</commit_analysis>";
        let out = strip_prompt_xml_tags(input);
        assert_eq!(out, "<context>x</commit_analysis>");
    }

    #[test]
    fn nested_reserved_tags_strip_outermost_first() {
        // The equivalent regex is non-greedy (`.*?`) so it strips the
        // shortest match. With `<context>a<context>b</context>c</context>`
        // the FIRST `</context>` closes the FIRST `<context>`, leaving
        // `b` and a stray `c</context>`. We mirror that.
        let input = "<context>a<context>b</context>c</context>";
        let out = strip_prompt_xml_tags(input);
        // Strip pass 1: "<context>a<context>b</context>" → ""
        //   leaving "c</context>"
        // No more openers in "c</context>" — stop.
        // Trim: "c</context>"
        assert_eq!(out, "c</context>");
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(strip_prompt_xml_tags(""), "");
    }

    #[test]
    fn whitespace_only_input_trims_to_empty() {
        assert_eq!(strip_prompt_xml_tags("   \n\t  "), "");
    }

    #[test]
    fn leading_and_trailing_whitespace_is_trimmed_after_strip() {
        let input = "\n\n  <context>x</context>\n  ";
        // Strip → "\n\n  \n  " (one \n eaten after close, others survive)
        // Wait — the close is followed by `\n  `; only the immediate \n
        // is eaten, leaving "\n\n    ". Then trim → "".
        let out = strip_prompt_xml_tags(input);
        assert_eq!(out, "");
    }

    #[test]
    fn lt_inside_text_does_not_break_scanner() {
        // Bare `<` characters that don't open a reserved tag must
        // pass through literally.
        let input = "a < b and b > c";
        assert_eq!(strip_prompt_xml_tags(input), "a < b and b > c");
    }

    /// Exhaustive table.
    #[test]
    fn strip_prompt_tags_table() {
        let cases: &[(&str, &str)] = &[
            ("", ""),
            ("plain prose", "plain prose"),
            ("<context>x</context>", ""),
            ("<commit_analysis>x</commit_analysis>", ""),
            ("<function_analysis>x</function_analysis>", ""),
            ("<pr_analysis>x</pr_analysis>", ""),
            ("<tick>x</tick>", "<tick>x</tick>"),
            ("a<context>x</context>b", "ab"),
            // The equivalent regex's `\n?` consumes exactly one trailing
            // newline AFTER the closing tag, so the entire match is
            // `<context>x</context>\n` and the result is just `ab`.
            ("a<context>x</context>\nb", "ab"),
            ("<context>multi\nline\nbody</context>", ""),
            ("<context>x</context><context>y</context>", ""),
            (
                "<context>x</commit_analysis>",
                "<context>x</commit_analysis>",
            ),
            ("<context>unterminated", "<context>unterminated"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                strip_prompt_xml_tags(input),
                *expected,
                "case failed for input {input:?}",
            );
        }
    }
}
