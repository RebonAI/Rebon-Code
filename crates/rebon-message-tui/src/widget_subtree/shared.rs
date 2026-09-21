use crate::projection_render::{parse_theme_color, MessagesRenderTheme};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Widget},
};
use rebon_design_system::theme;
use rebon_width::WidthStr;

use crate::render::MessageRenderTheme;

use super::layout::{line_row_height, measure_text_height, paint_line_into, paint_text_into};

/// A bordered, titled block with either a single-column or two-column body,
/// rendered via `Block::bordered().title(...).title_bottom(...)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::widget_subtree) struct BorderedBlock {
    pub(in crate::widget_subtree) title: String,
    pub(in crate::widget_subtree) title_style: Style,
    pub(in crate::widget_subtree) title_bottom: Option<String>,
    pub(in crate::widget_subtree) title_bottom_style: Style,
    pub(in crate::widget_subtree) body: BorderedBlockBody,
    pub(in crate::widget_subtree) border_style: Style,
}

/// Body layout for a [`BorderedBlock`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::widget_subtree) enum BorderedBlockBody {
    /// A single-column text body.
    SingleColumn(Text<'static>),
}

impl BorderedBlock {
    pub(in crate::widget_subtree) fn height(&self, width: u16) -> u16 {
        if width <= 2 {
            return 2;
        }
        let inner_width = width.saturating_sub(2).max(1);
        match &self.body {
            BorderedBlockBody::SingleColumn(body) => measure_text_height(body, inner_width) + 2,
        }
    }

    pub(in crate::widget_subtree) fn paint(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(self.border_style)
            .title(Span::styled(self.title, self.title_style));
        if let Some(bottom) = self.title_bottom {
            block = block.title_bottom(
                Line::from(Span::styled(bottom, self.title_bottom_style)).right_aligned(),
            );
        }
        let inner = block.inner(area);
        block.render(area, buf);
        match self.body {
            BorderedBlockBody::SingleColumn(body) => {
                paint_text_into(inner, buf, body);
            }
        }
    }
}

/// One paintable sub-row inside a typed widget body.
#[derive(Debug, Clone)]
pub(in crate::widget_subtree) enum BodyRow {
    /// A single styled line.
    Line(Line<'static>),
    /// A line indented by `indent` columns inside the content column.
    Indented { indent: u16, line: Line<'static> },
    /// A blank row used for visual breathing.
    Blank,
    /// Header-then-body transcript block; the body is indented by
    /// `body_padding` columns.
    TranscriptBlock {
        header: Line<'static>,
        body: Text<'static>,
        body_padding: u16,
    },
}

impl BodyRow {
    pub(in crate::widget_subtree) fn height(&self, width: u16) -> u16 {
        if width == 0 {
            return 0;
        }
        match self {
            Self::Line(line) => line_row_height(line, width),
            Self::Indented { indent, line } => {
                let content_width = width.saturating_sub(*indent).max(1);
                line_row_height(line, content_width)
            }
            Self::Blank => 1,
            Self::TranscriptBlock {
                header,
                body,
                body_padding,
            } => {
                let header_h = line_row_height(header, width);
                let body_width = width.saturating_sub(*body_padding).max(1);
                let body_h = measure_text_height(body, body_width).max(1);
                header_h + body_h
            }
        }
    }

    pub(in crate::widget_subtree) fn paint(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        match self {
            Self::Line(line) => paint_line_into(area, buf, line),
            Self::Indented { indent, line } => {
                let indent = indent.min(area.width);
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(indent), Constraint::Min(0)])
                    .split(area);
                paint_line_into(chunks[1], buf, line);
            }
            Self::Blank => {}
            Self::TranscriptBlock {
                header,
                body,
                body_padding,
            } => {
                let header_h = line_row_height(&header, area.width);
                let header_area = Rect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: header_h.min(area.height),
                };
                paint_line_into(header_area, buf, header);
                if header_h >= area.height {
                    return;
                }
                let body_area = Rect {
                    x: area.x,
                    y: area.y.saturating_add(header_h),
                    width: area.width,
                    height: area.height.saturating_sub(header_h),
                };
                let indent = body_padding.min(body_area.width);
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(indent), Constraint::Min(0)])
                    .split(body_area);
                paint_text_into(chunks[1], buf, body);
            }
        }
    }
}

pub(in crate::widget_subtree) fn bordered_text_block(
    title: String,
    body: &str,
    body_style: Style,
    theme: &MessagesRenderTheme,
) -> BorderedBlock {
    let line_count = body.lines().count().max(1);
    let title_style = if body_style == theme.error {
        theme.error
    } else if body_style == theme.warning {
        theme.warning
    } else {
        theme.accent
    };
    BorderedBlock {
        title,
        title_style,
        title_bottom: Some(format!(" {line_count} lines ")),
        title_bottom_style: theme.dim,
        body: BorderedBlockBody::SingleColumn(Text::from(
            body.lines()
                .map(|line| Line::from(Span::styled(line.to_owned(), body_style)))
                .collect::<Vec<_>>(),
        )),
        border_style: title_style,
    }
}

/// Derive the message-body child theme from the top-level render theme.
pub fn child_theme_for(theme: &MessageRenderTheme) -> MessagesRenderTheme {
    MessagesRenderTheme {
        text: theme.assistant_text,
        dim: theme.assistant,
        error: theme.error,
        warning: theme.hint,
        // Tool names use default text plus bold, not user/suggestion color.
        accent: theme.assistant_text.add_modifier(Modifier::BOLD),
    }
}

/// Derive the assistant text/connector child theme with the dedicated markdown
/// accent while leaving tool-subtree accents unchanged.
pub(crate) fn assistant_text_child_theme_for(theme: &MessageRenderTheme) -> MessagesRenderTheme {
    MessagesRenderTheme {
        accent: theme.markdown_accent,
        ..child_theme_for(theme)
    }
}

/// Convenience that returns an accent-heavy theme with solid fills for cases
/// where callers want a saturated attachment/grouped header.
pub fn accent_theme() -> MessagesRenderTheme {
    let t = theme::get_active_theme();
    MessagesRenderTheme {
        text: Style::new(),
        dim: Style::new().fg(parse_theme_color(t.inactive)),
        error: Style::new()
            .fg(parse_theme_color(t.error))
            .add_modifier(Modifier::BOLD),
        warning: Style::new().fg(parse_theme_color(t.warning)),
        accent: Style::new()
            .fg(parse_theme_color(t.suggestion))
            .add_modifier(Modifier::BOLD),
    }
}

/// A block fenced by a titled rule above and a rule below, with no side borders.
///
/// [`BorderedBlock`] boxes its body on all four sides, which puts a vertical
/// line immediately right of the gutter glyph — two lines down the left edge of
/// one block, and the body inset by two more columns. This draws only the two
/// rules, and it draws them across the FULL row including the gutter column, so
/// the glyph sits inside the fence instead of beside a second border.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::widget_subtree) struct RuledBlock {
    /// Label carried by the top rule.
    pub(in crate::widget_subtree) title: String,
    pub(in crate::widget_subtree) title_style: Style,
    /// Label carried by the bottom rule, right-aligned.
    pub(in crate::widget_subtree) footer: Option<String>,
    pub(in crate::widget_subtree) footer_style: Style,
    pub(in crate::widget_subtree) rule_style: Style,
}

/// Columns of rule drawn before a left-aligned title, and after a
/// right-aligned footer.
const RULE_LEAD: usize = 2;

impl RuledBlock {
    /// The rule that opens the block.
    pub(in crate::widget_subtree) fn top_line(&self, width: u16) -> Line<'static> {
        rule_line(
            width,
            Some(&self.title),
            self.title_style,
            self.rule_style,
            false,
        )
    }

    /// The rule that closes it.
    pub(in crate::widget_subtree) fn bottom_line(&self, width: u16) -> Line<'static> {
        rule_line(
            width,
            self.footer.as_deref(),
            self.footer_style,
            self.rule_style,
            true,
        )
    }
}

/// One rule row: `── label ─────` or `───── label ──`.
///
/// A label that cannot fit its lead, padding and at least one cell of rule is
/// dropped rather than truncated — a half-written title reads as corruption,
/// and at that width the rule alone still says where the block starts.
fn rule_line(
    width: u16,
    label: Option<&str>,
    label_style: Style,
    rule_style: Style,
    label_last: bool,
) -> Line<'static> {
    let width = width as usize;
    if width == 0 {
        return Line::default();
    }
    let labeled = label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(|label| (label, label.width() + 2))
        .filter(|(_, padded)| width >= RULE_LEAD + padded + 1);
    let Some((label, padded)) = labeled else {
        return Line::from(Span::styled("─".repeat(width), rule_style));
    };
    let rest = width - RULE_LEAD - padded;
    let (lead, tail) = if label_last {
        (rest, RULE_LEAD)
    } else {
        (RULE_LEAD, rest)
    };
    Line::from(vec![
        Span::styled("─".repeat(lead), rule_style),
        Span::styled(format!(" {label} "), label_style),
        Span::styled("─".repeat(tail), rule_style),
    ])
}
