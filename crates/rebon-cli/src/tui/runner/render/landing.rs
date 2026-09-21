use super::*;

pub(in crate::tui::runner) fn centered_rect(
    area: Rect,
    width_percent: u16,
    height_percent: u16,
) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

pub(in crate::tui::runner) fn prompt_height_for_width(
    app: &AppState,
    prompt_width: u16,
    terminal_height: u16,
) -> u16 {
    let prompt_inner_width = prompt_width.saturating_sub(4) as usize;
    let input_has_mode_prefix = app.input.starts_with('!');
    let display_for_height = if input_has_mode_prefix {
        &app.input[1..]
    } else {
        &app.input
    };
    let cursor_byte = if input_has_mode_prefix {
        app.cursor_offset.saturating_sub(1)
    } else {
        app.cursor_offset
    };
    let visual_lines =
        super::super::visual_lines_with_cursor(display_for_height, cursor_byte, prompt_inner_width);
    if terminal_height == 0 {
        return 0;
    }
    let desired_height = (visual_lines as u16).saturating_add(2).max(3);
    let minimum_height = terminal_height.min(3);
    let max_height = (terminal_height / 3)
        .max(minimum_height)
        .min(terminal_height);
    desired_height.min(max_height)
}

pub(in crate::tui::runner) fn should_render_landing_prompt(
    app: &AppState,
    is_loading: bool,
    _task_list_height: u16,
    queue_banner_height: u16,
) -> bool {
    !is_loading
        && app.rebon_tui.transcript.is_empty()
        && app.rebon_tui.overlay.is_empty()
        && app.pending_permission_view.is_none()
        && !app
            .resume_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_prompt_replacement())
        && queue_banner_height == 0
        && app.background_tasks_dialog.is_none()
        && app.goal_confirm_dialog.is_none()
        && app.teams_dialog.is_none()
}

pub(in crate::tui::runner) fn landing_prompt_width(width: u16) -> u16 {
    if width <= 4 {
        width
    } else {
        width.saturating_sub(4).min(88)
    }
}

pub(in crate::tui::runner) fn render_landing_logo(frame: &mut Frame, area: Rect, elapsed_ms: u64) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let ds = rebon_design_system::theme::get_active_theme();
    let asterisk_frame = animated_asterisk_state(false, elapsed_ms);
    let rgb = asterisk_frame.color;
    let asterisk_color = Color::Rgb(rgb.r, rgb.g, rgb.b);
    let brand_color = parse_theme_color(ds.rebon);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "*",
                Style::default()
                    .fg(asterisk_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " Rebon",
                Style::default()
                    .fg(brand_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        area,
    );
}

pub(in crate::tui::runner) fn render_landing_prompt_surface(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    runtime_state: &rebon_tui::promptinput::PromptInputRuntimeState,
    theme: &RenderTheme,
    status: &StatusBarInfo<'_>,
    is_loading: bool,
    elapsed_ms: u64,
    cursor_hint: &mut Option<(u16, u16)>,
) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }

    let logo_height = if area.height > 1 { 1 } else { 0 };
    if logo_height > 0 {
        render_landing_logo(
            frame,
            Rect::new(area.x, area.y, area.width, logo_height),
            elapsed_ms,
        );
    }

    let content_area = Rect::new(
        area.x,
        area.y.saturating_add(logo_height),
        area.width,
        area.height.saturating_sub(logo_height),
    );
    if content_area.width == 0 || content_area.height == 0 {
        return None;
    }

    let prompt_width = landing_prompt_width(content_area.width);
    if prompt_width == 0 {
        return None;
    }

    let prompt_height = prompt_height_for_width(app, prompt_width, content_area.height)
        .max(1)
        .min(content_area.height);
    let title_height = if content_area.height > prompt_height {
        1
    } else {
        0
    };
    let meta_height = if content_area.height > prompt_height.saturating_add(title_height) {
        1
    } else {
        0
    };
    let used_without_gap = prompt_height
        .saturating_add(title_height)
        .saturating_add(meta_height);
    let gap_height = if title_height > 0 && content_area.height > used_without_gap {
        1
    } else {
        0
    };
    let group_height = used_without_gap
        .saturating_add(gap_height)
        .min(content_area.height);
    let bottom = content_area.y.saturating_add(content_area.height);
    let x = content_area.x + content_area.width.saturating_sub(prompt_width) / 2;
    let mut y = content_area.y + content_area.height.saturating_sub(group_height) / 2;

    if title_height > 0 && y < bottom {
        let title_area = Rect::new(x, y, prompt_width, 1);
        render_landing_title(frame, title_area);
        y = y.saturating_add(1);
    }
    y = y.saturating_add(gap_height);

    let prompt_area = Rect::new(
        x,
        y,
        prompt_width,
        prompt_height.min(bottom.saturating_sub(y)),
    );
    if prompt_area.height > 0 {
        render_prompt_surface(
            frame,
            prompt_area,
            app,
            runtime_state,
            theme,
            is_loading,
            elapsed_ms,
            None,
            cursor_hint,
        );
    }
    y = y.saturating_add(prompt_area.height);

    if meta_height > 0 && y < bottom {
        let meta_area = Rect::new(x, y, prompt_width, 1);
        render_landing_meta_line(frame, meta_area, app, status);
    }

    Some(prompt_area)
}

pub(in crate::tui::runner) fn render_landing_title(frame: &mut Frame, area: Rect) {
    let ds = rebon_design_system::theme::get_active_theme();
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "What's the plan for today?",
            Style::default()
                .fg(parse_theme_color(ds.text))
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        area,
    );
}

pub(in crate::tui::runner) fn render_landing_meta_line(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    status: &StatusBarInfo<'_>,
) {
    let ds = rebon_design_system::theme::get_active_theme();
    let label_style = Style::default()
        .fg(parse_theme_color(ds.inactive))
        .add_modifier(Modifier::DIM);
    let separator_style = Style::default().fg(parse_theme_color(ds.subtle));
    let provider_style = Style::default().fg(parse_theme_color(ds.suggestion));
    let model_style = Style::default().fg(parse_theme_color(ds.text));
    let effort_style = Style::default()
        .fg(parse_theme_color(ds.planMode))
        .add_modifier(Modifier::BOLD);
    let fast_mode_style = Style::default()
        .fg(parse_theme_color(ds.chromeYellow))
        .add_modifier(Modifier::BOLD);
    let effort_is_explicit = app.effort_level.is_some();
    let effort_value = app
        .effort_level
        .map(|level| level.as_str().to_string())
        .unwrap_or_else(|| String::from("auto"));
    let effort_value_style = if effort_is_explicit {
        effort_style
    } else {
        label_style
    };

    let mut spans = vec![
        Span::styled("provider: ", label_style),
        Span::styled(status.provider.to_string(), provider_style),
        Span::styled("  ·  ", separator_style),
        Span::styled("model: ", label_style),
        Span::styled(status.model.to_string(), model_style),
    ];
    if !status.fast_mode_display.is_empty() {
        spans.push(Span::styled(
            format!(" {}", status.fast_mode_display),
            fast_mode_style,
        ));
    }
    spans.extend([
        Span::styled("  ·  ", separator_style),
        Span::styled("effort: ", label_style),
        Span::styled(effort_value, effort_value_style),
    ]);
    if app.coordinator_mode {
        spans.extend([
            Span::styled("  ·  ", separator_style),
            Span::styled("mode: ", label_style),
            Span::styled(
                "Coordinator Mode",
                Style::default()
                    .fg(parse_theme_color(ds.chromeYellow))
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}
