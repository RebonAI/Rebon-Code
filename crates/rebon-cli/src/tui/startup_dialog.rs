//! Minimal startup dialogs used before the main TUI session exists.

use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use rebon_design_system::theme::get_active_theme;
use rebon_dialog::common::DialogColor;
use rebon_dialog::dev_channels::{self, ChannelEntryInput, DevChannelsAction, DevChannelsValue};
use rebon_dialog::invalid_config::{self, InvalidConfigAction, InvalidConfigValue};
use rebon_dialog::invalid_settings::{
    self, InvalidSettingsAction, InvalidSettingsValue, ValidationErrorInput,
};
use rebon_dialog::mcp_server_approval::{self, McpServerApprovalAction, McpServerApprovalValue};
use rebon_dialog::mcp_server_multiselect::{self, McpServerMultiselectAction};
use rebon_tui::parse_theme_color;

use crate::rebon_config::ConfigParseFailure;
use crate::tui::dialog_support::truncate_to_width;
use crate::tui::terminal::TerminalGuard;

/// Result of the trust dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustDialogAction {
    /// User accepted trust — persist and continue.
    Accept,
    /// User declined — exit with code 1.
    Exit,
}

/// Show the per-directory trust dialog. Blocks until the user makes
/// a choice. Shown before the main TUI
/// event loop for any directory that hasn't been trusted yet.
pub fn run_trust_dialog(cwd: &str) -> anyhow::Result<TrustDialogAction> {
    let options = vec![
        "Yes, I trust this folder".to_string(),
        "No, exit".to_string(),
    ];
    let body = vec![
        format!("> You are in {cwd}"),
        String::new(),
        "Do you trust the contents of this directory?".to_string(),
        "Working with untrusted contents comes with higher risk of prompt injection.".to_string(),
    ];
    let selected = run_select_dialog(
        "Trust Directory",
        DialogColor::Warning,
        &body,
        &options,
        None,
        1, // cancel_index = "No, exit"
    )?;
    Ok(match selected {
        0 => TrustDialogAction::Accept,
        _ => TrustDialogAction::Exit,
    })
}

/// What to do about kernel plugins with no runtime to run them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRuntimeAction {
    /// Install the pinned build now, then continue into the session.
    Install,
    /// Start without kernel plugins. The configuration still asks for them, so
    /// this is not remembered — nothing has been settled, only postponed.
    Continue,
}

/// Offer the one command that fixes a missing plugin runtime, at the moment it
/// is missing.
///
/// Rebon does not fetch a runtime on its own — that decision is the user's, and
/// this is where they get to make it without first having to find out that
/// `rebon node install` exists. `kernelPlugins` is opt-in, so this can only ever
/// appear to someone who wrote the section and therefore already wants it.
pub fn run_node_runtime_dialog(
    entries: usize,
    reason: &str,
    version: &str,
) -> anyhow::Result<NodeRuntimeAction> {
    let options = vec![
        format!("Install Node {version} now"),
        "Start without kernel plugins".to_string(),
    ];
    let plural = if entries == 1 { "" } else { "s" };
    let body = vec![
        format!("> {entries} kernel plugin{plural} configured, and no Node runtime to run them"),
        String::new(),
        format!("Reason: {reason}"),
        String::new(),
        format!(
            "Rebon can install Node {version} under ~/.rebon. It accepts only bytes \
             matching a checksum built into this binary."
        ),
        "Already have a Node you want used? Set REBON_PLUGIN_NODE to it and restart.".to_string(),
    ];
    let selected = run_select_dialog(
        "Kernel plugin runtime",
        DialogColor::Warning,
        &body,
        &options,
        Some("Enter to choose · Esc to start without".to_string()),
        1, // cancel_index = start without
    )?;
    Ok(match selected {
        0 => NodeRuntimeAction::Install,
        _ => NodeRuntimeAction::Continue,
    })
}

pub fn run_invalid_config_dialog(
    failure: &ConfigParseFailure,
) -> anyhow::Result<InvalidConfigAction> {
    let options: Vec<String> = invalid_config::build_options()
        .into_iter()
        .map(|option| option.label)
        .collect();
    let body = vec![
        invalid_config::build_body(&failure.file_path.display().to_string()),
        failure.message.clone(),
        invalid_config::PROMPT_TEXT.to_string(),
    ];
    let selected = run_select_dialog(
        invalid_config::TITLE,
        invalid_config::DIALOG_COLOR,
        &body,
        &options,
        Some("Reset writes a safe default config and exits.".to_string()),
        0,
    )?;

    let value = match selected {
        0 => InvalidConfigValue::Exit,
        1 => InvalidConfigValue::Reset,
        _ => InvalidConfigValue::Exit,
    };
    Ok(invalid_config::handle_event(value))
}

pub fn run_dev_channels_dialog(
    entries: &[rebon_plugin_mcp::runtime::ChannelEntry],
) -> anyhow::Result<DevChannelsAction> {
    let options: Vec<String> = dev_channels::build_options()
        .into_iter()
        .map(|option| option.label)
        .collect();
    let dialog_entries: Vec<ChannelEntryInput> = entries
        .iter()
        .map(|entry| match entry {
            rebon_plugin_mcp::runtime::ChannelEntry::Plugin {
                name, marketplace, ..
            } => ChannelEntryInput::Plugin {
                name: name.clone(),
                marketplace: marketplace.clone(),
            },
            rebon_plugin_mcp::runtime::ChannelEntry::Server { name, .. } => {
                ChannelEntryInput::Server { name: name.clone() }
            }
        })
        .collect();
    let body = vec![
        dev_channels::WARNING_PARAGRAPH_1.to_string(),
        dev_channels::WARNING_PARAGRAPH_2.to_string(),
        format!(
            "Channels: {}",
            dev_channels::format_channel_list(&dialog_entries)
        ),
    ];
    let selected = run_select_dialog(
        dev_channels::TITLE,
        dev_channels::DIALOG_COLOR,
        &body,
        &options,
        None,
        1,
    )?;
    let value = match selected {
        0 => DevChannelsValue::Accept,
        _ => DevChannelsValue::Exit,
    };
    Ok(dev_channels::handle_event(value))
}

pub fn run_invalid_settings_dialog(
    errors: &[ValidationErrorInput],
) -> anyhow::Result<InvalidSettingsAction> {
    let options: Vec<String> = invalid_settings::build_options()
        .into_iter()
        .map(|option| option.label)
        .collect();
    let mut body = Vec::new();
    body.push(format!(
        "{} invalid settings file(s) detected:",
        invalid_settings::error_count(errors)
    ));
    for error in errors {
        body.push(format!("{} — {}", error.path, error.message));
    }
    let selected = run_select_dialog(
        invalid_settings::TITLE,
        invalid_settings::DIALOG_COLOR,
        &body,
        &options,
        Some(invalid_settings::FOOTER_TEXT.to_string()),
        0,
    )?;

    let value = match selected {
        0 => InvalidSettingsValue::Exit,
        1 => InvalidSettingsValue::Continue,
        _ => InvalidSettingsValue::Exit,
    };
    Ok(invalid_settings::handle_event(value))
}

pub fn run_mcp_server_approval_dialog(
    server_name: &str,
) -> anyhow::Result<McpServerApprovalAction> {
    let options: Vec<String> = mcp_server_approval::build_options()
        .into_iter()
        .map(|option| option.label)
        .collect();
    let body = vec![String::from(
        "MCP servers may execute code or access system resources. All tool calls require approval.",
    )];
    let selected = run_select_dialog(
        &mcp_server_approval::build_title(server_name),
        mcp_server_approval::DIALOG_COLOR,
        &body,
        &options,
        None,
        2,
    )?;
    let value = match selected {
        0 => McpServerApprovalValue::YesAll,
        1 => McpServerApprovalValue::Yes,
        _ => McpServerApprovalValue::No,
    };
    Ok(mcp_server_approval::handle_event(value, server_name))
}

pub fn run_mcp_server_multiselect_dialog(
    server_names: &[String],
) -> anyhow::Result<McpServerMultiselectAction> {
    let body = vec![
        String::from(
            "MCP servers may execute code or access system resources. All tool calls require approval.",
        ),
        String::from("Select any you wish to enable."),
    ];
    let selected = run_multi_select_dialog(
        &mcp_server_multiselect::build_title(server_names.len()),
        mcp_server_multiselect::DIALOG_COLOR,
        &body,
        server_names,
        Some(String::from(
            "Space select · Enter confirm · Esc reject all",
        )),
    )?;
    Ok(mcp_server_multiselect::handle_submit(
        server_names,
        &selected,
    ))
}

fn run_select_dialog(
    title: &str,
    color: DialogColor,
    body_lines: &[String],
    options: &[String],
    footer: Option<String>,
    cancel_index: usize,
) -> anyhow::Result<usize> {
    let mut guard = TerminalGuard::enter()?;
    let mut selected = 0usize;

    // Drain any buffered input events (e.g. the Enter key that
    // launched the process) so they don't instantly confirm the
    // dialog before the user has a chance to read it.
    while event::poll(Duration::from_millis(0))? {
        let _ = event::read()?;
    }

    loop {
        guard.terminal().draw(|frame| {
            let area = centered_rect(frame.area(), 88, 72);
            render_dialog(
                frame,
                area,
                title,
                color,
                body_lines,
                options,
                footer.as_deref(),
                selected,
            );
        })?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => match key.code {
                KeyCode::Esc => return Ok(cancel_index),
                KeyCode::Up | KeyCode::Left => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                    if selected + 1 < options.len() {
                        selected += 1;
                    }
                }
                KeyCode::Enter => return Ok(selected),
                _ => {}
            },
            _ => {}
        }
    }
}

fn run_multi_select_dialog(
    title: &str,
    color: DialogColor,
    body_lines: &[String],
    options: &[String],
    footer: Option<String>,
) -> anyhow::Result<Vec<String>> {
    let mut guard = TerminalGuard::enter()?;
    let mut focused = 0usize;
    let mut selected = options.to_vec();

    // Drain buffered input — same rationale as run_select_dialog.
    while event::poll(Duration::from_millis(0))? {
        let _ = event::read()?;
    }

    loop {
        guard.terminal().draw(|frame| {
            let area = centered_rect(frame.area(), 88, 72);
            render_multi_select_dialog(
                frame,
                area,
                title,
                color,
                body_lines,
                options,
                footer.as_deref(),
                focused,
                &selected,
            );
        })?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => match key.code {
                KeyCode::Esc => return Ok(Vec::new()),
                KeyCode::Up => {
                    focused = focused.saturating_sub(1);
                }
                KeyCode::Down => {
                    if focused + 1 < options.len() {
                        focused += 1;
                    }
                }
                KeyCode::Char(' ') => {
                    if let Some(option) = options.get(focused) {
                        if selected.iter().any(|name| name == option) {
                            selected.retain(|name| name != option);
                        } else {
                            selected.push(option.clone());
                        }
                    }
                }
                KeyCode::Enter => return Ok(selected),
                _ => {}
            },
            _ => {}
        }
    }
}

fn render_dialog(
    frame: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    color: DialogColor,
    body_lines: &[String],
    options: &[String],
    footer: Option<&str>,
    selected: usize,
) {
    let ds = get_active_theme();
    let dialog_bg = parse_theme_color(ds.inverseText);
    let border_color = match color {
        DialogColor::Warning => ds.warning,
        DialogColor::Error => ds.error,
        DialogColor::Permission => ds.permission,
        DialogColor::Success => ds.success,
        DialogColor::Default => ds.subtle,
    };
    let border = Style::default()
        .fg(parse_theme_color(border_color))
        .bg(dialog_bg);
    let title_style = border.add_modifier(Modifier::BOLD);
    let dim = Style::default()
        .fg(parse_theme_color(ds.inactive))
        .bg(dialog_bg);
    let normal = Style::default()
        .fg(parse_theme_color(ds.text))
        .bg(dialog_bg);
    let focused = Style::default()
        .fg(parse_theme_color(ds.suggestion))
        .bg(dialog_bg)
        .add_modifier(Modifier::BOLD);

    frame.render_widget(Clear, area);
    let block = Block::default()
        .style(Style::default().bg(dialog_bg))
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(format!(" {title} "), title_style));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let body_height = inner.height.saturating_sub(options.len() as u16 + 2);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(body_height.max(1)),
            Constraint::Length(options.len() as u16),
            Constraint::Length(footer.map(|_| 1).unwrap_or(0) as u16),
        ])
        .split(inner);

    render_body(frame, sections[0], body_lines, normal, dim);
    render_options(frame, sections[1], options, selected, focused, normal);
    if let Some(footer) = footer {
        render_footer(frame, sections[2], footer, dim);
    }
}

fn render_multi_select_dialog(
    frame: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    color: DialogColor,
    body_lines: &[String],
    options: &[String],
    footer: Option<&str>,
    focused: usize,
    selected_values: &[String],
) {
    let ds = get_active_theme();
    let dialog_bg = parse_theme_color(ds.inverseText);
    let border_color = match color {
        DialogColor::Warning => ds.warning,
        DialogColor::Error => ds.error,
        DialogColor::Permission => ds.permission,
        DialogColor::Success => ds.success,
        DialogColor::Default => ds.subtle,
    };
    let border = Style::default()
        .fg(parse_theme_color(border_color))
        .bg(dialog_bg);
    let title_style = border.add_modifier(Modifier::BOLD);
    let dim = Style::default()
        .fg(parse_theme_color(ds.inactive))
        .bg(dialog_bg);
    let normal = Style::default()
        .fg(parse_theme_color(ds.text))
        .bg(dialog_bg);
    let focused_style = Style::default()
        .fg(parse_theme_color(ds.suggestion))
        .bg(dialog_bg)
        .add_modifier(Modifier::BOLD);

    frame.render_widget(Clear, area);
    let block = Block::default()
        .style(Style::default().bg(dialog_bg))
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(format!(" {title} "), title_style));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let body_height = inner.height.saturating_sub(options.len() as u16 + 2);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(body_height.max(1)),
            Constraint::Length(options.len() as u16),
            Constraint::Length(footer.map(|_| 1).unwrap_or(0) as u16),
        ])
        .split(inner);

    render_body(frame, sections[0], body_lines, normal, dim);

    for (idx, option) in options.iter().enumerate() {
        if idx as u16 >= sections[1].height {
            break;
        }
        let checked = selected_values.iter().any(|value| value == option);
        let indicator = if checked { "[x]" } else { "[ ]" };
        let style = if idx == focused {
            focused_style
        } else {
            normal
        };
        let row_area = Rect::new(
            sections[1].x,
            sections[1].y + idx as u16,
            sections[1].width,
            1,
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(indicator.to_string(), style),
                Span::raw(" "),
                Span::styled(
                    truncate_to_width(option, sections[1].width.saturating_sub(4) as usize),
                    style,
                ),
            ])),
            row_area,
        );
    }

    if let Some(footer) = footer {
        render_footer(frame, sections[2], footer, dim);
    }
}

fn render_body(
    frame: &mut ratatui::Frame,
    area: Rect,
    body_lines: &[String],
    normal: Style,
    dim: Style,
) {
    let mut y = area.y;
    for line in body_lines {
        if y >= area.bottom() {
            break;
        }
        let wrapped = wrap_lines(line, area.width as usize);
        for row in wrapped {
            if y >= area.bottom() {
                break;
            }
            let row_area = Rect::new(area.x, y, area.width, 1);
            let style = if line.contains("detected:") || line == "Choose an option:" {
                normal.add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(row, style))),
                row_area,
            );
            y += 1;
        }
    }
}

fn render_options(
    frame: &mut ratatui::Frame,
    area: Rect,
    options: &[String],
    selected: usize,
    focused: Style,
    normal: Style,
) {
    for (idx, option) in options.iter().enumerate() {
        if idx as u16 >= area.height {
            break;
        }
        let is_selected = idx == selected;
        let style = if is_selected { focused } else { normal };
        let prefix = if is_selected { ">" } else { " " };
        let row_area = Rect::new(area.x, area.y + idx as u16, area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{prefix} "), style),
                Span::styled(
                    truncate_to_width(option, area.width.saturating_sub(2) as usize),
                    style,
                ),
            ])),
            row_area,
        );
    }
}

fn render_footer(frame: &mut ratatui::Frame, area: Rect, footer: &str, dim: Style) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_to_width(footer, area.width as usize),
            dim,
        ))),
        area,
    );
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
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

fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut used = 0usize;
        for word in raw_line.split_whitespace() {
            let extra = if current.is_empty() { 0 } else { 1 };
            let word_width = rebon_width::str_width(word);
            if used + extra + word_width > width && !current.is_empty() {
                rows.push(current);
                current = String::new();
                used = 0;
            }
            if !current.is_empty() {
                current.push(' ');
                used += 1;
            }
            current.push_str(word);
            used += word_width;
        }
        if !current.is_empty() {
            rows.push(current);
        }
    }
    rows
}
