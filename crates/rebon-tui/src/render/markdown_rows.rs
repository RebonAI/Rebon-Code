//! Markdown as the exact rows a non-wrapping surface paints.

use ratatui::style::Style;
use ratatui::text::Line;

use super::parse_theme_color;

/// Render `src` as markdown in the active palette and wrap every line to
/// `width` columns, so a surface that paints with a plain, non-wrapping
/// paragraph can both draw these rows and count them for its height.
pub fn markdown_rows(src: &str, width: usize) -> Vec<Line<'static>> {
    let ds = rebon_design_system::theme::get_active_theme();
    let width = width.max(1);
    let theme =
        rebon_message_tui::MarkdownTheme::from_messages(&rebon_message_tui::MessagesRenderTheme {
            text: Style::default(),
            dim: Style::default().fg(parse_theme_color(ds.inactive)),
            error: Style::default().fg(parse_theme_color(ds.error)),
            warning: Style::default().fg(parse_theme_color(ds.warning)),
            accent: Style::default().fg(parse_theme_color(ds.permission)),
        });
    rebon_message_tui::render_markdown_blocks_with_width(src, &theme, width)
        .lines
        .into_iter()
        .flat_map(|line| rebon_message_tui::wrap_styled_line(line, width))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter()
            .map(|row| row.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn markdown_is_rendered_not_echoed() {
        let rows = markdown_rows("## Plan\n\n- change `len()`\n- run **tests**", 40);
        let rows = text(&rows);
        assert!(rows.iter().any(|row| row == "Plan"), "{rows:?}");
        // Inline code sits on a chip padded a column each side.
        assert!(
            rows.iter().any(|row| row.trim_end() == "• change  len()"),
            "{rows:?}"
        );
        assert!(rows
            .iter()
            .all(|row| !row.contains('`') && !row.contains("**")));
    }

    #[test]
    fn every_row_fits_the_width() {
        let src = "A paragraph long enough that it has to wrap across several rows of text.\n\n\
                   - a list item that is also long enough to wrap onto a second row";
        for width in [12usize, 20, 33] {
            for row in text(&markdown_rows(src, width)) {
                assert!(rebon_width::str_width(&row) <= width, "{width}: {row:?}");
            }
        }
        assert!(!markdown_rows("x", 0).is_empty());
    }
}
