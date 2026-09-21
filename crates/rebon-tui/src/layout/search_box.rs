//! Search-box display projection.
//!
//! [`project_search_box`] is pure data: it takes a [`SearchBoxInput`]
//! (query text, optional placeholder / prefix / width, the two focus
//! flags, and an optional cursor offset) and computes a box frame
//! plus a text-segment sequence that branches six ways:
//!
//! 1. focused + terminal-focused + has query → cursor carved into text
//! 2. focused + terminal-focused + no query → first-char-inverse on
//!    placeholder
//! 3. focused + not terminal-focused + has query → plain query
//! 4. focused + not terminal-focused + no query → dim placeholder
//! 5. not focused + has query → plain query
//! 6. not focused + no query → dim placeholder
//!
//! Defaults: `placeholder = "Search…"` (U+2026), `prefix = "⌕"` (U+2315),
//! `borderless = false`. When `cursor_offset` is `None` it falls back to
//! the query's character count.
//!
//! This module returns a semantic [`SearchBoxDisplay`] struct — the
//! consumer decides how to draw the dimming, the inverse run, and the
//! border. The frame (`border_style`, `border_color`, `border_dim`,
//! `padding_x`, `width`) is exposed as a [`SearchBoxFrame`] so the
//! consumer can reuse its own border widget.

/// Default placeholder text (`"Search…"` — U+2026, one character).
pub const DEFAULT_PLACEHOLDER: &str = "Search\u{2026}";

/// Default prefix glyph (`"⌕"` — U+2315).
pub const DEFAULT_PREFIX: &str = "\u{2315}";

/// Optional width setting for the outer box: either a fixed cell count
/// or a string the consumer's layout engine resolves — modeled as a
/// tiny enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoxWidth {
    /// Fixed cell count (20 cells).
    Cells(u16),
    /// A percentage such as `"50%"`, or any other width string. We do
    /// not interpret the string here — the consumer's layout engine does.
    Flex(String),
}

/// Frame chrome for the search box's outer border. Computed as:
///
/// * `border_style` — `Some("round")` unless `borderless`
/// * `border_color` — `Some("suggestion")` when `is_focused`, else `None`
/// * `border_dim` — the negation of `is_focused`
/// * `padding_x` — `0` when `borderless`, else `1`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchBoxFrame {
    /// `Some("round")` unless `borderless` is set.
    pub border_style: Option<&'static str>,
    /// `Some("suggestion")` when focused.
    pub border_color: Option<&'static str>,
    /// `true` when not focused.
    pub border_dim: bool,
    /// `0` when borderless, `1` otherwise.
    pub padding_x: u16,
    /// Pass-through width.
    pub width: Option<BoxWidth>,
}

/// A single text segment in the inner display. Semantic styling flags
/// (`dim`, `inverse`) let the consumer produce either styled terminal
/// text, ANSI strings, or a ratatui `Span`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSegment {
    pub text: String,
    pub dim: bool,
    pub inverse: bool,
}

impl TextSegment {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            dim: false,
            inverse: false,
        }
    }
    fn dim(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            dim: true,
            inverse: false,
        }
    }
    fn inverse(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            dim: false,
            inverse: true,
        }
    }
}

/// A semantic projection of the inner text (dimmed when
/// `!is_focused`) — the prefix, then a space, then the four-branch
/// content segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchBoxDisplay {
    /// The outer border chrome.
    pub frame: SearchBoxFrame,
    /// Segments of the inner text. The outer text is dim when
    /// `!is_focused` — the consumer should apply that globally, and
    /// individual segments carry their own `dim` / `inverse` overrides.
    pub outer_dim: bool,
    pub segments: Vec<TextSegment>,
}

/// Inputs to the projection. `cursor_offset = None` means "default to
/// `query.len()`".
#[derive(Debug, Clone)]
pub struct SearchBoxInput {
    pub query: String,
    pub placeholder: Option<String>,
    pub is_focused: bool,
    pub is_terminal_focused: bool,
    pub prefix: Option<String>,
    pub width: Option<BoxWidth>,
    pub cursor_offset: Option<usize>,
    pub borderless: bool,
}

/// Pure projection from [`SearchBoxInput`] to [`SearchBoxDisplay`].
pub fn project_search_box(input: &SearchBoxInput) -> SearchBoxDisplay {
    let placeholder = input
        .placeholder
        .clone()
        .unwrap_or_else(|| DEFAULT_PLACEHOLDER.to_string());
    let prefix = input
        .prefix
        .clone()
        .unwrap_or_else(|| DEFAULT_PREFIX.to_string());
    let borderless = input.borderless;
    // Cursor carving counts `char` positions, not bytes, so the offset
    // always lands on a character boundary. Chars are the deterministic
    // choice for ASCII and for emoji alike.
    let offset = input
        .cursor_offset
        .unwrap_or_else(|| input.query.chars().count());

    let frame = SearchBoxFrame {
        border_style: if borderless { None } else { Some("round") },
        border_color: if input.is_focused {
            Some("suggestion")
        } else {
            None
        },
        border_dim: !input.is_focused,
        padding_x: if borderless { 0 } else { 1 },
        width: input.width.clone(),
    };

    let mut segments: Vec<TextSegment> = Vec::new();
    // `{prefix}{" "}` — shared across all branches.
    segments.push(TextSegment::plain(prefix));
    segments.push(TextSegment::plain(" "));

    if input.is_focused {
        // Focused branch.
        if !input.query.is_empty() {
            if input.is_terminal_focused {
                // query with cursor carve-out
                let chars: Vec<char> = input.query.chars().collect();
                let offset = offset.min(chars.len());
                let before: String = chars[..offset].iter().collect();
                segments.push(TextSegment::plain(before));
                if offset < chars.len() {
                    let cursor: String = chars[offset].to_string();
                    segments.push(TextSegment::inverse(cursor));
                    let after: String = chars[offset + 1..].iter().collect();
                    segments.push(TextSegment::plain(after));
                } else {
                    // At end of query: render " " inverse.
                    segments.push(TextSegment::inverse(" "));
                }
            } else {
                // query, no terminal focus: plain
                segments.push(TextSegment::plain(input.query.clone()));
            }
        } else {
            // Empty query, focused
            if input.is_terminal_focused {
                let mut ch_iter = placeholder.chars();
                if let Some(first) = ch_iter.next() {
                    segments.push(TextSegment::inverse(first.to_string()));
                    let rest: String = ch_iter.collect();
                    segments.push(TextSegment::dim(rest));
                } else {
                    // Empty placeholder — nothing to show.
                }
            } else {
                segments.push(TextSegment::dim(placeholder));
            }
        }
    } else {
        // Not focused
        if !input.query.is_empty() {
            segments.push(TextSegment::plain(input.query.clone()));
        } else {
            segments.push(TextSegment::plain(placeholder));
        }
    }

    SearchBoxDisplay {
        frame,
        outer_dim: !input.is_focused,
        segments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SearchBoxInput {
        SearchBoxInput {
            query: String::new(),
            placeholder: None,
            is_focused: false,
            is_terminal_focused: false,
            prefix: None,
            width: None,
            cursor_offset: None,
            borderless: false,
        }
    }

    #[test]
    fn default_placeholder_is_search_ellipsis() {
        assert_eq!(DEFAULT_PLACEHOLDER, "Search\u{2026}");
        assert_eq!(DEFAULT_PLACEHOLDER.chars().count(), 7);
    }

    #[test]
    fn default_prefix_is_u2315() {
        assert_eq!(DEFAULT_PREFIX, "\u{2315}");
    }

    #[test]
    fn default_frame_has_round_border_and_padding_1() {
        let d = project_search_box(&base());
        assert_eq!(d.frame.border_style, Some("round"));
        assert_eq!(d.frame.padding_x, 1);
        assert!(d.frame.border_dim);
        assert_eq!(d.frame.border_color, None);
    }

    #[test]
    fn borderless_clears_border_and_padding() {
        let mut i = base();
        i.borderless = true;
        let d = project_search_box(&i);
        assert_eq!(d.frame.border_style, None);
        assert_eq!(d.frame.padding_x, 0);
    }

    #[test]
    fn focused_sets_suggestion_border_and_not_dim() {
        let mut i = base();
        i.is_focused = true;
        let d = project_search_box(&i);
        assert_eq!(d.frame.border_color, Some("suggestion"));
        assert!(!d.frame.border_dim);
        assert!(!d.outer_dim);
    }

    #[test]
    fn unfocused_empty_query_renders_plain_placeholder() {
        let d = project_search_box(&base());
        let last = d.segments.last().unwrap();
        assert_eq!(last.text, DEFAULT_PLACEHOLDER);
        assert!(!last.dim);
        assert!(!last.inverse);
        // outer dim is true so consumer applies dim globally
        assert!(d.outer_dim);
    }

    #[test]
    fn unfocused_with_query_renders_plain_query() {
        let mut i = base();
        i.query = "hello".into();
        let d = project_search_box(&i);
        let last = d.segments.last().unwrap();
        assert_eq!(last.text, "hello");
        assert!(!last.dim);
    }

    #[test]
    fn focused_no_term_empty_renders_dim_placeholder() {
        let mut i = base();
        i.is_focused = true;
        let d = project_search_box(&i);
        let last = d.segments.last().unwrap();
        assert_eq!(last.text, DEFAULT_PLACEHOLDER);
        assert!(last.dim);
    }

    #[test]
    fn focused_term_empty_splits_placeholder_inverse_first() {
        let mut i = base();
        i.is_focused = true;
        i.is_terminal_focused = true;
        let d = project_search_box(&i);
        // segments: [prefix, " ", inverse("S"), dim("earch…")]
        assert_eq!(d.segments[2].text, "S");
        assert!(d.segments[2].inverse);
        assert_eq!(d.segments[3].text, "earch\u{2026}");
        assert!(d.segments[3].dim);
    }

    #[test]
    fn focused_term_with_query_and_cursor_in_middle_carves_cursor() {
        let mut i = base();
        i.is_focused = true;
        i.is_terminal_focused = true;
        i.query = "hello".into();
        i.cursor_offset = Some(2);
        let d = project_search_box(&i);
        // [prefix, " ", "he", inverse("l"), "lo"]
        assert_eq!(d.segments[2].text, "he");
        assert_eq!(d.segments[3].text, "l");
        assert!(d.segments[3].inverse);
        assert_eq!(d.segments[4].text, "lo");
    }

    #[test]
    fn focused_term_cursor_at_end_shows_inverse_space() {
        let mut i = base();
        i.is_focused = true;
        i.is_terminal_focused = true;
        i.query = "abc".into();
        // default cursor offset at end
        let d = project_search_box(&i);
        // [prefix, " ", "abc", inverse(" ")]
        assert_eq!(d.segments[2].text, "abc");
        assert_eq!(d.segments[3].text, " ");
        assert!(d.segments[3].inverse);
    }

    #[test]
    fn focused_no_term_with_query_renders_plain_query() {
        let mut i = base();
        i.is_focused = true;
        i.query = "world".into();
        let d = project_search_box(&i);
        let last = d.segments.last().unwrap();
        assert_eq!(last.text, "world");
        assert!(!last.inverse);
        assert!(!last.dim);
    }

    #[test]
    fn cursor_offset_default_is_query_len() {
        let mut i = base();
        i.is_focused = true;
        i.is_terminal_focused = true;
        i.query = "ab".into();
        let d = project_search_box(&i);
        // Should carve past end → inverse space
        assert_eq!(d.segments[2].text, "ab");
        assert_eq!(d.segments[3].text, " ");
        assert!(d.segments[3].inverse);
    }

    #[test]
    fn custom_placeholder_honored() {
        let mut i = base();
        i.placeholder = Some("Query name".into());
        let d = project_search_box(&i);
        let last = d.segments.last().unwrap();
        assert_eq!(last.text, "Query name");
    }

    #[test]
    fn custom_prefix_honored() {
        let mut i = base();
        i.prefix = Some(">".into());
        let d = project_search_box(&i);
        assert_eq!(d.segments[0].text, ">");
    }

    #[test]
    fn width_pass_through_cells() {
        let mut i = base();
        i.width = Some(BoxWidth::Cells(40));
        let d = project_search_box(&i);
        assert_eq!(d.frame.width, Some(BoxWidth::Cells(40)));
    }

    #[test]
    fn width_pass_through_flex() {
        let mut i = base();
        i.width = Some(BoxWidth::Flex("50%".into()));
        let d = project_search_box(&i);
        assert_eq!(d.frame.width, Some(BoxWidth::Flex("50%".into())));
    }

    #[test]
    fn cursor_offset_past_end_clamps_to_end() {
        let mut i = base();
        i.is_focused = true;
        i.is_terminal_focused = true;
        i.query = "ab".into();
        i.cursor_offset = Some(999);
        let d = project_search_box(&i);
        // Clamped to end → inverse space
        assert_eq!(d.segments[2].text, "ab");
        assert!(d.segments[3].inverse);
    }

    #[test]
    fn empty_placeholder_focused_term_is_safe() {
        let mut i = base();
        i.placeholder = Some(String::new());
        i.is_focused = true;
        i.is_terminal_focused = true;
        let d = project_search_box(&i);
        // Only prefix + space, no cursor split (empty placeholder)
        assert_eq!(d.segments.len(), 2);
    }

    #[test]
    fn unicode_query_carves_correctly() {
        let mut i = base();
        i.is_focused = true;
        i.is_terminal_focused = true;
        i.query = "café".into();
        i.cursor_offset = Some(3);
        let d = project_search_box(&i);
        assert_eq!(d.segments[2].text, "caf");
        assert_eq!(d.segments[3].text, "é");
        assert!(d.segments[3].inverse);
    }

    #[test]
    fn focused_term_cursor_offset_zero_first_char_inverse() {
        let mut i = base();
        i.is_focused = true;
        i.is_terminal_focused = true;
        i.query = "xyz".into();
        i.cursor_offset = Some(0);
        let d = project_search_box(&i);
        // [prefix, " ", "", inverse("x"), "yz"]
        assert_eq!(d.segments[2].text, "");
        assert_eq!(d.segments[3].text, "x");
        assert!(d.segments[3].inverse);
        assert_eq!(d.segments[4].text, "yz");
    }
}
