use super::*;

pub(in crate::tui::runner) fn render_header(
    frame: &mut Frame,
    area: Rect,
    status: &StatusBarInfo<'_>,
    coordinator_mode: bool,
    app: &mut AppState,
) {
    if render_sticky_anchor_header(frame, area, app) {
        return;
    }

    // Use the design-system rebon color for Rebon identity.
    let ds_theme = rebon_design_system::theme::get_active_theme();
    let brand_color = parse_theme_color(ds_theme.rebon);
    let version = env!("CARGO_PKG_VERSION");

    // Wire `rebon-tui`'s animated asterisk: its color cycles through the
    // hue spectrum while the session is active, settling to grey when
    // the animation completes.
    let asterisk_frame = animated_asterisk_state(false, status.elapsed_ms);
    let rgb = asterisk_frame.color;
    let asterisk_color = Color::Rgb(rgb.r, rgb.g, rgb.b);

    let inactive_color = parse_theme_color(ds_theme.inactive);

    let mut left_spans = vec![
        Span::styled(
            " *",
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(asterisk_color),
        ),
        Span::styled(
            format!(" Rebon v{}", version),
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(brand_color),
        ),
        Span::styled(
            format!("  {}  {}", status.provider, status.model),
            Style::default().fg(inactive_color),
        ),
    ];
    if app.ui_mode == crate::ui_config::UiMode::Screen {
        left_spans.push(Span::styled(
            format!(
                "  {}",
                super::super::live_agent_view::current_page_name(app)
            ),
            Style::default()
                .fg(brand_color)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if !status.fast_mode_display.is_empty() {
        left_spans.push(Span::styled(
            format!(" {}", status.fast_mode_display),
            Style::default()
                .fg(parse_theme_color(ds_theme.chromeYellow))
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Effort / thinking level indicator (only when explicitly set).
    if !status.effort_display.is_empty() {
        left_spans.push(Span::styled(
            format!("  {}", status.effort_display),
            Style::default().fg(brand_color),
        ));
    }

    // Right-aligned coordinator/ultraplan indicator when active.
    let right_label = if coordinator_mode {
        Some(" Coordinator Mode ")
    } else {
        ultraplan_header_label(app)
    };
    if let Some(label) = right_label {
        let left_width: usize = left_spans.iter().map(|s| s.width()).sum();
        let right_width = label.len();
        let gap = (area.width as usize).saturating_sub(left_width + right_width);
        if gap > 0 {
            left_spans.push(Span::raw(" ".repeat(gap)));
        }
        left_spans.push(Span::styled(
            label.to_string(),
            Style::default()
                .fg(parse_theme_color(ds_theme.chromeYellow))
                .add_modifier(Modifier::BOLD),
        ));
    }

    let header = Paragraph::new(Line::from(left_spans));
    frame.render_widget(header, area);
}

fn render_sticky_anchor_header(frame: &mut Frame, area: Rect, app: &mut AppState) -> bool {
    app.transcript_sticky_anchor_area = None;
    if app.follow_transcript_tail || area.width == 0 || area.height == 0 {
        return false;
    }
    let Some(label) = app.transcript_sticky_anchor_label.as_deref() else {
        return false;
    };

    let ds = rebon_design_system::theme::get_active_theme();
    let available_width = area.width.saturating_sub(4) as usize;
    let label = truncate_to_ellipsis(label, available_width);
    if label.is_empty() {
        return false;
    }

    let text = format!("> {label}");
    let width = text.width().min(area.width as usize) as u16;
    let anchor_area = Rect {
        x: area.x,
        y: area.y,
        width,
        height: 1,
    };
    app.transcript_sticky_anchor_area = Some(anchor_area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default()
                .fg(parse_theme_color(ds.suggestion))
                .add_modifier(Modifier::BOLD),
        ))),
        anchor_area,
    );
    true
}

pub(in crate::tui::runner) fn ultraplan_header_label(app: &AppState) -> Option<&'static str> {
    // Only flag the Planning phase: during Executing the model is
    // actively producing tool calls / output, and the permission-mode
    // pill that ExitPlanMode installs in the footer
    // (`Bypass permissions on` / `Accept edits` / `Default`) already
    // tells the user where the workflow is. Showing both at once read
    // as a duplicate marker.
    app.ultraplan_status
        .as_ref()
        .filter(|status| !matches!(status.phase, UltraplanPhase::Executing))
        .map(|_| " Ultraplan Planning ")
}

pub(in crate::tui::runner) fn render_empty_transcript_background(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let ds = rebon_design_system::theme::get_active_theme();
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let tick = (now_ms / 120) as usize;
    let width = area.width as usize;
    let height = area.height as usize;
    let title_row = if height >= 7 {
        height / 2 - 2
    } else {
        height / 2
    };
    let subtitle_row = (title_row + 2).min(height.saturating_sub(1));
    let pulse_row = (subtitle_row + 2).min(height.saturating_sub(1));

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
    for row in 0..height {
        if row == title_row {
            let title = if width < 28 {
                "Ready"
            } else {
                "No messages yet"
            };
            lines.push(shimmer_text_line(
                title,
                tick,
                Style::default()
                    .fg(parse_theme_color(ds.inactive))
                    .add_modifier(Modifier::BOLD),
                Style::default()
                    .fg(parse_theme_color(ds.rebonShimmer))
                    .add_modifier(Modifier::BOLD),
                Style::default()
                    .fg(parse_theme_color(ds.rebon))
                    .add_modifier(Modifier::BOLD),
            ));
        } else if row == subtitle_row && subtitle_row != title_row {
            lines.push(Line::from(Span::styled(
                "Type a prompt and press Enter",
                Style::default()
                    .fg(parse_theme_color(ds.inactive))
                    .add_modifier(Modifier::DIM),
            )));
        } else if row == pulse_row && pulse_row != title_row && pulse_row != subtitle_row {
            lines.push(empty_state_pulse_line(tick));
        } else {
            lines.push(empty_state_background_line(width, row, tick));
        }
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        area,
    );
}

pub(in crate::tui::runner) fn shimmer_text_line(
    text: &'static str,
    tick: usize,
    base: Style,
    glow: Style,
    hot: Style,
) -> Line<'static> {
    let len = text.chars().count();
    let shimmer_center = (tick % (len + 8)) as isize - 4;
    let spans = text
        .chars()
        .enumerate()
        .map(|(i, ch)| {
            let distance = (i as isize - shimmer_center).abs();
            let style = if distance == 0 {
                hot
            } else if distance <= 2 {
                glow
            } else {
                base
            };
            Span::styled(ch.to_string(), style)
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

pub(in crate::tui::runner) fn empty_state_background_line(
    width: usize,
    _row: usize,
    _tick: usize,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }

    Line::from(Span::raw(" ".repeat(width)))
}

pub(in crate::tui::runner) fn empty_state_pulse_line(tick: usize) -> Line<'static> {
    let ds = rebon_design_system::theme::get_active_theme();
    let active = tick % 8;
    let mut spans = Vec::with_capacity(15);
    for i in 0usize..8 {
        let distance = i.abs_diff(active);
        let style = if distance == 0 {
            Style::default().fg(parse_theme_color(ds.rebon))
        } else if distance <= 1 {
            Style::default().fg(parse_theme_color(ds.rebonShimmer))
        } else {
            Style::default()
                .fg(parse_theme_color(ds.inactive))
                .add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled("•", style));
        if i + 1 < 8 {
            spans.push(Span::raw(" "));
        }
    }
    Line::from(spans)
}
