use super::*;

// Slash command picker (backed by rebon-customselect)
// ---------------------------------------------------------------------------

/// Render the slash command picker as an overlay above the prompt area.
///
/// Uses `rebon-customselect`'s `NavigationState` via the
/// [`crate::tui::slash_picker`] module for viewport math, wrap-around,
/// and focus tracking.
pub(in crate::tui::runner) fn slash_picker_overlay_height(app: &AppState) -> u16 {
    crate::tui::slash_picker::render_data(&app.slash_picker, &app.slash_commands, &app.input)
        .map(|data| picker_height(data.rows.len()))
        .unwrap_or(0)
}

fn picker_height(visible_count: usize) -> u16 {
    (visible_count as u16).saturating_add(2)
}

pub(in crate::tui::runner) fn render_slash_picker_overlay(
    frame: &mut Frame,
    prompt_area: Rect,
    parent_area: Rect,
    app: &AppState,
) {
    let Some(data) =
        crate::tui::slash_picker::render_data(&app.slash_picker, &app.slash_commands, &app.input)
    else {
        return;
    };

    let Some(picker_area) =
        inline_picker_area(prompt_area, parent_area, picker_height(data.rows.len()))
    else {
        return;
    };
    render_slash_picker_data(frame, picker_area, &data);
}

pub(in crate::tui::runner) fn render_slash_picker_inline_overlay(
    frame: &mut Frame,
    prompt_area: Rect,
    parent_area: Rect,
    app: &AppState,
) {
    let Some(data) =
        crate::tui::slash_picker::render_data(&app.slash_picker, &app.slash_commands, &app.input)
    else {
        return;
    };

    let Some(picker_area) =
        inline_picker_area_below_first(prompt_area, parent_area, picker_height(data.rows.len()))
    else {
        return;
    };
    render_slash_picker_data(frame, picker_area, &data);
}

fn render_slash_picker_data(
    frame: &mut Frame,
    picker_area: Rect,
    data: &crate::tui::slash_picker::PickerRenderData,
) {
    let Some(inner) = render_picker_frame(frame, picker_area, "commands") else {
        return;
    };
    let rows: Vec<PickerLine<'_>> = data
        .rows
        .iter()
        .map(|row| PickerLine {
            name: format!("/{}", row.name),
            description: &row.description,
            tag: row.category.map(|c| c.label()),
            focused: row.is_focused,
        })
        .collect();
    render_picker_rows(frame, inner, &rows);
}

/// One row of a prompt picker, before layout.
struct PickerLine<'a> {
    name: String,
    description: &'a str,
    tag: Option<&'static str>,
    focused: bool,
}

/// Draw the shared picker chrome — a rounded, quiet frame with the picker's
/// name set into the top edge — and return the area rows go in.
fn render_picker_frame(frame: &mut Frame, area: Rect, title: &str) -> Option<Rect> {
    let ds = rebon_design_system::theme::get_active_theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(parse_theme_color(ds.subtle)))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(parse_theme_color(ds.inactive)),
        ));
    let inner = block.inner(area);
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(block, area);
    (inner.width > 0 && inner.height > 0).then_some(inner)
}

/// Lay picker rows out as aligned columns: pointer, name, description, and
/// a right-aligned tag. The focused row is carried by a soft band across the
/// whole row plus the accent pointer, so it reads at a glance without the
/// other rows having to shout.
fn render_picker_rows(frame: &mut Frame, inner: Rect, rows: &[PickerLine<'_>]) {
    let ds = rebon_design_system::theme::get_active_theme();
    let width = inner.width as usize;
    let tag_width = rows
        .iter()
        .filter_map(|row| row.tag.map(|tag| tag.width()))
        .max()
        .map(|w| w + 1)
        .unwrap_or(0);
    // Pointer (2) + name column + gap (2); the name column is as wide as the
    // longest visible name, but never so wide that descriptions vanish.
    let longest_name = rows.iter().map(|row| row.name.width()).max().unwrap_or(0);
    let name_cap = (width * 2 / 5).max(8);
    let name_col = longest_name.min(name_cap);
    let band = parse_theme_color(ds.messageActionsBackground);
    let accent = Style::default()
        .fg(parse_theme_color(ds.suggestion))
        .add_modifier(Modifier::BOLD);
    let secondary = Style::default().fg(parse_theme_color(ds.inactive));
    let tag_style = Style::default().fg(parse_theme_color(ds.subtle));

    for (row_idx, row) in rows.iter().enumerate().take(inner.height as usize) {
        let base = if row.focused {
            Style::default().bg(band)
        } else {
            Style::default()
        };
        let name = truncate_to_ellipsis(&row.name, name_col);
        let name_pad = name_col.saturating_sub(name.width());
        let desc_budget = width
            .saturating_sub(2 + name_col + 2)
            .saturating_sub(tag_width)
            .saturating_sub(1);
        let desc = truncate_to_ellipsis(row.description, desc_budget);
        let used = 2 + name_col + 2 + desc.width();
        let tag = row.tag.unwrap_or("");
        let fill = width.saturating_sub(used).saturating_sub(tag.width() + 1);

        let (pointer, name_style) = if row.focused {
            ("❯ ", accent)
        } else {
            ("  ", Style::default())
        };
        let line = Line::from(vec![
            Span::styled(pointer, accent.patch(base)),
            Span::styled(name, name_style.patch(base)),
            Span::styled(" ".repeat(name_pad + 2), base),
            Span::styled(desc, secondary.patch(base)),
            Span::styled(" ".repeat(fill), base),
            Span::styled(tag.to_string(), tag_style.patch(base)),
            Span::styled(" ", base),
        ]);
        let row_area = Rect::new(inner.x, inner.y + row_idx as u16, inner.width, 1);
        frame.render_widget(Paragraph::new(line).style(base), row_area);
    }
}

/// Render the `@` mention picker overlay above the prompt area.
///
/// Uses the same visual style as the slash picker but with `@ mentions`
/// as the title.
pub(in crate::tui::runner) fn at_mention_overlay_height(app: &AppState) -> u16 {
    crate::tui::at_mention_picker::render_data(&app.at_mention_picker)
        .map(|data| picker_height(data.rows.len()))
        .unwrap_or(0)
}

pub(in crate::tui::runner) fn render_at_mention_overlay(
    frame: &mut Frame,
    prompt_area: Rect,
    parent_area: Rect,
    app: &AppState,
) {
    let Some(data) = crate::tui::at_mention_picker::render_data(&app.at_mention_picker) else {
        return;
    };

    let Some(picker_area) =
        inline_picker_area(prompt_area, parent_area, picker_height(data.rows.len()))
    else {
        return;
    };
    render_at_mention_data(frame, picker_area, &data);
}

pub(in crate::tui::runner) fn render_at_mention_inline_overlay(
    frame: &mut Frame,
    prompt_area: Rect,
    parent_area: Rect,
    app: &AppState,
) {
    let Some(data) = crate::tui::at_mention_picker::render_data(&app.at_mention_picker) else {
        return;
    };

    let Some(picker_area) =
        inline_picker_area_below_first(prompt_area, parent_area, picker_height(data.rows.len()))
    else {
        return;
    };
    render_at_mention_data(frame, picker_area, &data);
}

fn render_at_mention_data(
    frame: &mut Frame,
    picker_area: Rect,
    data: &crate::tui::at_mention_picker::MentionRenderData,
) {
    let Some(inner) = render_picker_frame(frame, picker_area, "@ mentions") else {
        return;
    };
    let rows: Vec<PickerLine<'_>> = data
        .rows
        .iter()
        .map(|row| PickerLine {
            name: if row.is_selectable {
                format!("@{}", row.display)
            } else {
                row.display.clone()
            },
            description: "",
            tag: None,
            focused: row.is_focused,
        })
        .collect();
    render_picker_rows(frame, inner, &rows);
}
