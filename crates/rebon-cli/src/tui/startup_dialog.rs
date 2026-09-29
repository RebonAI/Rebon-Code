//! Minimal startup dialogs used before the main TUI session exists.

use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
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
            let area = frame.area();
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
            let area = frame.area();
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

/// The palette a startup dialog draws with.
struct DialogStyles {
    background: Style,
    border: Style,
    title: Style,
    headline: Style,
    normal: Style,
    dim: Style,
    focused: Style,
    key: Style,
}

impl DialogStyles {
    fn for_color(color: DialogColor) -> Self {
        let ds = get_active_theme();
        let dialog_bg = parse_theme_color(ds.inverseText);
        let background = Style::default().bg(dialog_bg);
        let fg = |key: &str| background.fg(parse_theme_color(key));
        let border_color = match color {
            DialogColor::Warning => ds.warning,
            DialogColor::Error => ds.error,
            DialogColor::Permission => ds.permission,
            DialogColor::Success => ds.success,
            DialogColor::Default => ds.subtle,
        };
        Self {
            background,
            border: fg(border_color),
            title: fg(border_color).add_modifier(Modifier::BOLD),
            headline: fg(ds.text).add_modifier(Modifier::BOLD),
            normal: fg(ds.text),
            dim: fg(ds.inactive),
            focused: fg(ds.suggestion).add_modifier(Modifier::BOLD),
            key: fg(ds.text),
        }
    }
}

/// How a body line reads. Dialog bodies are plain strings, so the tone comes
/// from the line itself: a `> ` lead-in is the headline, a question or a
/// "detected:" summary is what the user is being asked about, and everything
/// else is supporting detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyTone {
    Headline,
    Normal,
    Dim,
}

fn body_tone(line: &str) -> (BodyTone, &str) {
    if let Some(rest) = line.strip_prefix("> ") {
        return (BodyTone::Headline, rest);
    }
    let trimmed = line.trim_end();
    if trimmed.contains("detected:") || trimmed == "Choose an option:" || trimmed.ends_with('?') {
        (BodyTone::Normal, line)
    } else {
        (BodyTone::Dim, line)
    }
}

/// Horizontal padding between the frame and the content.
const DIALOG_PAD_X: u16 = 2;
/// Vertical padding between the frame and the content.
const DIALOG_PAD_Y: u16 = 1;
const DIALOG_MIN_WIDTH: u16 = 52;
const DIALOG_MAX_WIDTH: u16 = 84;

/// A startup dialog sized to what it says: as wide as its longest line (within
/// a readable measure) and exactly as tall as its content, centred in `area`.
/// Filling most of the screen with an empty frame made a two-line question look
/// like something had failed to load.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DialogLayout {
    frame: Rect,
    body: Rect,
    options: Rect,
    footer: Rect,
    body_rows: Vec<(BodyTone, String)>,
}

fn dialog_layout(
    area: Rect,
    body_lines: &[String],
    option_widths: impl Iterator<Item = usize>,
    option_count: usize,
    footer: &str,
) -> DialogLayout {
    let chrome_x = 2 + 2 * DIALOG_PAD_X;
    let body_width = body_lines
        .iter()
        .flat_map(|line| line.split('\n'))
        .map(|line| rebon_width::str_width(body_tone(line).1))
        .max()
        .unwrap_or(0);
    let options_width = option_widths.map(|w| w + 2).max().unwrap_or(0);
    let wanted = body_width
        .max(options_width)
        .max(rebon_width::str_width(footer))
        .min(u16::MAX as usize) as u16;
    let width = wanted
        .saturating_add(chrome_x)
        .clamp(DIALOG_MIN_WIDTH, DIALOG_MAX_WIDTH)
        .min(area.width);
    let text_width = width.saturating_sub(chrome_x) as usize;

    let mut body_rows = Vec::new();
    for line in body_lines {
        let (tone, text) = body_tone(line);
        for row in wrap_lines(text, text_width) {
            body_rows.push((tone, row));
        }
    }
    // Trailing blank rows would only pad the gap above the options.
    while body_rows.last().is_some_and(|(_, row)| row.is_empty()) {
        body_rows.pop();
    }

    let option_count = option_count.min(u16::MAX as usize) as u16;
    let body_height = body_rows.len() as u16;
    let gap = u16::from(body_height > 0);
    let footer_height = if footer.is_empty() { 0 } else { 2 };
    let content = body_height + gap + option_count + footer_height;
    let height = (content + 2 + 2 * DIALOG_PAD_Y).min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let frame = Rect::new(x, y, width, height);

    let inner = Rect::new(
        x + 1 + DIALOG_PAD_X,
        y + 1 + DIALOG_PAD_Y,
        width.saturating_sub(2 + 2 * DIALOG_PAD_X),
        height.saturating_sub(2 + 2 * DIALOG_PAD_Y),
    );
    // Options and the footer are what the user acts on, so when the terminal
    // is too short the body gives way first.
    let reserved = option_count + footer_height + gap;
    let body_height = body_height.min(inner.height.saturating_sub(reserved));
    let options_height = option_count.min(inner.height.saturating_sub(body_height + gap));
    let body = Rect::new(inner.x, inner.y, inner.width, body_height);
    let options = Rect::new(
        inner.x,
        inner.y + body_height + gap,
        inner.width,
        options_height,
    );
    let footer_y = options.bottom() + 1;
    let footer = if footer_height > 0 && footer_y < inner.bottom() {
        Rect::new(inner.x, footer_y, inner.width, 1)
    } else {
        Rect::new(inner.x, inner.bottom(), inner.width, 0)
    };
    DialogLayout {
        frame,
        body,
        options,
        footer,
        body_rows,
    }
}

fn render_dialog_frame(frame: &mut ratatui::Frame, rect: Rect, title: &str, styles: &DialogStyles) {
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .style(styles.background)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(styles.border)
        .title(Span::styled(format!(" {title} "), styles.title));
    frame.render_widget(block, rect);
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
    let styles = DialogStyles::for_color(color);
    let footer = footer.unwrap_or("↑/↓ to choose · Enter to confirm · Esc to cancel");
    let layout = dialog_layout(
        area,
        body_lines,
        options.iter().map(|o| rebon_width::str_width(o)),
        options.len(),
        footer,
    );
    if layout.frame.width < 4 || layout.frame.height < 3 {
        return;
    }
    render_dialog_frame(frame, layout.frame, title, &styles);
    render_body(frame, layout.body, &layout.body_rows, &styles);
    render_options(frame, layout.options, options, selected, &styles);
    render_footer(frame, layout.footer, footer, &styles);
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
    let styles = DialogStyles::for_color(color);
    let footer = footer.unwrap_or("Space to toggle · Enter to confirm · Esc to cancel");
    let layout = dialog_layout(
        area,
        body_lines,
        options.iter().map(|o| rebon_width::str_width(o) + 4),
        options.len(),
        footer,
    );
    if layout.frame.width < 4 || layout.frame.height < 3 {
        return;
    }
    render_dialog_frame(frame, layout.frame, title, &styles);
    render_body(frame, layout.body, &layout.body_rows, &styles);

    let rows = layout.options;
    for (idx, option) in options.iter().enumerate() {
        if idx as u16 >= rows.height {
            break;
        }
        let checked = selected_values.iter().any(|value| value == option);
        let is_focused = idx == focused;
        let pointer = if is_focused { "❯ " } else { "  " };
        let text_style = if is_focused {
            styles.focused
        } else {
            styles.normal
        };
        let (indicator, indicator_style) = if checked {
            ("[x]", styles.focused)
        } else {
            ("[ ]", styles.dim)
        };
        let row_area = Rect::new(rows.x, rows.y + idx as u16, rows.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(pointer, styles.focused),
                Span::styled(indicator, indicator_style),
                Span::styled(" ", styles.background),
                Span::styled(
                    truncate_to_width(option, rows.width.saturating_sub(6) as usize),
                    text_style,
                ),
            ])),
            row_area,
        );
    }

    render_footer(frame, layout.footer, footer, &styles);
}

fn render_body(
    frame: &mut ratatui::Frame,
    area: Rect,
    rows: &[(BodyTone, String)],
    styles: &DialogStyles,
) {
    for (offset, (tone, row)) in rows.iter().enumerate().take(area.height as usize) {
        let style = match tone {
            BodyTone::Headline => styles.headline,
            BodyTone::Normal => styles.normal,
            BodyTone::Dim => styles.dim,
        };
        let row_area = Rect::new(area.x, area.y + offset as u16, area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(row.clone(), style))),
            row_area,
        );
    }
}

fn render_options(
    frame: &mut ratatui::Frame,
    area: Rect,
    options: &[String],
    selected: usize,
    styles: &DialogStyles,
) {
    for (idx, option) in options.iter().enumerate() {
        if idx as u16 >= area.height {
            break;
        }
        let is_selected = idx == selected;
        let (prefix, style) = if is_selected {
            ("❯ ", styles.focused)
        } else {
            ("  ", styles.normal)
        };
        let row_area = Rect::new(area.x, area.y + idx as u16, area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix, styles.focused),
                Span::styled(
                    truncate_to_width(option, area.width.saturating_sub(2) as usize),
                    style,
                ),
            ])),
            row_area,
        );
    }
}

/// Render a `key action · key action` hint with the keys in the body tone and
/// the rest receding, so the controls can be read at a glance.
fn render_footer(frame: &mut ratatui::Frame, area: Rect, footer: &str, styles: &DialogStyles) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let footer = truncate_to_width(footer, area.width as usize);
    let mut spans = Vec::new();
    for (idx, part) in footer.split(" · ").enumerate() {
        if idx > 0 {
            spans.push(Span::styled(" · ", styles.dim));
        }
        match part.split_once(" to ") {
            Some((key, action)) if !key.is_empty() && key.len() <= 12 => {
                spans.push(Span::styled(key.to_string(), styles.key));
                spans.push(Span::styled(format!(" to {action}"), styles.dim));
            }
            _ => spans.push(Span::styled(part.to_string(), styles.dim)),
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
            // A word wider than the row (a long path, a URL) is broken at
            // the column instead of being clipped by the frame.
            for ch in word.chars() {
                let ch_width = rebon_width::terminal_char_width(ch);
                if used + ch_width > width && !current.is_empty() {
                    rows.push(std::mem::take(&mut current));
                    used = 0;
                }
                current.push(ch);
                used += ch_width;
            }
        }
        if !current.is_empty() {
            rows.push(current);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const LONG_CWD: &str = r"C:\Users\bon\AppData\Local\Temp\claude\F--dev-sandbox-v2-reboncode\e45fe615-eddb-40f8-abea-45a2c99c150d\scratchpad\demo";

    fn trust_body(cwd: &str) -> Vec<String> {
        vec![
            format!("> You are in {cwd}"),
            String::new(),
            "Do you trust the contents of this directory?".to_string(),
            "Working with untrusted contents comes with higher risk of prompt injection."
                .to_string(),
        ]
    }

    fn trust_options() -> Vec<String> {
        vec![
            "Yes, I trust this folder".to_string(),
            "No, exit".to_string(),
        ]
    }

    fn layout_for(area: Rect, body: &[String], options: &[String], footer: &str) -> DialogLayout {
        dialog_layout(
            area,
            body,
            options.iter().map(|o| rebon_width::str_width(o)),
            options.len(),
            footer,
        )
    }

    fn render_rows(width: u16, height: u16, body: &[String], selected: usize) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                render_dialog(
                    frame,
                    frame.area(),
                    "Trust Directory",
                    DialogColor::Warning,
                    body,
                    &trust_options(),
                    None,
                    selected,
                )
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn body_tone_reads_the_line_itself() {
        assert_eq!(
            body_tone(r"> You are in D:\x"),
            (BodyTone::Headline, r"You are in D:\x")
        );
        assert_eq!(
            body_tone("Do you trust the contents of this directory?").0,
            BodyTone::Normal
        );
        assert_eq!(
            body_tone("2 invalid settings file(s) detected:").0,
            BodyTone::Normal
        );
        assert_eq!(body_tone("Choose an option:").0, BodyTone::Normal);
        assert_eq!(body_tone("Reset writes a safe default.").0, BodyTone::Dim);
        assert_eq!(body_tone("").0, BodyTone::Dim);
    }

    #[test]
    fn dialog_is_sized_to_its_content_and_centred() {
        let area = Rect::new(0, 0, 120, 40);
        let footer = "↑/↓ to choose · Enter to confirm · Esc to cancel";
        let layout = layout_for(
            area,
            &trust_body(r"D:\own\MyVault"),
            &trust_options(),
            footer,
        );

        // 4 body rows, a gap, 2 options, a gap and the footer, inside a
        // 1-row pad and the frame.
        assert_eq!(layout.body_rows.len(), 4);
        assert_eq!(layout.frame.height, 2 + 2 * DIALOG_PAD_Y + 4 + 1 + 2 + 2);
        // Wide enough for the longest line, and no wider.
        let longest = rebon_width::str_width(&trust_body("")[3]) as u16;
        assert_eq!(layout.frame.width, longest + 2 + 2 * DIALOG_PAD_X);
        assert_eq!(layout.frame.x, (120 - layout.frame.width) / 2);
        assert_eq!(layout.frame.y, (40 - layout.frame.height) / 2);
        assert_eq!(layout.options.height, 2);
        assert_eq!(layout.footer.height, 1);
        assert_eq!(layout.footer.y, layout.options.bottom() + 1);
    }

    #[test]
    fn dialog_width_stays_within_the_reading_measure() {
        let area = Rect::new(0, 0, 200, 40);
        let short = layout_for(area, &["> Hi".to_string()], &["Ok".to_string()], "");
        assert_eq!(short.frame.width, DIALOG_MIN_WIDTH);
        let long = layout_for(area, &trust_body(LONG_CWD), &trust_options(), "");
        assert_eq!(long.frame.width, DIALOG_MAX_WIDTH);
        let narrow = layout_for(
            Rect::new(0, 0, 40, 40),
            &trust_body(LONG_CWD),
            &trust_options(),
            "",
        );
        assert_eq!(narrow.frame.width, 40);
    }

    #[test]
    fn a_long_directory_wraps_instead_of_being_clipped() {
        let layout = layout_for(
            Rect::new(0, 0, 120, 40),
            &trust_body(LONG_CWD),
            &trust_options(),
            "",
        );
        let text_width = layout.body.width as usize;
        let headline: String = layout
            .body_rows
            .iter()
            .filter(|(tone, _)| *tone == BodyTone::Headline)
            .map(|(_, row)| row.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            headline
                .replace(' ', "")
                .contains(&LONG_CWD.replace(' ', "")),
            "{headline}"
        );
        for (_, row) in &layout.body_rows {
            assert!(
                rebon_width::str_width(row) <= text_width,
                "{row:?} > {text_width}"
            );
        }
    }

    #[test]
    fn a_short_terminal_squeezes_the_body_before_the_options() {
        let area = Rect::new(0, 0, 80, 9);
        let footer = "Enter to confirm";
        let layout = layout_for(area, &trust_body(LONG_CWD), &trust_options(), footer);
        assert_eq!(layout.frame.height, 9);
        assert_eq!(layout.options.height, 2);
        assert!(layout.body.height < layout.body_rows.len() as u16);
        assert!(layout.options.bottom() <= layout.frame.bottom() - 1 - DIALOG_PAD_Y);
    }

    #[test]
    fn wrap_lines_breaks_words_wider_than_the_row() {
        assert_eq!(wrap_lines("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(
            wrap_lines(r"go to C:\abcdef", 6),
            vec!["go to", r"C:\abc", "def"]
        );
        assert_eq!(wrap_lines("你好世界", 5), vec!["你好", "世界"]);
        assert_eq!(wrap_lines("a\n\nb", 4), vec!["a", "", "b"]);
        assert!(wrap_lines("anything", 0).is_empty());
    }

    #[test]
    fn trust_dialog_renders_a_rounded_frame_with_a_pointer_and_key_hints() {
        let rows = render_rows(100, 24, &trust_body(r"D:\own\MyVault"), 0);
        let text = rows.join("\n");
        assert!(
            rows.iter().any(|row| row.contains("╭ Trust Directory ─")),
            "{text}"
        );
        assert!(rows.iter().any(|row| row.contains('╰')), "{text}");
        assert!(text.contains(r"You are in D:\own\MyVault"), "{text}");
        assert!(!text.contains("> You are in"), "{text}");
        assert!(text.contains("❯ Yes, I trust this folder"), "{text}");
        assert!(text.contains("  No, exit"), "{text}");
        assert!(text.contains("Enter to confirm"), "{text}");
        // Content-sized: the frame does not reach the top or bottom rows.
        assert!(!rows[0].contains('╭') && !rows[23].contains('╰'), "{text}");

        let second = render_rows(100, 24, &trust_body(r"D:\own\MyVault"), 1).join("\n");
        assert!(second.contains("❯ No, exit"), "{second}");
    }

    #[test]
    fn dialog_renders_nothing_in_a_degenerate_area() {
        let rows = render_rows(3, 2, &trust_body(r"D:\x"), 0);
        assert!(rows.iter().all(|row| row.trim().is_empty()), "{rows:?}");
    }
}
