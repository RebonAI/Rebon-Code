use std::collections::VecDeque;

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Paragraph, Widget, Wrap},
};
use rebon_width::WidthStr;
use unicode_segmentation::UnicodeSegmentation;

/// Gutter label column width (`"● "`, `"◆ "`, `"❯ "` = 2 columns).
pub const GUTTER_WIDTH: u16 = 2;

pub(in crate::widget_subtree) fn gutter_label(prefix: &'static str, style: Style) -> Line<'static> {
    Line::from(Span::styled(format!("{prefix} "), style))
}

pub(in crate::widget_subtree) fn measure_text_height(text: &Text<'static>, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    text.lines
        .iter()
        .map(|line| line_row_height(line, width))
        .sum()
}

pub(in crate::widget_subtree) fn split_gutter(area: Rect) -> (Rect, Rect) {
    if area.width <= GUTTER_WIDTH {
        (area, Rect::new(area.x, area.y, 0, area.height))
    } else {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(GUTTER_WIDTH), Constraint::Min(0)])
            .split(area);
        (chunks[0], chunks[1])
    }
}

pub(in crate::widget_subtree) fn paint_line_into(
    area: Rect,
    buf: &mut Buffer,
    line: Line<'static>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    Paragraph::new(Text::from(line))
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

pub(in crate::widget_subtree) fn paint_text_into(
    area: Rect,
    buf: &mut Buffer,
    text: Text<'static>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

pub(in crate::widget_subtree) fn line_row_height(line: &Line<'static>, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    let mut wrapped = 0u16;
    for line_text in line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
        .lines()
    {
        wrapped = wrapped.saturating_add(wrapped_line_height(line_text, width));
    }
    wrapped.max(1)
}

fn wrapped_line_height(line: &str, width: u16) -> u16 {
    let mut wrapped = 0u16;
    let mut line_width = 0u16;
    let mut word_width = 0u16;
    let mut whitespace_width = 0u16;
    let mut pending_whitespace = VecDeque::new();
    let mut saw_grapheme = false;
    let mut non_whitespace_previous = false;

    for grapheme in line.graphemes(true) {
        let symbol_width = WidthStr::width(grapheme) as u16;
        if symbol_width > width {
            continue;
        }
        saw_grapheme = true;
        let is_whitespace = grapheme == "\u{200b}"
            || (grapheme.chars().all(char::is_whitespace) && grapheme != "\u{00a0}");
        let word_found = non_whitespace_previous && is_whitespace;
        let untrimmed_overflow = line_width == 0
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(symbol_width)
                > width;

        if word_found || untrimmed_overflow {
            line_width = line_width.saturating_add(whitespace_width);
            line_width = line_width.saturating_add(word_width);
            pending_whitespace.clear();
            whitespace_width = 0;
            word_width = 0;
        }

        if line_width >= width
            || (symbol_width > 0
                && line_width
                    .saturating_add(whitespace_width)
                    .saturating_add(word_width)
                    >= width)
        {
            wrapped = wrapped.saturating_add(1);
            line_width = 0;
            let mut remaining_width = width;
            while let Some(width) = pending_whitespace.front().copied() {
                if width > remaining_width {
                    break;
                }
                whitespace_width = whitespace_width.saturating_sub(width);
                remaining_width = remaining_width.saturating_sub(width);
                pending_whitespace.pop_front();
            }
            if is_whitespace && pending_whitespace.is_empty() {
                non_whitespace_previous = false;
                continue;
            }
        }

        if is_whitespace {
            whitespace_width = whitespace_width.saturating_add(symbol_width);
            pending_whitespace.push_back(symbol_width);
        } else {
            word_width = word_width.saturating_add(symbol_width);
        }
        non_whitespace_previous = !is_whitespace;
    }

    if !saw_grapheme {
        return 1;
    }
    if line_width > 0 || word_width > 0 || whitespace_width > 0 {
        wrapped = wrapped.saturating_add(1);
    }
    wrapped.max(1)
}
