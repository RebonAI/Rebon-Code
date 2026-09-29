//! A file change as the rows of the diff it applies, for surfaces that paint
//! line by line (the permission prompt) instead of into a tool card.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use rebon_message_tui::{diff_lines_to_text, MessagesRenderTheme};
use rebon_render::fold_long_diff_runs;

/// The diff that turns `old_text` into `new_text`, drawn exactly as a
/// completed Update card draws it — same numbering, word highlights and
/// folding — indented two columns, one row per line, at most `max_rows` rows.
/// A longer diff ends in a `… +N lines` row, so the caller can count rows
/// without wrapping them.
///
/// `original_file` is the file before the change. With it the diff carries
/// real line numbers and three lines of context; without it the change is
/// numbered from 1.
pub fn file_change_diff_rows(
    old_text: Option<&str>,
    new_text: &str,
    original_file: Option<&str>,
    width: u16,
    max_rows: usize,
    hint_style: Style,
) -> Vec<Line<'static>> {
    let (diff_lines, start_line) =
        rebon_render::build_context_diff(old_text, new_text, original_file);
    if diff_lines.is_empty() || width == 0 {
        return Vec::new();
    }
    let content_width = width.saturating_sub(super::GUTTER) as usize;
    let wrap_fn = |s: &str, _w: usize| -> Vec<String> { vec![s.to_string()] };
    let word_diff_fn = rebon_render::calculate_word_diff;
    let options = rebon_render::FormatOptions {
        width: content_width,
        dim: false,
        wrap: &wrap_fn,
        word_diff: &word_diff_fn,
    };
    let rendered = rebon_render::format_diff_lines(&diff_lines, start_line, &options);
    let rendered = fold_long_diff_runs(rendered, content_width);
    let text = diff_lines_to_text(&rendered, &MessagesRenderTheme::default_styled());

    let total = text.lines.len();
    let keep = if total > max_rows {
        max_rows.saturating_sub(1)
    } else {
        total
    };
    let mut rows: Vec<Line<'static>> = text
        .lines
        .into_iter()
        .take(keep)
        .map(|mut line| {
            line.spans.insert(0, Span::raw("  "));
            line
        })
        .collect();
    if total > keep {
        rows.push(Line::from(Span::styled(
            format!("  … +{} lines", total - keep),
            hint_style,
        )));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                row.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .map(|row| row.trim_end().to_string())
            .collect()
    }

    const ORIGINAL: &str = "a\nb\nc\nd\nlet x = old;\ne\nf\ng\nh\n";

    #[test]
    fn an_edit_is_numbered_from_the_file_with_context() {
        let rows = file_change_diff_rows(
            Some("let x = old;"),
            "let x = new;",
            Some(ORIGINAL),
            60,
            20,
            Style::default(),
        );
        let text = plain(&rows);
        assert!(
            text.iter()
                .any(|row| row.contains("5 -") && row.contains("old")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|row| row.contains("5 +") && row.contains("new")),
            "{text:?}"
        );
        // Three lines of context either side.
        assert!(text.iter().any(|row| row.contains("2  b")), "{text:?}");
        assert!(text.iter().any(|row| row.contains("8  g")), "{text:?}");
        assert!(text.iter().all(|row| row.starts_with("  ")), "{text:?}");
    }

    #[test]
    fn a_new_file_is_all_additions_from_line_one() {
        let rows = file_change_diff_rows(None, "one\ntwo\n", None, 40, 20, Style::default());
        let text = plain(&rows);
        assert_eq!(text.len(), 2, "{text:?}");
        assert!(
            text[0].contains("1 +") && text[0].contains("one"),
            "{text:?}"
        );
        assert!(
            text[1].contains("2 +") && text[1].contains("two"),
            "{text:?}"
        );
    }

    #[test]
    fn a_long_diff_is_capped_with_a_count_of_what_is_hidden() {
        let new_text: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let rows = file_change_diff_rows(None, &new_text, None, 40, 10, Style::default());
        assert_eq!(rows.len(), 10);
        let last = plain(&rows).pop().unwrap();
        assert!(last.starts_with("  … +"), "{last:?}");
        assert!(file_change_diff_rows(None, "x", None, 0, 10, Style::default()).is_empty());
    }
}
