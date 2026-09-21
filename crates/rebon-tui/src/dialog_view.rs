//! The ratatui painter for [`rebon_dialog::model::ViewSpec`].
//!
//! Dialog models describe what they want shown; this module is the one
//! place that turns such a description into ratatui widgets. Every
//! hosted list dialog goes through [`render_list_view`], so a change to
//! how a list looks is a change in one function rather than in each
//! dialog's own `render`.
//!
//! [`render_panel_view`] is the widest of the painters: a bordered frame
//! with an optional tab strip, one or two panes of styled rows, and a
//! segmented footer. It is what every panel is described with —
//! diagnostics, the plugin manager, the hook browser, settings — rather
//! than each owning a renderer of its own.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use rebon_dialog::host::{DialogStack, StackKey};
use rebon_dialog::model::{
    DialogKey, KeyPress, ListAccent, ListView, OutlineView, PanelPane, PanelRow, PanelSplit,
    PanelTab, PanelView, RowTone, SearchView, TextSpan, ViewSpec,
};

use crate::render::parse_theme_color;

/// Re-exported from [`rebon_width`], where the policy lives. Painting
/// a row is what needs it, so this is where every caller already looks.
pub use rebon_width::truncate_to_width;

/// Translate a terminal key event into the backend-free key a dialog
/// model understands. `None` is a key no dialog can act on; the stack
/// still swallows it.
pub fn translate_key(key: &KeyEvent) -> Option<KeyPress> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    let translated = match key.code {
        KeyCode::Up => Some(DialogKey::Up),
        KeyCode::Down => Some(DialogKey::Down),
        KeyCode::Home => Some(DialogKey::Home),
        KeyCode::End => Some(DialogKey::End),
        KeyCode::Left => Some(DialogKey::Left),
        KeyCode::Right => Some(DialogKey::Right),
        KeyCode::Tab => Some(DialogKey::Tab),
        KeyCode::BackTab => Some(DialogKey::BackTab),
        KeyCode::Backspace => Some(DialogKey::Backspace),
        KeyCode::PageUp => Some(DialogKey::PageUp),
        KeyCode::PageDown => Some(DialogKey::PageDown),
        KeyCode::Enter => Some(DialogKey::Enter),
        KeyCode::Delete => Some(DialogKey::Delete),
        KeyCode::Esc => Some(DialogKey::Escape),
        KeyCode::Char(value) => Some(DialogKey::Char {
            value,
            plain: !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT),
        }),
        _ => None,
    };
    translated.map(|translated| KeyPress {
        key: translated,
        repeat: matches!(key.kind, KeyEventKind::Repeat),
        // Filled in by the stack, which is what knows the viewport.
        viewport_rows: None,
    })
}

/// Offer a terminal key event to the top of `stack`.
pub fn handle_key(stack: &mut DialogStack, key: &KeyEvent) -> StackKey {
    stack.on_key(translate_key(key))
}

/// Paint the top dialog into `area` when it has a declarative view.
///
/// Returns the body rows it painted, so the caller can report them back
/// to the stack for paging, or `None` when there is no dialog open.
pub fn render_top_view(stack: &DialogStack, frame: &mut Frame, area: Rect) -> Option<u16> {
    match stack.top_view()? {
        ViewSpec::List(list) => {
            render_list_view(frame, area, &list);
            Some(area.height.saturating_sub(2))
        }
        ViewSpec::Outline(outline) => Some(render_outline_view(frame, area, &outline)),
        ViewSpec::Search(search) => {
            render_search_view(frame, area, &search);
            Some(area.height.saturating_sub(4))
        }
        ViewSpec::Panel(panel) => Some(render_panel_view(frame, area, &panel)),
    }
}

/// Paint an [`OutlineView`]: a scrolling body of pre-formatted lines
/// with one reversed row and a dim footer pinned to the bottom.
///
/// Returns the body height, which is what a dialog pages by.
pub fn render_outline_view(frame: &mut Frame, area: Rect, view: &OutlineView) -> u16 {
    if area.width == 0 || area.height == 0 {
        return 1;
    }
    let ds = rebon_design_system::theme::get_active_theme();
    let tones = TonePalette::new();
    let border = Style::default().fg(parse_theme_color(ds.subtle));
    let normal = Style::default().fg(parse_theme_color(ds.text));
    let dim = Style::default().fg(parse_theme_color(ds.inactive));
    let selected = normal.add_modifier(Modifier::BOLD | Modifier::REVERSED);

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(
            view.title.clone(),
            normal.add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return 1;
    }

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let body_height = sections[0].height.max(1);

    let lines: Vec<Line> = if view.rows.is_empty() {
        view.empty_text
            .iter()
            .map(|text| Line::styled(text.clone(), dim))
            .collect()
    } else {
        view.rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let style = match (Some(index) == view.selected, row.tone) {
                    (true, _) => selected,
                    (false, tone) => tones.style(tone),
                };
                Line::styled(row.text.clone(), style)
            })
            .collect()
    };

    // An explicit offset scrolls where the dialog says; without one the
    // body scrolls the least it can to keep the selection on screen.
    let offset = view.scroll.unwrap_or_else(|| {
        view.selected
            .unwrap_or(0)
            .saturating_sub(body_height.saturating_sub(1) as usize) as u16
    });
    frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), sections[0]);
    frame.render_widget(
        Paragraph::new(Line::styled(view.footer.clone(), dim)),
        sections[1],
    );
    body_height
}

/// The eight [`RowTone`] roles resolved against the active theme.
///
/// Built once per paint rather than once per span: a panel draws a few
/// hundred runs a frame and every one of them would otherwise take the
/// theme lock.
struct TonePalette {
    normal: Style,
    dim: Style,
    strong: Style,
    brand: Style,
    focus: Style,
    success: Style,
    warning: Style,
    error: Style,
}

impl TonePalette {
    fn new() -> Self {
        let ds = rebon_design_system::theme::get_active_theme();
        let fg = |color| Style::default().fg(parse_theme_color(color));
        Self {
            normal: fg(ds.text),
            dim: fg(ds.inactive),
            strong: fg(ds.text).add_modifier(Modifier::BOLD),
            brand: fg(ds.rebon).add_modifier(Modifier::BOLD),
            focus: fg(ds.suggestion).add_modifier(Modifier::BOLD),
            success: fg(ds.success),
            warning: fg(ds.warning),
            error: fg(ds.error),
        }
    }

    fn style(&self, tone: RowTone) -> Style {
        match tone {
            RowTone::Normal => self.normal,
            RowTone::Dim => self.dim,
            RowTone::Strong => self.strong,
            RowTone::Brand => self.brand,
            RowTone::Focus => self.focus,
            RowTone::Success => self.success,
            RowTone::Warning => self.warning,
            RowTone::Error => self.error,
        }
    }
}

/// Paint a [`PanelView`]: a bordered frame, an optional tab strip, one
/// or two panes of styled rows, and a segmented footer.
///
/// Returns the body height, which is what a panel pages by.
pub fn render_panel_view(frame: &mut Frame, area: Rect, view: &PanelView) -> u16 {
    if area.width == 0 || area.height == 0 {
        return 1;
    }
    let tones = TonePalette::new();
    let border = Style::default().fg(parse_theme_color(
        rebon_design_system::theme::get_active_theme().subtle,
    ));

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(view.title.clone(), tones.strong));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return 1;
    }

    // A strip and a footer each cost a row only when there is one, so a
    // panel with neither gets the whole frame for its panes.
    let tab_rows = u16::from(!view.tabs.is_empty());
    let footer_rows = u16::from(!view.footer.is_empty());
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(tab_rows),
            Constraint::Min(1),
            Constraint::Length(footer_rows),
        ])
        .split(inner);

    if tab_rows == 1 {
        frame.render_widget(Paragraph::new(tab_strip(&view.tabs, &tones)), sections[0]);
    }

    let body = sections[1];
    match &view.side {
        None => render_panel_pane(frame, body, &view.body, &tones),
        Some(side) => {
            let (first, second) = split_panel_body(body, view.split);
            render_panel_pane(frame, first, &view.body, &tones);
            render_panel_pane(frame, second, side, &tones);
        }
    }

    if footer_rows == 1 {
        frame.render_widget(Paragraph::new(span_line(&view.footer, &tones)), sections[2]);
    }
    body.height.max(1)
}

/// The tab strip: names separated by two dim spaces, the active one in
/// the focus role.
fn tab_strip(tabs: &[PanelTab], tones: &TonePalette) -> Line<'static> {
    let mut spans = Vec::with_capacity(tabs.len() * 2);
    for (index, tab) in tabs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", tones.dim));
        }
        spans.push(Span::styled(
            tab.label.clone(),
            if tab.active { tones.focus } else { tones.dim },
        ));
    }
    Line::from(spans)
}

/// Where the two panes of a split panel go.
///
/// Side by side above the panel's own stacking width, stacked below it.
/// Every number is the panel's; nothing is re-derived here.
fn split_panel_body(area: Rect, split: PanelSplit) -> (Rect, Rect) {
    if split.stack_below > 0 && area.width < split.stack_below {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(split.stacked_second_rows),
            ])
            .split(area);
        return (rows[0], rows[1]);
    }
    let percent = if split.first_percent == 0 {
        50
    } else {
        split.first_percent.min(100)
    };
    let first = ((u32::from(area.width) * u32::from(percent)) / 100) as u16;
    let first = first.max(split.first_min_columns).min(area.width);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(first),
            Constraint::Length(split.gap),
            Constraint::Min(0),
        ])
        .split(area);
    (cols[0], cols[2])
}

/// Paint one pane: its border when it has a title, then its rows from
/// `scroll` down, or its empty text when it has no rows.
fn render_panel_pane(frame: &mut Frame, area: Rect, pane: &PanelPane, tones: &TonePalette) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let inner = match &pane.title {
        Some(title) => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(tones.dim)
                .title(Span::styled(title.clone(), tones.dim));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            inner
        }
        None => area,
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if pane.rows.is_empty() {
        if let Some(text) = &pane.empty_text {
            frame.render_widget(Paragraph::new(Line::styled(text.clone(), tones.dim)), inner);
        }
        return;
    }
    frame.render_widget(panel_paragraph(pane, tones).scroll((pane.scroll, 0)), inner);
}

/// Measure the same wrapped rows the painter uses, excluding any pane border.
pub fn panel_content_height(pane: &PanelPane, content_width: u16) -> usize {
    panel_paragraph(pane, &TonePalette::new()).line_count(content_width)
}

fn panel_paragraph(pane: &PanelPane, tones: &TonePalette) -> Paragraph<'static> {
    let lines: Vec<Line> = pane
        .rows
        .iter()
        .map(|row| panel_row_line(row, tones))
        .collect();
    let paragraph = Paragraph::new(lines);
    if pane.wrap {
        paragraph.wrap(Wrap { trim: false })
    } else {
        paragraph
    }
}

/// One row's runs, each in its own role, the whole row reversed when it
/// is the highlighted one.
fn panel_row_line(row: &PanelRow, tones: &TonePalette) -> Line<'static> {
    Line::from(
        row.spans
            .iter()
            .map(|span| {
                let style = tones.style(span.tone);
                Span::styled(
                    span.text.clone(),
                    if row.highlighted {
                        style.add_modifier(Modifier::REVERSED | Modifier::BOLD)
                    } else {
                        style
                    },
                )
            })
            .collect::<Vec<_>>(),
    )
}

/// A line of styled runs, for a footer or any other single row.
fn span_line(spans: &[TextSpan], tones: &TonePalette) -> Line<'static> {
    Line::from(
        spans
            .iter()
            .map(|span| Span::styled(span.text.clone(), tones.style(span.tone)))
            .collect::<Vec<_>>(),
    )
}

/// Paint a [`ListView`] into `area`, filling it opaque first so an
/// inline host does not bleed the prompt underneath.
///
/// The row layout is `marker · prefix · label · detail · badge`, each
/// part carrying its own style role: the marker and detail dim when the
/// row is not selected, the prefix and label fall back to normal text,
/// and the badge is always the success colour.
pub fn render_list_view(frame: &mut Frame, area: Rect, view: &ListView) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let ds = rebon_design_system::theme::get_active_theme();
    let border = Style::default().fg(parse_theme_color(ds.subtle));
    let title_style = Style::default()
        .fg(parse_theme_color(ds.text))
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(parse_theme_color(ds.inactive));
    let normal = Style::default().fg(parse_theme_color(ds.text));
    let accent_color = match view.accent {
        ListAccent::Brand => ds.rebon,
        ListAccent::Success => ds.success,
    };
    let accent = Style::default()
        .fg(parse_theme_color(accent_color))
        .add_modifier(Modifier::BOLD);
    let ok = Style::default().fg(parse_theme_color(ds.success));
    let ok_bold = ok.add_modifier(Modifier::BOLD);
    let detail_selected = if view.detail_follows_selection {
        accent
    } else {
        dim
    };

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(view.title.clone(), title_style));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::with_capacity(view.rows.len() + view.header.len() + 2);
    for header in &view.header {
        lines.push(Line::from(Span::styled(header.clone(), dim)));
    }

    for (index, row) in visible_range(view, inner.height) {
        let is_selected = index == view.selected;
        let marker = if is_selected { "❯ " } else { "  " };
        let label_style = if is_selected { accent } else { normal };
        let mut spans = vec![Span::styled(marker, if is_selected { accent } else { dim })];
        if let Some(checked) = row.checked {
            spans.push(Span::styled(
                if checked { "[x] " } else { "[ ] " },
                if checked { ok } else { dim },
            ));
        }
        if let Some(prefix) = &row.prefix {
            spans.push(Span::styled(prefix.clone(), label_style));
        }
        spans.push(Span::styled(row.label.clone(), label_style));
        if let Some(detail) = &row.detail {
            spans.push(Span::styled(
                detail.clone(),
                if is_selected { detail_selected } else { dim },
            ));
        }
        if let Some(badge) = &row.badge {
            spans.push(Span::styled(
                badge.text.clone(),
                if badge.bold { ok_bold } else { ok },
            ));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    for footer in &view.footer {
        lines.push(Line::from(Span::styled(footer.clone(), dim)));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The rows to paint, paired with their index in `view.rows`.
///
/// An uncapped list paints every row and lets the paragraph clip. A
/// capped one scrolls just enough to keep the selection on screen,
/// sizing its window to whatever the border leaves after the header,
/// the blank separator, and the footer.
fn visible_range(
    view: &ListView,
    inner_height: u16,
) -> impl Iterator<Item = (usize, &rebon_dialog::model::ListRow)> {
    let (start, end) = match view.max_visible {
        None => (0, view.rows.len()),
        Some(cap) => {
            let reserved = view.chrome_height().saturating_sub(2);
            let window = (inner_height as usize)
                .saturating_sub(reserved)
                .clamp(1, cap)
                .min(view.rows.len());
            let start = if view.selected >= window {
                view.selected + 1 - window
            } else {
                0
            };
            (start, (start + window).min(view.rows.len()))
        }
    };
    view.rows[start..end]
        .iter()
        .enumerate()
        .map(move |(offset, row)| (start + offset, row))
}

/// Paint a [`SearchView`]: a filter line, a bordered result list, a
/// bordered preview of the focused result, and a segmented footer.
///
/// The preview sits to the right when the panel says the frame is wide
/// enough, and below it otherwise. Every width and count comes from the
/// panel; nothing is re-derived here, so the rows a panel windowed are
/// the rows that get drawn.
pub fn render_search_view(frame: &mut Frame, area: Rect, view: &SearchView) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let ds = rebon_design_system::theme::get_active_theme();
    let border = Style::default().fg(parse_theme_color(ds.subtle));
    let title_style = Style::default()
        .fg(parse_theme_color(ds.text))
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(parse_theme_color(ds.inactive));
    let selected = Style::default()
        .fg(parse_theme_color(ds.suggestion))
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(parse_theme_color(ds.text));

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(view.title.clone(), title_style));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(view.layout.body_min_rows),
            Constraint::Length(1),
        ])
        .split(inner);

    let query_line = if view.query.is_empty() {
        Line::from(vec![
            Span::styled(" Filter: ", dim),
            Span::styled(view.placeholder.clone(), dim),
        ])
    } else {
        Line::from(vec![
            Span::styled(" Filter: ", dim),
            Span::styled(view.query.clone(), normal),
        ])
    };
    frame.render_widget(Paragraph::new(query_line), sections[0]);

    let (list_area, preview_area) = if view.layout.preview_on_right {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(view.layout.list_width.min(sections[1].width as usize) as u16),
                Constraint::Length(1),
                Constraint::Min(20),
            ])
            .split(sections[1]);
        (cols[0], cols[2])
    } else if let Some(list_rows) = view.layout.stacked_list_rows {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(list_rows as u16 + 2),
                Constraint::Length(1),
                Constraint::Min(4),
            ])
            .split(sections[1]);
        (rows[0], rows[2])
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(4),
                Constraint::Length((view.layout.preview_rows + 2) as u16),
            ])
            .split(sections[1]);
        (rows[0], rows[1])
    };

    render_search_list(frame, list_area, view, selected, normal, dim);
    render_search_preview(frame, preview_area, view, dim);

    let footer = Line::from(
        view.footer
            .iter()
            .map(|segment| {
                Span::styled(
                    segment.text.clone(),
                    if segment.emphasis { selected } else { dim },
                )
            })
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(footer), sections[2]);
}

fn render_search_list(
    frame: &mut Frame,
    area: Rect,
    view: &SearchView,
    selected: Style,
    normal: Style,
    dim: Style,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let block = Block::default().borders(Borders::ALL).border_style(dim);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if view.rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(view.empty_message.clone(), dim))),
            inner,
        );
        return;
    }

    for (index, row) in view.rows.iter().enumerate() {
        if index as u16 >= inner.height {
            break;
        }
        let style = if row.focused { selected } else { normal };
        let prefix = if row.focused { ">" } else { " " };
        let line = Line::from(vec![
            Span::styled(format!("{prefix} "), style),
            Span::styled(
                truncate_to_width(&row.text, inner.width.saturating_sub(2) as usize),
                style,
            ),
        ]);
        let row_area = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
        frame.render_widget(Paragraph::new(line), row_area);
    }
}

fn render_search_preview(frame: &mut Frame, area: Rect, view: &SearchView, dim: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let block = Block::default().borders(Borders::ALL).border_style(dim);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if view.preview.is_empty() && view.preview_header.is_none() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                view.preview_empty_message.clone(),
                dim,
            ))),
            inner,
        );
        return;
    }

    // Through the palette, so a role added later is painted rather than
    // silently collapsed onto one of the two this pane started with.
    let body = TonePalette::new().style(view.preview_tone);
    let mut y = inner.y;
    if let Some(header) = &view.preview_header {
        if y < inner.bottom() {
            let row = Rect::new(inner.x, y, inner.width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate_to_width(header, inner.width as usize),
                    dim,
                ))),
                row,
            );
            y += 1;
        }
    }
    for line in &view.preview {
        if y >= inner.bottom() {
            break;
        }
        let row = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_to_width(line, inner.width as usize),
                body,
            ))),
            row,
        );
        y += 1;
    }
    if view.preview_more > 0 && y < inner.bottom() {
        let row = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("... +{} more lines", view.preview_more),
                dim,
            ))),
            row,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use rebon_dialog::model::{
        ListBadge, ListRow, PanelPane, PanelRow, PanelSplit, PanelTab, PanelView, TextSpan,
    };

    fn row(label: &str) -> ListRow {
        ListRow {
            label: label.into(),
            ..ListRow::default()
        }
    }

    fn draw(view: &ListView, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_list_view(frame, frame.area(), view))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn draw_panel(view: &PanelView, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_panel_view(frame, frame.area(), view);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn panel_height_counts_word_wrapping_wide_characters_and_blank_rows() {
        let mut pane = PanelPane {
            rows: vec![
                PanelRow::spans(vec![TextSpan::strong("abcd "), TextSpan::normal("efgh")]),
                PanelRow::blank(),
                PanelRow::one(TextSpan::dim("界界界界")),
            ],
            wrap: true,
            ..Default::default()
        };
        assert_eq!(panel_content_height(&pane, 6), 5);
        assert_eq!(panel_content_height(&pane, 20), 3);
        pane.wrap = false;
        assert_eq!(panel_content_height(&pane, 6), 3);
    }

    #[test]
    fn panel_height_handles_empty_content_and_zero_columns() {
        assert_eq!(panel_content_height(&PanelPane::default(), 20), 0);
        let pane = PanelPane::rows(vec![PanelRow::one(TextSpan::normal("text"))]);
        assert_eq!(panel_content_height(&pane, 0), 0);
    }

    #[test]
    fn measured_panel_height_reaches_the_last_painted_line() {
        for width in [4, 8, 30] {
            let body = PanelPane {
                rows: vec![
                    PanelRow::one(TextSpan::normal(
                        "words with spaces and a-long-directory-name",
                    )),
                    PanelRow::one(TextSpan::normal("界界界界 e\u{301}e\u{301}")),
                    PanelRow::one(TextSpan::strong("END")),
                ],
                wrap: true,
                ..Default::default()
            };
            let height = panel_content_height(&body, width);
            let view = PanelView {
                body: PanelPane {
                    scroll: (height - 1) as u16,
                    ..body
                },
                ..Default::default()
            };
            let lines = draw_panel(&view, width + 2, 4);
            assert!(lines[1].contains("END"), "width={width}: {lines:?}");
        }
    }

    #[test]
    fn a_panel_paints_its_title_rows_and_footer() {
        let view = PanelView {
            title: " Doctor ".into(),
            body: PanelPane::rows(vec![
                PanelRow::spans(vec![TextSpan::strong("version: "), TextSpan::normal("1.0")]),
                PanelRow::blank(),
                PanelRow::one(TextSpan::dim("       Fix: read the docs")),
            ]),
            footer: vec![TextSpan::dim("Esc close")],
            ..PanelView::default()
        };
        let lines = draw_panel(&view, 44, 8);

        assert!(lines[0].contains("Doctor"), "{lines:?}");
        assert!(lines[1].contains("version: 1.0"), "{lines:?}");
        assert!(
            lines.iter().any(|line| line.contains("Fix: read the docs")),
            "{lines:?}"
        );
        // The footer is pinned to the last body row, inside the border.
        assert!(lines[6].contains("Esc close"), "{lines:?}");
    }

    #[test]
    fn a_panel_scrolls_its_body_and_keeps_the_footer() {
        let view = PanelView {
            title: " Long ".into(),
            body: PanelPane {
                rows: (0..30)
                    .map(|index| PanelRow::one(TextSpan::normal(format!("row{index}"))))
                    .collect(),
                scroll: 12,
                ..PanelPane::default()
            },
            footer: vec![TextSpan::dim("Esc close")],
            ..PanelView::default()
        };
        let lines = draw_panel(&view, 30, 6);

        assert!(lines[1].contains("row12"), "{lines:?}");
        assert!(
            !lines.iter().any(|line| line.contains("row0 ")),
            "{lines:?}"
        );
        assert!(lines[4].contains("Esc close"), "{lines:?}");
    }

    #[test]
    fn a_two_pane_panel_puts_the_side_pane_beside_the_body_and_stacks_when_narrow() {
        let view = PanelView {
            title: " Hooks ".into(),
            tabs: vec![
                PanelTab {
                    label: "status".into(),
                    active: true,
                },
                PanelTab {
                    label: "config".into(),
                    active: false,
                },
            ],
            body: PanelPane::rows(vec![PanelRow::one(TextSpan::normal("LEFT"))]),
            side: Some(PanelPane::rows(vec![PanelRow::one(TextSpan::normal(
                "RIGHT",
            ))])),
            split: PanelSplit {
                first_percent: 40,
                stack_below: 40,
                stacked_second_rows: 3,
                ..PanelSplit::default()
            },
            footer: vec![TextSpan::dim("Esc")],
            ..PanelView::default()
        };

        let wide = draw_panel(&view, 60, 10);
        // The tab strip is the first row inside the border, active first.
        assert!(wide[1].contains("status  config"), "{wide:?}");
        // Both panes share the row under it.
        let row = &wide[2];
        assert!(row.contains("LEFT"), "{wide:?}");
        assert!(row.contains("RIGHT"), "{wide:?}");
        assert!(
            row.find("LEFT") < row.find("RIGHT"),
            "the body pane comes first: {wide:?}"
        );

        // Below the panel's own stacking width they sit on separate rows.
        let narrow = draw_panel(&view, 30, 12);
        let left = narrow.iter().position(|line| line.contains("LEFT"));
        let right = narrow.iter().position(|line| line.contains("RIGHT"));
        assert!(left.is_some() && right.is_some(), "{narrow:?}");
        assert!(left < right, "{narrow:?}");
    }

    #[test]
    fn a_pane_with_a_title_draws_its_own_border_and_an_empty_one_its_placeholder() {
        let view = PanelView {
            title: " Settings ".into(),
            body: PanelPane {
                title: Some(" Config options ".into()),
                empty_text: Some("(no config options available)".into()),
                ..PanelPane::default()
            },
            ..PanelView::default()
        };
        let lines = draw_panel(&view, 50, 8);

        assert!(lines[1].contains("Config options"), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("(no config options available)")),
            "{lines:?}"
        );
    }

    #[test]
    fn rows_render_marker_prefix_label_detail_and_badge_in_order() {
        let view = ListView {
            title: " Pick ".into(),
            rows: vec![ListRow {
                checked: None,
                prefix: Some("1. ".into()),
                label: "Low  ".into(),
                detail: Some("fast".into()),
                badge: Some(ListBadge {
                    text: "  (current)".into(),
                    bold: true,
                }),
            }],
            footer: vec!["Esc cancel".into()],
            ..ListView::default()
        };
        let lines = draw(&view, 40, 6);
        assert!(lines[0].contains("Pick"), "{lines:?}");
        assert!(lines[1].contains("❯ 1. Low  fast  (current)"), "{lines:?}");
        assert!(lines[3].contains("Esc cancel"), "{lines:?}");
    }

    #[test]
    fn a_checked_row_draws_a_checkbox_and_an_unchecked_one_an_empty_box() {
        let view = ListView {
            rows: vec![
                ListRow {
                    checked: Some(true),
                    label: "on".into(),
                    ..ListRow::default()
                },
                ListRow {
                    checked: Some(false),
                    label: "off".into(),
                    ..ListRow::default()
                },
            ],
            footer: vec!["Esc".into()],
            ..ListView::default()
        };
        let lines = draw(&view, 40, 8);
        assert!(lines[1].contains("[x] on"), "{lines:?}");
        assert!(lines[2].contains("[ ] off"), "{lines:?}");
        // A row that opts out draws no box at all.
        let plain = ListView {
            rows: vec![row("bare")],
            footer: vec!["Esc".into()],
            ..ListView::default()
        };
        assert!(!draw(&plain, 40, 6)[1].contains('['));
    }

    #[test]
    fn uncapped_lists_paint_every_row() {
        let view = ListView {
            rows: (0..4).map(|i| row(&format!("row{i}"))).collect(),
            footer: vec!["help".into()],
            ..ListView::default()
        };
        let lines = draw(&view, 30, 10);
        for i in 0..4 {
            assert!(
                lines.iter().any(|line| line.contains(&format!("row{i}"))),
                "row{i} missing from {lines:?}"
            );
        }
    }

    #[test]
    fn capped_lists_scroll_to_keep_the_selection_visible() {
        let view = ListView {
            header: vec!["Active: p".into(), String::new()],
            rows: (0..25).map(|i| row(&format!("row{i}"))).collect(),
            selected: 20,
            footer: vec!["help".into()],
            max_visible: Some(10),
            ..ListView::default()
        };
        let lines = draw(&view, 40, 16);
        assert!(
            lines.iter().any(|line| line.contains("row20")),
            "selection scrolled off: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("row0 ") || line == "row0"),
            "top row should have scrolled away: {lines:?}"
        );
    }

    #[test]
    fn tiny_and_roomy_viewports_both_render_without_panicking() {
        let view = ListView {
            title: " Pick ".into(),
            header: vec!["Active: p".into(), String::new()],
            rows: (0..25).map(|i| row(&format!("row{i}"))).collect(),
            selected: 20,
            footer: vec!["help".into()],
            max_visible: Some(10),
            accent: ListAccent::Success,
            detail_follows_selection: true,
        };
        for (width, height) in [(0u16, 0u16), (4, 2), (20, 3), (64, 14), (100, 40)] {
            let mut terminal =
                Terminal::new(TestBackend::new(width.max(1), height.max(1))).unwrap();
            terminal
                .draw(|frame| {
                    render_list_view(
                        frame,
                        Rect {
                            x: 0,
                            y: 0,
                            width,
                            height,
                        },
                        &view,
                    )
                })
                .unwrap();
        }
    }
}
