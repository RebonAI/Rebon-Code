//! Fence-state scanning for split-safe streaming text.
//!
//! A consumer that commits the top of a still-streaming text block into
//! immutable scrollback and leaves the rest live re-parses the two halves as
//! *independent* markdown documents afterwards, so a cut inside an unclosed
//! fenced code block inverts code/prose styling for everything after the cut:
//! the remainder's lines lose their code styling, and the original closing
//! fence line *opens* a fence in the remainder's fresh parse.
//!
//! [`open_fence_at_end`] reports the fence left open at the end of the
//! committed half so the caller can close it there and re-open it at
//! the head of the live remainder. Fence lines are consumed by the
//! parser and never rendered, so the repair is invisible.
//!
//! ## Scope
//!
//! Top-level CommonMark fences only:
//!
//! * ≥3 backticks or tildes, indented at most 3 spaces, open a fence.
//!   A backtick-fence info string may not contain a backtick (that
//!   shape is inline code, not a fence).
//! * A line of ≥ the opening run of the same marker (≤3 spaces indent,
//!   nothing else but trailing whitespace) closes it.
//! * Inside an open fence every other line — including would-be
//!   openers — is content.
//!
//! Fences nested in lists or blockquotes are out of scope: they need
//! container-stack context that only a full block parser has, and
//! assistant prose overwhelmingly uses top-level fences.

/// A fenced code block left open at the end of a scanned text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFence {
    /// Fence marker character: `` ` `` or `~`.
    marker: char,
    /// Length of the opening marker run (≥3). A closer must be at
    /// least this long.
    count: usize,
    /// Info string of the opener (language tag), already trimmed.
    info: String,
}

impl OpenFence {
    /// The line that closes this fence.
    pub fn closing_line(&self) -> String {
        self.marker.to_string().repeat(self.count)
    }

    /// The line that re-opens an identical fence (marker run plus the
    /// original info string) so a continuation re-parses with the same
    /// code-block context and the eventual real closer still matches.
    pub fn opening_line(&self) -> String {
        let mut line = self.closing_line();
        line.push_str(&self.info);
        line
    }
}

/// Scan `text` line by line and return the fenced code block still
/// open at its end, if any.
pub fn open_fence_at_end(text: &str) -> Option<OpenFence> {
    let mut open: Option<OpenFence> = None;
    for line in text.lines() {
        match &open {
            None => open = parse_opening_fence(line),
            Some(fence) => {
                if is_closing_fence(line, fence) {
                    open = None;
                }
            }
        }
    }
    open
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn parse_opening_fence(line: &str) -> Option<OpenFence> {
    let indent = leading_spaces(line);
    if indent > 3 {
        // 4+ spaces is an indented code block, never a fence.
        return None;
    }
    let rest = &line[indent..];
    let marker = rest.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let count = rest.chars().take_while(|&c| c == marker).count();
    if count < 3 {
        return None;
    }
    let info = rest[count..].trim();
    if marker == '`' && info.contains('`') {
        return None;
    }
    Some(OpenFence {
        marker,
        count,
        info: info.to_string(),
    })
}

fn is_closing_fence(line: &str, fence: &OpenFence) -> bool {
    let indent = leading_spaces(line);
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let count = rest.chars().take_while(|&c| c == fence.marker).count();
    // Marker chars are ASCII, so char count == byte offset.
    count >= fence.count && rest[count..].trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fence(marker: char, count: usize, info: &str) -> OpenFence {
        OpenFence {
            marker,
            count,
            info: info.to_string(),
        }
    }

    #[test]
    fn empty_text_has_no_open_fence() {
        assert_eq!(open_fence_at_end(""), None);
    }

    #[test]
    fn plain_prose_has_no_open_fence() {
        assert_eq!(open_fence_at_end("just some text\nand more"), None);
    }

    #[test]
    fn unclosed_backtick_fence_is_reported_with_info() {
        let text = "before\n```text\ncargo fmt --all --check\ncargo test";
        assert_eq!(open_fence_at_end(text), Some(fence('`', 3, "text")));
    }

    #[test]
    fn unclosed_fence_without_info_is_reported() {
        assert_eq!(open_fence_at_end("```\ncode"), Some(fence('`', 3, "")));
    }

    #[test]
    fn closed_fence_is_not_reported() {
        assert_eq!(open_fence_at_end("```rust\nfn main() {}\n```"), None);
    }

    #[test]
    fn closed_fence_with_trailing_whitespace_on_closer_is_not_reported() {
        assert_eq!(open_fence_at_end("```\ncode\n```   "), None);
    }

    #[test]
    fn tilde_fence_opens_and_closes() {
        assert_eq!(open_fence_at_end("~~~py\nprint(1)\n~~~"), None);
        assert_eq!(
            open_fence_at_end("~~~py\nprint(1)"),
            Some(fence('~', 3, "py"))
        );
    }

    #[test]
    fn shorter_marker_run_does_not_close_longer_opener() {
        let text = "````\ncode\n```";
        assert_eq!(open_fence_at_end(text), Some(fence('`', 4, "")));
    }

    #[test]
    fn longer_marker_run_closes_shorter_opener() {
        assert_eq!(open_fence_at_end("```\ncode\n`````"), None);
    }

    #[test]
    fn mismatched_marker_does_not_close() {
        assert_eq!(open_fence_at_end("```\ncode\n~~~"), Some(fence('`', 3, "")));
    }

    #[test]
    fn opener_inside_open_fence_is_content() {
        // The inner ```text line is fence content, so after the closer
        // the document is back outside any fence.
        assert_eq!(open_fence_at_end("````md\n```text\nhi\n````"), None);
    }

    #[test]
    fn indented_four_spaces_is_not_a_fence() {
        assert_eq!(open_fence_at_end("    ```\ncode"), None);
    }

    #[test]
    fn indent_up_to_three_spaces_still_opens() {
        assert_eq!(open_fence_at_end("   ```sh\nls"), Some(fence('`', 3, "sh")));
    }

    #[test]
    fn backtick_info_containing_backtick_is_inline_code_not_a_fence() {
        assert_eq!(open_fence_at_end("``` `inline` ```\ntext"), None);
    }

    #[test]
    fn tilde_info_may_contain_backticks() {
        assert_eq!(
            open_fence_at_end("~~~ a`b\ncontent"),
            Some(fence('~', 3, "a`b"))
        );
    }

    #[test]
    fn reopened_fence_after_closed_one_is_reported() {
        let text = "```rust\nfn a() {}\n```\nprose\n```text\ncargo check";
        assert_eq!(open_fence_at_end(text), Some(fence('`', 3, "text")));
    }

    #[test]
    fn opener_as_final_line_without_newline_counts_as_open() {
        assert_eq!(
            open_fence_at_end("prose\n```text"),
            Some(fence('`', 3, "text"))
        );
    }

    #[test]
    fn closing_and_opening_lines_round_trip() {
        let f = fence('`', 4, "text");
        assert_eq!(f.closing_line(), "````");
        assert_eq!(f.opening_line(), "````text");
        let plain = fence('~', 3, "");
        assert_eq!(plain.closing_line(), "~~~");
        assert_eq!(plain.opening_line(), "~~~");
    }
}
