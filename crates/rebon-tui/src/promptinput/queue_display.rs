//! Pure layout helpers for rendering the queued-message banner above the
//! prompt input surface.
//!
//! The caller owns the final rendering; this module only computes the
//! display-ready data: a header line ("N queued message(s)") and the
//! individual stacked lines prefixed with `⎿`.

/// The stacking glyph used for each queued message line.
pub const QUEUE_STACK_GLYPH: &str = "\u{23BF}";

/// Maximum characters to show per queued message line before truncating.
const DEFAULT_MAX_LINE_WIDTH: usize = 60;

/// Single queued message to display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueDisplayItem {
    /// Display text for this queued message.
    pub text: String,
}

/// Input for building the queue display layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueDisplayInput {
    /// The queued messages to display (after filtering idle notifications, etc.).
    pub items: Vec<QueueDisplayItem>,
    /// Available width for truncating long messages. When `0`, the default
    /// cap ([`DEFAULT_MAX_LINE_WIDTH`]) is used.
    pub max_width: usize,
}

/// One rendered line in the queue display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueDisplayLine {
    /// The prefix glyph (`⎿`).
    pub glyph: &'static str,
    /// The display text (possibly truncated to the first line).
    pub text: String,
}

/// Resolved layout for the queued-message banner shown above the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueDisplayLayout {
    /// Whether the banner should be shown at all.
    pub visible: bool,
    /// Header text, e.g. `"3 queued message(s)"`.
    pub header: String,
    /// Individual stacked lines, one per queued message.
    pub lines: Vec<QueueDisplayLine>,
}

/// Build the queue display layout from the given queued items.
///
/// Returns a layout with `visible: false` when the item list is empty.
pub fn build_queue_display(input: &QueueDisplayInput) -> QueueDisplayLayout {
    if input.items.is_empty() {
        return QueueDisplayLayout {
            visible: false,
            header: String::new(),
            lines: vec![],
        };
    }

    let count = input.items.len();
    let header = format!(
        "{count} queued message{}",
        if count == 1 { "" } else { "s" }
    );

    let cap = if input.max_width > 0 {
        input.max_width
    } else {
        DEFAULT_MAX_LINE_WIDTH
    };

    let lines = input
        .items
        .iter()
        .map(|item| QueueDisplayLine {
            glyph: QUEUE_STACK_GLYPH,
            text: truncate_first_line(&item.text, cap),
        })
        .collect();

    QueueDisplayLayout {
        visible: true,
        header,
        lines,
    }
}

/// Extract the first line, trim it, and truncate to `max_chars` with `…`.
fn truncate_first_line(text: &str, max_chars: usize) -> String {
    let first_line = text.lines().next().unwrap_or(text).trim();
    if first_line.chars().count() <= max_chars {
        first_line.to_string()
    } else {
        let truncated: String = first_line
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        format!("{truncated}\u{2026}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str) -> QueueDisplayItem {
        QueueDisplayItem {
            text: text.to_string(),
        }
    }

    // ── visibility ──────────────────────────────────────────────

    #[test]
    fn empty_queue_produces_invisible_layout() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![],
            max_width: 0,
        });
        assert!(!layout.visible);
        assert!(layout.header.is_empty());
        assert!(layout.lines.is_empty());
    }

    #[test]
    fn non_empty_queue_is_visible() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item("hello")],
            max_width: 0,
        });
        assert!(layout.visible);
    }

    // ── header pluralization ────────────────────────────────────

    #[test]
    fn single_item_header_is_singular() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item("a")],
            max_width: 0,
        });
        assert_eq!(layout.header, "1 queued message");
    }

    #[test]
    fn multiple_items_header_is_plural() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item("a"), item("b"), item("c")],
            max_width: 0,
        });
        assert_eq!(layout.header, "3 queued messages");
    }

    // ── stacking lines ─────────────────────────────────────────

    #[test]
    fn each_item_produces_one_line_with_glyph() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item("first"), item("second")],
            max_width: 0,
        });
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(layout.lines[0].glyph, QUEUE_STACK_GLYPH);
        assert_eq!(layout.lines[0].text, "first");
        assert_eq!(layout.lines[1].glyph, QUEUE_STACK_GLYPH);
        assert_eq!(layout.lines[1].text, "second");
    }

    // ── truncation ──────────────────────────────────────────────

    #[test]
    fn short_text_is_not_truncated() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item("short")],
            max_width: 20,
        });
        assert_eq!(layout.lines[0].text, "short");
    }

    #[test]
    fn long_text_is_truncated_with_ellipsis() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item("abcdefghij")],
            max_width: 6,
        });
        // 5 chars + …
        assert_eq!(layout.lines[0].text, "abcde\u{2026}");
    }

    #[test]
    fn default_width_is_used_when_max_width_is_zero() {
        let long = "a".repeat(100);
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item(&long)],
            max_width: 0,
        });
        // DEFAULT_MAX_LINE_WIDTH is 60, so 59 chars + …
        assert_eq!(layout.lines[0].text.chars().count(), DEFAULT_MAX_LINE_WIDTH);
        assert!(layout.lines[0].text.ends_with('\u{2026}'));
    }

    #[test]
    fn exact_boundary_is_not_truncated() {
        let exact = "a".repeat(10);
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item(&exact)],
            max_width: 10,
        });
        assert_eq!(layout.lines[0].text, exact);
    }

    // ── multi-line input ────────────────────────────────────────

    #[test]
    fn only_first_line_is_shown_for_multiline_message() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item("line one\nline two\nline three")],
            max_width: 0,
        });
        assert_eq!(layout.lines[0].text, "line one");
    }

    #[test]
    fn leading_and_trailing_whitespace_is_trimmed() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item("  hello world  ")],
            max_width: 0,
        });
        assert_eq!(layout.lines[0].text, "hello world");
    }

    // ── unicode safety ──────────────────────────────────────────

    #[test]
    fn cjk_text_truncates_by_char_count() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item("你好世界你好世界你好世界")],
            max_width: 5,
        });
        assert_eq!(layout.lines[0].text, "你好世界\u{2026}");
    }

    #[test]
    fn emoji_text_truncates_safely() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![item("😀😀😀😀😀😀")],
            max_width: 4,
        });
        let text = &layout.lines[0].text;
        assert!(text.ends_with('\u{2026}'));
        assert!(!text.contains('\u{fffd}'));
    }

    // ── mixed scenario ──────────────────────────────────────────

    #[test]
    fn full_scenario_with_mixed_items() {
        let layout = build_queue_display(&QueueDisplayInput {
            items: vec![
                item("fix the login bug"),
                item("this is a very long message that should be truncated to fit within the display width"),
                item("  multiline\nsecond line  "),
            ],
            max_width: 30,
        });
        assert!(layout.visible);
        assert_eq!(layout.header, "3 queued messages");
        assert_eq!(layout.lines.len(), 3);
        assert_eq!(layout.lines[0].text, "fix the login bug");
        assert_eq!(
            layout.lines[1].text,
            "this is a very long message t\u{2026}"
        );
        assert_eq!(layout.lines[2].text, "multiline");
    }
}
