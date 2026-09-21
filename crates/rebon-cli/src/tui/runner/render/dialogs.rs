use super::*;

use rebon_slash_commands::help::{
    catalog_command_rows, dismiss_footer, help_command_rows, version_bar_title, HelpBucket,
    HelpTab, HelpTabKey, GENERAL_BLURB, HELP_TABS, SHORTCUTS_HEADER,
};

use rebon_plugin_tasks::ui::teams::{DialogLevel, TeammateActivity};

pub(in crate::tui::runner) fn agent_switcher_height(app: &AppState) -> u16 {
    let rows = crate::tui::agent_switcher::build_agent_switcher_rows(
        &app.agent_task_snapshots(),
        !app.is_loading,
    );
    rows.len().min(4) as u16
}

pub(in crate::tui::runner) fn render_agent_switcher_rows(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    is_leader_idle: bool,
) {
    if area.width == 0 || area.height == 0 || app.background_tasks_dialog.is_some() {
        return;
    }

    let rows = crate::tui::agent_switcher::build_agent_switcher_rows(
        &app.agent_task_snapshots(),
        is_leader_idle,
    );
    if rows.is_empty() {
        return;
    }

    use rebon_tui::promptinput::footer_navigation::FooterItem;
    let ds = rebon_design_system::theme::get_active_theme();
    let selected_index = app.teammate_footer_index.min(rows.len().saturating_sub(1));
    let focused = app.footer_selection == Some(FooterItem::Tasks);
    let visible_count = rows.len().min(area.height as usize);
    let start = if selected_index >= visible_count {
        selected_index + 1 - visible_count
    } else {
        0
    };

    for (screen_row, (idx, row)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(area.height as usize)
        .enumerate()
    {
        let is_selected = focused && idx == selected_index;
        let row_area = Rect::new(
            area.x,
            area.y.saturating_add(screen_row as u16),
            area.width,
            1,
        );
        let marker = if is_selected { ">" } else { " " };
        let marker_style = if is_selected {
            Style::default()
                .fg(parse_theme_color(ds.suggestion))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(parse_theme_color(ds.inactive))
        };
        let name_style = agent_switcher_name_style(row.agent_color.as_deref(), row.is_idle, &ds)
            .add_modifier(if is_selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let activity_style = Style::default().fg(parse_theme_color(ds.inactive));
        let label = format!("@{}", row.agent_name);
        let metrics = row
            .metrics
            .as_deref()
            .filter(|metrics| WidthStr::width(*metrics).saturating_add(4) <= area.width as usize);
        let metrics_width = metrics.map(WidthStr::width).unwrap_or(0);
        let metrics_gap = if metrics.is_some() { 2 } else { 0 };
        let left_width = area
            .width
            .saturating_sub((metrics_width + metrics_gap) as u16);
        let left_area = Rect::new(row_area.x, row_area.y, left_width, 1);
        let used_width = 2 + WidthStr::width(label.as_str()) + 2;
        let max_activity_width = (left_width as usize).saturating_sub(used_width);
        let activity = truncate_to_ellipsis(&row.activity, max_activity_width);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{marker} "), marker_style),
                Span::styled(label, name_style),
                Span::styled("  ", activity_style),
                Span::styled(activity, activity_style),
            ])),
            left_area,
        );
        if let Some(metrics) = metrics {
            let metrics_area = Rect::new(
                row_area
                    .x
                    .saturating_add(row_area.width.saturating_sub(metrics_width as u16)),
                row_area.y,
                metrics_width as u16,
                1,
            );
            frame.render_widget(
                Paragraph::new(Span::styled(metrics, activity_style)),
                metrics_area,
            );
        }
    }
}

pub(in crate::tui::runner) fn agent_switcher_name_style(
    color: Option<&str>,
    is_idle: bool,
    ds: &rebon_design_system::theme::Theme,
) -> Style {
    let fg = match color {
        Some("success") => parse_theme_color(ds.success),
        Some("error") => parse_theme_color(ds.error),
        Some("warning") => parse_theme_color(ds.warning),
        Some("rebon") => parse_theme_color(ds.rebon),
        Some("blue") => parse_theme_color(ds.professionalBlue),
        _ => parse_theme_color(ds.text),
    };
    let mut style = Style::default().fg(fg);
    if is_idle {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

pub(in crate::tui::runner) fn render_help_overlay(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
) {
    render_help_panel(frame, help_overlay_rect(area), app);
}

pub(in crate::tui::runner) fn render_help_panel(frame: &mut Frame, area: Rect, app: &mut AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let tabs = &HELP_TABS;
    if app.help_tab_index >= tabs.len() {
        app.help_tab_index = 0;
    }

    let ds = rebon_design_system::theme::get_active_theme();
    let border_color = parse_theme_color(ds.professionalBlue);
    let text_style = Style::default().fg(parse_theme_color(ds.text));
    let dim_style = Style::default().fg(parse_theme_color(ds.inactive));
    let selected_style = Style::default()
        .fg(parse_theme_color(ds.suggestion))
        .add_modifier(Modifier::BOLD);
    let header_style = text_style.add_modifier(Modifier::BOLD);
    let title = format!(" {} Help ", version_bar_title(env!("CARGO_PKG_VERSION")));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(title, header_style));
    let inner = block.inner(area);
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let bottom = inner.y.saturating_add(inner.height);
    let footer_y = bottom.saturating_sub(1);
    let body_bottom = footer_y.saturating_sub(1);
    let mut y = inner.y;

    if y < body_bottom {
        let tab_spans = help_tab_spans(tabs, app.help_tab_index, selected_style, dim_style);
        frame.render_widget(
            Paragraph::new(Line::from(tab_spans)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(2);
    }

    let selected = &tabs[app.help_tab_index];
    match selected.key {
        HelpTabKey::General => render_help_general_tab(
            frame,
            inner,
            y,
            body_bottom,
            text_style,
            dim_style,
            header_style,
        ),
        HelpTabKey::Commands | HelpTabKey::Custom => render_help_commands_tab(
            frame,
            inner,
            y,
            body_bottom,
            app,
            selected,
            text_style,
            dim_style,
            selected_style,
        ),
    }

    if footer_y < bottom {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " ←/→ or Tab switch tabs · Esc/Enter close ",
                dim_style,
            ))),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    }
}

fn help_overlay_rect(area: Rect) -> Rect {
    let base = centered_rect(area, 90, 70);
    let max_height = area.height.saturating_sub(2).max(1);
    let max_width = area.width.saturating_sub(2).max(1);
    let height = base.height.max(18.min(max_height)).min(max_height);
    let width = base.width.max(50.min(max_width)).min(max_width);
    let x = base
        .x
        .min(area.x.saturating_add(area.width.saturating_sub(width)));
    let y = base
        .y
        .min(area.y.saturating_add(area.height.saturating_sub(height)));
    Rect::new(x, y, width, height)
}

fn help_tab_spans(
    tabs: &[HelpTab],
    selected_index: usize,
    selected_style: Style,
    dim_style: Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (idx, tab) in tabs.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("  ", dim_style));
        }
        let label = if idx == selected_index {
            format!("[{}]", tab.title)
        } else {
            format!(" {} ", tab.title)
        };
        spans.push(Span::styled(
            label,
            if idx == selected_index {
                selected_style
            } else {
                dim_style
            },
        ));
    }
    spans
}

fn render_help_general_tab(
    frame: &mut Frame,
    inner: Rect,
    mut y: u16,
    bottom: u16,
    text_style: Style,
    dim_style: Style,
    header_style: Style,
) {
    y = render_wrapped_text(frame, inner, y, bottom, GENERAL_BLURB, text_style, 1);
    y = y.saturating_add(1);
    if y < bottom {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {SHORTCUTS_HEADER}"),
                header_style,
            ))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    }
    render_help_shortcut_columns(frame, inner, y, bottom, text_style, dim_style, header_style);
}

fn render_help_shortcut_columns(
    frame: &mut Frame,
    inner: Rect,
    mut y: u16,
    bottom: u16,
    text_style: Style,
    dim_style: Style,
    header_style: Style,
) {
    let sections: [(&str, &[&str]); 3] = [
        (
            "Prompt",
            &[
                "? or /help open help",
                "! bash mode",
                "/ slash commands",
                "@ file paths",
                "& background task",
            ],
        ),
        (
            "Navigation",
            &[
                "Esc cancel or close",
                "Enter submit or close help",
                "Up/Down history",
                "PageUp/PageDown scroll",
                "\\ + Enter newline",
                "Ctrl+J newline",
            ],
        ),
        (
            "Actions",
            &[
                "shift+tab cycles default/plan/accept edits/auto",
                "Ctrl+O toggle tool output",
                "Ctrl+E show all output",
                "Ctrl+B background tasks",
                "Ctrl+Z undo",
                "Ctrl+V paste clipboard",
                "Alt+V paste image",
            ],
        ),
    ];

    let column_count = if inner.width >= 96 {
        3usize
    } else if inner.width >= 64 {
        2usize
    } else {
        1usize
    };
    if column_count == 1 {
        for (title, rows) in sections {
            if y >= bottom {
                return;
            }
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(format!(" {title}"), header_style))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y = y.saturating_add(1);
            for row_text in rows {
                if y >= bottom {
                    return;
                }
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!("  {row_text}"),
                        text_style,
                    ))),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y = y.saturating_add(1);
            }
            y = y.saturating_add(1);
        }
        return;
    }

    for chunk in sections.chunks(column_count) {
        if y >= bottom {
            return;
        }
        let col_width = inner.width / chunk.len() as u16;
        for (idx, (title, _)) in chunk.iter().enumerate() {
            let x = inner.x.saturating_add(col_width.saturating_mul(idx as u16));
            let width = if idx + 1 == chunk.len() {
                inner.x.saturating_add(inner.width).saturating_sub(x)
            } else {
                col_width
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(format!(" {title}"), header_style))),
                Rect::new(x, y, width, 1),
            );
        }
        y = y.saturating_add(1);
        let max_rows = chunk.iter().map(|(_, rows)| rows.len()).max().unwrap_or(0);
        for row_idx in 0..max_rows {
            if y >= bottom {
                return;
            }
            for (idx, (_, rows)) in chunk.iter().enumerate() {
                let Some(row_text) = rows.get(row_idx) else {
                    continue;
                };
                let x = inner.x.saturating_add(col_width.saturating_mul(idx as u16));
                let width = if idx + 1 == chunk.len() {
                    inner.x.saturating_add(inner.width).saturating_sub(x)
                } else {
                    col_width
                };
                let clipped = truncate_to_ellipsis(row_text, width.saturating_sub(2) as usize);
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(format!("  {clipped}"), text_style))),
                    Rect::new(x, y, width, 1),
                );
            }
            y = y.saturating_add(1);
        }
        y = y.saturating_add(1);
    }

    if y < bottom {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}", dismiss_footer("Esc")),
                dim_style,
            ))),
            Rect::new(inner.x, y, inner.width, 1),
        );
    }
}

/// Paint one of the two command lists.
///
/// The `commands` tab reads the command-registry seat here rather than
/// `app.slash_commands`: that list is seeded before the session boots the
/// kernel, so it is the built-in fallback table without the commands the
/// `agents`, `tasks`, `updater` and `profile` plugins register, and it does
/// not follow a switch flipped in `/plugins` afterwards. Skills are the other
/// way round — they are not seat commands, so the `custom-commands` tab does
/// read the front end's own list.
fn render_help_commands_tab(
    frame: &mut Frame,
    inner: Rect,
    mut y: u16,
    bottom: u16,
    app: &AppState,
    tab: &HelpTab,
    text_style: Style,
    dim_style: Style,
    selected_style: Style,
) {
    if let Some(title) = tab.list_title {
        if y < bottom {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {title}"),
                    text_style.add_modifier(Modifier::BOLD),
                ))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y = y.saturating_add(1);
        }
    }

    // Exhaustive on purpose: a fourth tab has to say which list it paints
    // here rather than falling into the catalog's by default. `General`
    // never reaches this function; `render_help_panel` sends it elsewhere.
    let rows = match tab.key {
        HelpTabKey::Custom => help_command_rows(&app.slash_commands, HelpBucket::Custom),
        HelpTabKey::Commands | HelpTabKey::General => {
            catalog_command_rows(rebon_slash_commands::Surface::Tui)
        }
    };

    if rows.is_empty() {
        if let Some(message) = tab.empty_message {
            if y < bottom {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(format!(" {message}"), dim_style))),
                    Rect::new(inner.x, y, inner.width, 1),
                );
            }
        }
        return;
    }

    let label_width = rows
        .iter()
        .map(|row| row.label.width())
        .max()
        .unwrap_or(0)
        .min(24);
    let description_width = inner
        .width
        .saturating_sub(label_width as u16)
        .saturating_sub(4) as usize;

    for row in rows {
        if y >= bottom {
            break;
        }
        let label = truncate_to_ellipsis(&row.label, label_width);
        let label = format!(" {:label_width$}", label, label_width = label_width);
        let description = truncate_to_ellipsis(&row.description, description_width);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(label, selected_style),
                Span::styled("  ", dim_style),
                Span::styled(description, text_style),
            ])),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    }
}

/// Render the teams dialog overlay in the prompt area.
pub(in crate::tui::runner) fn render_teams_dialog(frame: &mut Frame, area: Rect, app: &AppState) {
    let Some(dialog) = app.teams_dialog.as_ref() else {
        return;
    };

    let ds = rebon_design_system::theme::get_active_theme();
    let border_color = parse_theme_color(ds.subtle);
    let dim_style = Style::default().fg(parse_theme_color(ds.inactive));
    let default_style = Style::default().fg(parse_theme_color(ds.text));
    let selected_style = Style::default()
        .fg(parse_theme_color(ds.suggestion))
        .add_modifier(Modifier::BOLD);

    let team_name = match &dialog.state.dialog_level {
        DialogLevel::TeammateList { team_name } | DialogLevel::TeammateDetail { team_name, .. } => {
            team_name
        }
    };
    let title = format!(" Team {team_name} ");
    let subtitle = format!(
        "{} {}",
        dialog.state.teammates.len(),
        if dialog.state.teammates.len() == 1 {
            "teammate"
        } else {
            "teammates"
        }
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut y = inner.y;
    let bottom = inner.y + inner.height;

    if y < bottom {
        let row = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {subtitle}"), dim_style))),
            row,
        );
        y += 1;
    }

    if y < bottom {
        y += 1;
    }

    match &dialog.state.dialog_level {
        DialogLevel::TeammateList { .. } => {
            let list_bottom = bottom.saturating_sub(2);
            for (idx, teammate) in dialog.state.teammates.iter().enumerate() {
                if y >= list_bottom {
                    break;
                }
                let is_selected = idx == dialog.state.selected_index;
                let style = if is_selected {
                    selected_style
                } else {
                    default_style
                };
                let prefix = if is_selected { "▸ " } else { "  " };
                let status_tag = match teammate.status {
                    TeammateActivity::Running => "[running]",
                    TeammateActivity::Idle => "[idle]",
                    TeammateActivity::Unknown => "[unknown]",
                };
                let mut spans = vec![
                    Span::styled(format!(" {prefix}"), style),
                    Span::styled(format!("{status_tag} "), dim_style),
                    Span::styled(format!("@{}", teammate.name), style),
                ];
                if teammate.is_hidden {
                    spans.push(Span::styled(" [hidden]", dim_style));
                }
                if let Some(model) = teammate.model.as_ref() {
                    spans.push(Span::styled(format!(" ({model})"), dim_style));
                }
                let row = Rect::new(inner.x, y, inner.width, 1);
                frame.render_widget(Paragraph::new(Line::from(spans)), row);
                y += 1;
            }
            if dialog.state.teammates.is_empty() && y < list_bottom {
                let row = Rect::new(inner.x, y, inner.width, 1);
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(" (no teammates)", dim_style))),
                    row,
                );
            }
            if bottom > inner.y {
                let footer_row = Rect::new(inner.x, bottom.saturating_sub(1), inner.width, 1);
                let footer = if dialog.supports_hide_show() {
                    " ↑/↓ select · Enter view · k kill · s shutdown · p prune idle · h hide/show · H hide/show all · Esc close"
                } else {
                    " ↑/↓ select · Enter view · k kill · s shutdown · p prune idle · Esc close"
                };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(footer, dim_style))),
                    footer_row,
                );
            }
        }
        DialogLevel::TeammateDetail { .. } => {
            let Some(teammate) = dialog.state.current_teammate() else {
                return;
            };
            if y < bottom {
                let row = Rect::new(inner.x, y, inner.width, 1);
                let status_tag = match teammate.status {
                    TeammateActivity::Running => "running",
                    TeammateActivity::Idle => "idle",
                    TeammateActivity::Unknown => "unknown",
                };
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(" Teammate: ", dim_style),
                        Span::styled(format!("@{}", teammate.name), default_style),
                        Span::styled(format!(" · {status_tag}"), dim_style),
                        Span::styled(if teammate.is_hidden { " · hidden" } else { "" }, dim_style),
                    ])),
                    row,
                );
                y += 1;
            }
            if let Some(model) = teammate.model.as_ref() {
                if y < bottom {
                    let row = Rect::new(inner.x, y, inner.width, 1);
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(" Model: ", dim_style),
                            Span::styled(model.clone(), default_style),
                        ])),
                        row,
                    );
                    y += 1;
                }
            }
            if y < bottom {
                let row = Rect::new(inner.x, y, inner.width, 1);
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        " Prompt:",
                        default_style.add_modifier(Modifier::BOLD),
                    ))),
                    row,
                );
                y += 1;
            }
            let _ = render_wrapped_text(
                frame,
                inner,
                y,
                bottom.saturating_sub(1),
                teammate.prompt.as_deref().unwrap_or(""),
                default_style,
                2,
            );
            if bottom > inner.y {
                let footer_row = Rect::new(inner.x, bottom.saturating_sub(1), inner.width, 1);
                let footer = if dialog.current_teammate_can_hide() {
                    " ← back · Enter view output · k kill · s shutdown · h hide/show · Esc close"
                } else {
                    " ← back · Enter view output · k kill · s shutdown · Esc close"
                };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(footer, dim_style))),
                    footer_row,
                );
            }
        }
    }
}

/// Wrap a multi-line string to `inner.width` columns and render it
/// starting at `y`, returning the next free row. `indent` columns
/// of leading whitespace are inserted on every rendered row.
pub(in crate::tui::runner) fn render_wrapped_text(
    frame: &mut Frame,
    inner: Rect,
    mut y: u16,
    bottom: u16,
    text: &str,
    style: Style,
    indent: u16,
) -> u16 {
    let wrap_width = inner.width.saturating_sub(indent + 1).max(1) as usize;
    for raw_line in text.split('\n') {
        if y >= bottom {
            return y;
        }
        // Simple char-count wrap — good enough for ASCII prompts and
        // avoids pulling in a wrapping dep. Multi-byte characters
        // count as 1 column, an approximation of true display width.
        let mut remaining = raw_line;
        loop {
            if remaining.is_empty() {
                break;
            }
            let take = remaining.chars().take(wrap_width).collect::<String>();
            let row = Rect::new(inner.x, y, inner.width, 1);
            let prefix = " ".repeat(indent as usize);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(format!("{prefix}{take}"), style))),
                row,
            );
            y += 1;
            if y >= bottom {
                return y;
            }
            let consumed = take.chars().count();
            remaining = remaining
                .char_indices()
                .nth(consumed)
                .map(|(i, _)| &remaining[i..])
                .unwrap_or("");
        }
        if raw_line.is_empty() && y < bottom {
            y += 1;
        }
    }
    y
}

/// Paint the background-tasks panel.
///
/// The panel itself is `rebon_plugin_tasks::ui::background_tasks_dialog`:
/// it holds the state, takes the keys, re-derives its layout from the
/// registry every frame, and describes every row it wants shown. What is
/// left here is choosing the width to build those rows for and handing
/// the description to the shared painter.
pub(in crate::tui::runner) fn render_background_tasks_dialog(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
) {
    let Some(dialog) = app.background_tasks_dialog.as_ref() else {
        return;
    };
    let view = dialog.panel_view(usize::from(area.width.saturating_sub(2)));
    rebon_tui::dialog_view::render_panel_view(frame, area, &view);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn painted(app: &mut AppState) -> String {
        // Tall enough for the whole catalog: a clipped list would make the
        // assertions below depend on where the alphabet runs out.
        let backend = TestBackend::new(100, 90);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render_help_panel(frame, Rect::new(0, 0, 100, 90), app))
            .expect("draw the help panel");
        let buffer = terminal.backend().buffer().clone();
        (0..90)
            .map(|y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            )
    }

    /// The commands tab lists what the command-registry seat holds when the
    /// screen opens, not what the front end sampled at start-up: the TUI
    /// seeds its slash-command list before the session boots the kernel, so
    /// the commands `agents`, `tasks` and `updater` register are not in it.
    #[test]
    fn the_commands_tab_lists_plugin_commands_the_front_end_never_sampled() {
        rebon_harness::kernel_bootstrap::process_kernel();
        let mut app = AppState::default();
        app.slash_commands.clear();
        app.help_tab_index = 1;

        let screen = painted(&mut app);
        for command in ["/agents", "/tasks", "/update"] {
            assert!(
                screen.contains(command),
                "{command} is registered on the seat and must be listed:
{screen}"
            );
        }
    }

    /// The custom tab is the user's skills, which are not seat commands: they
    /// reach the screen through the front end's own list.
    #[test]
    fn the_custom_tab_lists_the_front_ends_skills_and_nothing_else() {
        rebon_harness::kernel_bootstrap::process_kernel();
        let mut app = AppState::default();
        app.slash_commands = vec![rebon_types::SlashCommand {
            name: "commit".to_string(),
            description: "Write a commit message".to_string(),
            input: None,
            category: Some(rebon_types::SlashCommandCategory::Skill),
            aliases: Vec::new(),
        }];
        app.help_tab_index = 2;

        let screen = painted(&mut app);
        assert!(
            screen.contains("/commit"),
            "the skill is listed:
{screen}"
        );
        assert!(
            !screen.contains("/agents"),
            "a seat command is not a custom command:
{screen}"
        );
    }
}
