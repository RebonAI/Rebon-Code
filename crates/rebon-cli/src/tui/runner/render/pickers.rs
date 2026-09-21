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
    let width = picker_area.width;

    let ds_picker = rebon_design_system::theme::get_active_theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(parse_theme_color(ds_picker.subtle)))
        .title(Span::styled(
            " / commands ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(picker_area);
    // Clear background.
    frame.render_widget(ratatui::widgets::Clear, picker_area);
    frame.render_widget(block, picker_area);

    for (row_idx, row) in data.rows.iter().enumerate() {
        let prefix = if row.is_focused { "▸ " } else { "  " };
        let name = format!("/{}", row.name);

        // Category badge: [skill] [agent] [cmd]
        let badge = row
            .category
            .map(|c| format!("[{}]", c.label()))
            .unwrap_or_default();
        let badge_len = badge.len();

        let desc_budget = (width as usize)
            .saturating_sub(name.len() + 3 + badge_len + if badge_len > 0 { 1 } else { 0 });
        let desc: String = row.description.chars().take(desc_budget).collect();

        let style = if row.is_focused {
            Style::default()
                .fg(parse_theme_color(ds_picker.suggestion))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let desc_style = Style::default().fg(parse_theme_color(ds_picker.inactive));

        let badge_style = match row.category {
            Some(rebon_types::SlashCommandCategory::Skill) => {
                Style::default().fg(parse_theme_color(ds_picker.success))
            }
            Some(rebon_types::SlashCommandCategory::Agent) => {
                Style::default().fg(parse_theme_color(ds_picker.warning))
            }
            _ => Style::default().fg(parse_theme_color(ds_picker.subtle)),
        };

        let mut spans = vec![
            Span::styled(prefix.to_string(), style),
            Span::styled(name, style),
        ];
        if !badge.is_empty() {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(badge, badge_style));
        }
        spans.push(Span::raw(" "));
        spans.push(Span::styled(desc, desc_style));

        let line = Line::from(spans);

        if row_idx < inner.height as usize {
            let row_area = Rect::new(inner.x, inner.y + row_idx as u16, inner.width, 1);
            frame.render_widget(Paragraph::new(line), row_area);
        }
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
    let ds_mention = rebon_design_system::theme::get_active_theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(parse_theme_color(ds_mention.subtle)))
        .title(Span::styled(
            " @ mentions ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(picker_area);
    frame.render_widget(ratatui::widgets::Clear, picker_area);
    frame.render_widget(block, picker_area);

    for (row_idx, row) in data.rows.iter().enumerate() {
        let prefix = if row.is_focused { "▸ " } else { "  " };
        let display = if row.is_selectable {
            format!("@{}", row.display)
        } else {
            row.display.clone()
        };

        let style = if row.is_focused {
            Style::default()
                .fg(parse_theme_color(ds_mention.suggestion))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let line = Line::from(vec![
            Span::styled(prefix.to_string(), style),
            Span::styled(display, style),
        ]);

        if row_idx < inner.height as usize {
            let row_area = Rect::new(inner.x, inner.y + row_idx as u16, inner.width, 1);
            frame.render_widget(Paragraph::new(line), row_area);
        }
    }
}
