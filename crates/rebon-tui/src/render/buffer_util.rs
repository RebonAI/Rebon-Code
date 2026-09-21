use ratatui::{buffer::Buffer, layout::Rect};

/// Reset every cell in `area` to ratatui's default state — empty
/// symbol, no fg/bg, no modifiers. This prevents character residue
/// when wrapped lines have a dim/empty gutter or when the rendered
/// content is shorter than the viewport. We deliberately do NOT
/// apply the theme's `background` token: forcing a bg color on
/// every cell makes the transcript chrome clash with the user's
/// terminal background. Cells that aren't subsequently painted
/// inherit the terminal's natural background instead.
pub fn clear_buffer_area(buf: &mut Buffer, area: Rect) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier, Style},
    };

    use crate::message::{
        AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
        AssistantTextBlock, Message,
    };
    use crate::state::{reducer, Action, AppState};

    use super::super::{
        render_transcript_cached, RenderTheme, ToolOutputVerbosity, TranscriptMeasureCache,
    };

    fn new_buf(w: u16, h: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, w, h))
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        let mut s = String::new();
        for x in 0..buf.area().width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.trim_end().to_string()
    }

    fn assistant_text(uuid: &str, text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::Text(AssistantTextBlock {
                    text: text.into(),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    /// Helper: collect all non-blank text from a buffer into a single string.
    fn all_text(buf: &Buffer) -> String {
        (0..buf.area().height)
            .map(|y| row_text(buf, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn dirty_style() -> Style {
        Style::new()
            .fg(Color::Yellow)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    }

    fn fill_area(buf: &mut Buffer, area: Rect, symbol: &str, style: Style) {
        for y in area.y..area.y.saturating_add(area.height) {
            for x in area.x..area.x.saturating_add(area.width) {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.reset();
                    cell.set_symbol(symbol);
                    cell.set_style(style);
                }
            }
        }
    }

    fn assert_area_has_no_dirty_cells(buf: &Buffer, area: Rect) {
        for y in area.y..area.y.saturating_add(area.height) {
            for x in area.x..area.x.saturating_add(area.width) {
                let cell = buf.cell((x, y)).expect("cell inside test buffer");
                assert_ne!(cell.symbol(), "█", "dirty glyph remained at ({x}, {y})");
                assert_ne!(cell.bg, Color::Magenta, "dirty bg remained at ({x}, {y})");
            }
        }
    }

    fn assert_area_blank_reset(buf: &Buffer, area: Rect) {
        for y in area.y..area.y.saturating_add(area.height) {
            for x in area.x..area.x.saturating_add(area.width) {
                let cell = buf.cell((x, y)).expect("cell inside test buffer");
                assert_eq!(cell.symbol(), " ", "cell at ({x}, {y}) was not blank");
                assert_eq!(cell.fg, Color::Reset, "cell fg at ({x}, {y}) was not reset");
                assert_eq!(cell.bg, Color::Reset, "cell bg at ({x}, {y}) was not reset");
                assert_eq!(
                    cell.modifier,
                    Modifier::empty(),
                    "cell modifiers at ({x}, {y}) were not reset"
                );
            }
        }
    }

    #[test]
    fn render_transcript_clears_dirty_buffer_wrapped_gutter() {
        let width = 18;
        let height = 6;
        let area = Rect::new(0, 0, width, height);
        let theme = RenderTheme::plain();
        let mut state = AppState::new();
        reducer(
            &mut state,
            Action::Commit(assistant_text(
                "a-long",
                "one two three four five six seven eight nine ten eleven twelve",
            )),
        );

        let mut buf = new_buf(width, height);
        fill_area(&mut buf, area, "█", dirty_style());
        let mut cache = TranscriptMeasureCache::new();
        let result = render_transcript_cached(
            &state,
            area,
            &mut buf,
            &theme,
            0,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
        );

        assert!(result.total_lines > 1, "fixture must wrap: {result:?}");
        assert_area_has_no_dirty_cells(&buf, area);
        assert_eq!(buf[(0, 1)].symbol(), " ");
        assert_eq!(buf[(1, 1)].symbol(), " ");
        assert_eq!(buf[(0, 1)].bg, Color::Reset);
        assert_eq!(buf[(1, 1)].bg, Color::Reset);
    }

    #[test]
    fn render_transcript_clears_dirty_buffer_bottom_blank_rows() {
        let width = 32;
        let height = 8;
        let area = Rect::new(0, 0, width, height);
        let theme = RenderTheme::plain();
        let mut state = AppState::new();
        reducer(
            &mut state,
            Action::Commit(assistant_text(
                "a-old",
                "obsolete text ".repeat(40).as_str(),
            )),
        );

        let mut buf = new_buf(width, height);
        let mut cache = TranscriptMeasureCache::new();
        render_transcript_cached(
            &state,
            area,
            &mut buf,
            &theme,
            0,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
        );
        assert!(all_text(&buf).contains("obsolete"));

        state.transcript.clear();
        reducer(&mut state, Action::Commit(assistant_text("a-new", "short")));
        let result = render_transcript_cached(
            &state,
            area,
            &mut buf,
            &theme,
            0,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
        );

        assert!(
            result.render_y_end < area.y.saturating_add(area.height),
            "fixture must leave blank rows: {result:?}"
        );
        assert!(!all_text(&buf).contains("obsolete"));
        assert_area_blank_reset(
            &buf,
            Rect::new(
                area.x,
                result.render_y_end,
                area.width,
                area.y.saturating_add(area.height) - result.render_y_end,
            ),
        );
    }

    #[test]
    fn render_transcript_clears_dirty_buffer_empty_transcript() {
        let area = Rect::new(2, 1, 5, 3);
        let theme = RenderTheme::plain();
        let state = AppState::new();
        let mut buf = new_buf(10, 6);
        let full_area = *buf.area();
        fill_area(&mut buf, full_area, "█", dirty_style());
        let mut cache = TranscriptMeasureCache::new();

        let result = render_transcript_cached(
            &state,
            area,
            &mut buf,
            &theme,
            0,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
        );

        assert_eq!(result.total_lines, 0);
        assert_eq!(result.render_y_end, area.y);
        assert_eq!(result.suffix_skip_lines, 0);
        assert_area_blank_reset(&buf, area);
        assert_eq!(buf[(0, 0)].symbol(), "█");
        assert_eq!(buf[(0, 0)].bg, Color::Magenta);
    }
}
