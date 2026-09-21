use super::*;

pub(in crate::tui::runner) fn render_footer(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    status: &StatusBarInfo<'_>,
    _is_leader_idle: bool,
    zones: &LayoutZones,
) {
    let mut priority_spans: Vec<Span<'static>> = Vec::new();

    // Permission mode indicator — only when non-default.
    if !rebon_permissions::is_default_mode(Some(app.permission_mode)) {
        let sym = permission_mode_symbol(app.permission_mode);
        let label = match app.permission_mode {
            rebon_permissions::PermissionMode::Plan => "Plan mode",
            rebon_permissions::PermissionMode::BypassPermissions => "Bypass permissions on",
            rebon_permissions::PermissionMode::AcceptEdits => "Accept edits",
            rebon_permissions::PermissionMode::DontAsk => "Don't ask",
            rebon_permissions::PermissionMode::Auto => "Auto",
            _ => rebon_permissions::permission_mode_short_title(app.permission_mode),
        };
        let ds_mode = rebon_design_system::theme::get_active_theme();
        let mode_color = match app.permission_mode {
            rebon_permissions::PermissionMode::Plan => parse_theme_color(ds_mode.planMode),
            _ => parse_theme_color(ds_mode.chromeYellow),
        };
        let padded_sym = rebon_width::pad_wide_symbol(sym);
        priority_spans.push(Span::styled(
            format!(" {padded_sym} {label} (shift+tab to cycle) "),
            Style::default().fg(mode_color),
        ));
    }

    let ds_footer = rebon_design_system::theme::get_active_theme();
    let snapshots = app.task_snapshots();
    use rebon_tui::promptinput::footer_navigation::FooterItem;

    if let Some(label) =
        rebon_plugin_tasks::ui::tasks_view::background_tasks_footer_label(&snapshots)
    {
        let mut tasks_style = Style::default().fg(parse_theme_color(ds_footer.chromeYellow));
        if tasks_footer_is_selected(app) {
            tasks_style = tasks_style
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD);
        }
        priority_spans.push(Span::styled(format!(" {label} "), tasks_style));
    }

    let mut left_spans = priority_spans.clone();

    // Pill / scroll indicator driven by zones + unseen divider.
    if zones.show_pill {
        let display = new_messages_pill::project_pill(zones.pill_count, false);
        left_spans.push(Span::styled(
            format!(" {} {} ", display.arrow, display.label),
            Style::default().fg(parse_theme_color(ds_footer.professionalBlue)),
        ));
    }

    let queue_len = visible_queue_len(app);
    if queue_len > 0 {
        left_spans.push(Span::styled(
            format!(" queue: {queue_len} "),
            Style::default().fg(parse_theme_color(ds_footer.chromeYellow)),
        ));
    }

    // Ultraplan phase is rendered as a top-right header chip
    // (`ultraplan_header_label`); duplicating it in the footer just
    // wastes columns and crowded out the rest of the indicators.

    let teams_selected = app.footer_selection == Some(FooterItem::Teams);

    // Show team status when teammates are present.
    if app.teams_dialog.is_none() {
        let team_status_input =
            rebon_plugin_tasks::ui::teams_view::build_team_status_input(&snapshots);
        let team_display = rebon_plugin_tasks::ui::teams::team_status::render_team_status(
            &team_status_input,
            false,
            false,
        );
        if let Some(ts) = team_display {
            let mut team_style = Style::default().fg(parse_theme_color(ds_footer.planMode));
            if teams_selected {
                team_style = team_style
                    .add_modifier(Modifier::REVERSED)
                    .add_modifier(Modifier::BOLD);
            }
            left_spans.push(Span::styled(format!(" {} ", ts.text), team_style));
        }
    }

    // Wire the usage projection: cost display — only when there is real cost.
    // format_cost_default is ready; skip rendering when cost is zero.
    let _cost_formatter = format_cost_default; // tracked for when API cost is plumbed

    if let Some(label) = rebon_plugin_tasks::ui::tasks_view::workflows_footer_label(&snapshots) {
        let mut workflows_style = Style::default().fg(parse_theme_color(ds_footer.planMode));
        if app.footer_selection == Some(FooterItem::Workflows) {
            workflows_style = workflows_style
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD);
        }
        left_spans.push(Span::styled(format!(" {label} "), workflows_style));
    }

    if app.rc_status.is_visible() {
        let mut style = Style::default().fg(parse_theme_color(ds_footer.professionalBlue));
        if app.footer_selection == Some(FooterItem::Bridge) {
            style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        }
        left_spans.push(Span::styled(" Bridge ", style));
    }

    let cwd_display = shorten_cwd(status.cwd);
    left_spans.push(Span::styled(
        format!(" {cwd_display}"),
        Style::default().fg(parse_theme_color(ds_footer.inactive)),
    ));
    if let Some(hint) = status.footer_action_hint {
        left_spans.push(Span::styled(
            format!("  {hint}"),
            Style::default().fg(parse_theme_color(ds_footer.chromeYellow)),
        ));
    }

    // Right side: active-agent timer, /new hint and context-budget indicator, right-aligned.
    // The timer stays visible while any coordinator-backed agent is active.
    // The /new hint appears once the replayed context is large enough to be useful.
    // The auto-compact warning still appears when the context budget is tight.
    let mut right_spans: Vec<Span<'static>> = Vec::new();
    if let Some(activity) = status.agent_activity {
        right_spans.push(Span::styled(
            total_elapsed_timer_label(activity.elapsed_ms),
            Style::default().fg(parse_theme_color(ds_footer.chromeYellow)),
        ));
    }
    if let Some(activity) = status.goal_activity {
        if !right_spans.is_empty() {
            right_spans.push(Span::raw("  "));
        }
        right_spans.push(Span::styled(
            goal_elapsed_timer_label(activity.status, activity.elapsed_ms),
            Style::default().fg(parse_theme_color(ds_footer.planMode)),
        ));
    }
    if let Some(hint) = status.new_session_hint.as_ref() {
        if !right_spans.is_empty() {
            right_spans.push(Span::raw("  "));
        }
        right_spans.push(Span::styled(
            hint.clone(),
            Style::default().fg(parse_theme_color(ds_footer.inactive)),
        ));
    }
    if let Some(pct) = status.context_left_pct {
        if pct < 20 {
            let color = if pct > 10 {
                parse_theme_color(ds_footer.warning)
            } else {
                parse_theme_color(ds_footer.error)
            };
            if !right_spans.is_empty() {
                right_spans.push(Span::raw("  "));
            }
            right_spans.push(Span::styled(
                format!("{pct}% until auto-compact"),
                Style::default().fg(color),
            ));
        }
    }

    let left_line = Line::from(left_spans);
    let right_line = Line::from(right_spans).alignment(ratatui::layout::Alignment::Right);

    // Render left and right on the same row.
    frame.render_widget(Paragraph::new(left_line), area);
    frame.render_widget(Paragraph::new(right_line), area);
    // Keep navigable priority items visible when right-aligned status text overlaps.
    if !priority_spans.is_empty() {
        frame.render_widget(Paragraph::new(Line::from(priority_spans)), area);
    }
}

pub(in crate::tui::runner) fn total_elapsed_timer_label(elapsed_ms: u64) -> String {
    let duration = format_footer_duration(elapsed_ms);
    format!("total elapsed · {duration}")
}

pub(in crate::tui::runner) fn goal_elapsed_timer_label(
    status: crate::goal::GoalStatus,
    elapsed_ms: u64,
) -> String {
    let duration = format_footer_duration(elapsed_ms);
    let status = match status {
        crate::goal::GoalStatus::Active => "active",
        crate::goal::GoalStatus::Paused => "paused",
        crate::goal::GoalStatus::Complete => "complete",
        crate::goal::GoalStatus::Archived => "archived",
    };
    format!("goal {status} · {duration}")
}

pub(in crate::tui::runner) fn format_footer_duration(elapsed_ms: u64) -> String {
    format_duration(elapsed_ms, DurationFormatOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use rebon_tui::promptinput::footer_navigation::FooterItem;

    #[test]
    fn bridge_footer_is_painted_only_when_visible_and_marks_selection() {
        for (visible, selected) in [(false, false), (true, false), (true, true)] {
            let mut app = AppState::default();
            app.rc_status.error = visible.then(|| "unreadable RC status".into());
            app.footer_selection = selected.then_some(FooterItem::Bridge);
            let status = StatusBarInfo {
                provider: "test",
                model: "model",
                cwd: "cwd",
                elapsed_ms: 0,
                effort_display: String::new(),
                fast_mode_display: String::new(),
                context_left_pct: None,
                agent_activity: None,
                goal_activity: None,
                footer_action_hint: None,
                new_session_hint: None,
            };
            let zones = LayoutZones {
                passthrough: false,
                show_sticky_header: false,
                sticky_header_text: None,
                pad_collapsed: false,
                scroll_padding_top: 0,
                show_pill: false,
                pill_count: 0,
                show_bottom_float: false,
                show_modal: false,
                modal_rows: 0,
                modal_columns: 0,
                modal_max_height: 0,
                show_bottom_bar: true,
                show_suggestions_overlay: false,
                show_dialog_overlay: false,
            };
            for width in [24, 80] {
                let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
                terminal
                    .draw(|frame| render_footer(frame, frame.area(), &app, &status, true, &zones))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let row: String = (0..width).map(|x| buffer[(x, 0)].symbol()).collect();
                assert_eq!(row.contains("Bridge"), visible, "{row}");
                if let Some(index) = row.find("Bridge") {
                    let modifiers = buffer[(index as u16, 0)].modifier;
                    assert_eq!(
                        modifiers.contains(Modifier::REVERSED | Modifier::BOLD),
                        selected
                    );
                }
            }
        }
    }
}
