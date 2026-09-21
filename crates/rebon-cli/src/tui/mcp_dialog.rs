//! Read-only browser for configured MCP servers and loaded tools.
//!
//! Drawn over an [`McpStatusSnapshot`] either way: the host's own, or the
//! one a mirrored session's owner last sent.

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use rebon_design_system::theme::get_active_theme;
use rebon_session_host::McpStatusSnapshot;
use rebon_tui::parse_theme_color;

use crate::tui::wiring::TuiEngineSession;

const TITLE: &str = "MCP";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpDialogOutcome {
    None,
    Close,
    OpenConfig(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerRow {
    name: String,
    transport: String,
    source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolRow {
    name: String,
    tokens: usize,
}

#[derive(Debug, Clone)]
pub struct McpDialogState {
    servers: Vec<ServerRow>,
    tools: Vec<ToolRow>,
    load_status: String,
    warnings: Vec<String>,
    warnings_expanded: bool,
    selected: usize,
    default_config_path: PathBuf,
}

impl McpDialogState {
    pub fn open(session: &TuiEngineSession) -> Self {
        let snapshot = crate::mcp_status::owner_mcp_snapshot(session)
            .unwrap_or_else(crate::mcp_status::not_hosted_here);
        Self::from_snapshot(&snapshot, PathBuf::from(&session.cwd))
    }

    /// The browser over what the session's owner reports, for a terminal
    /// that mirrors a session rather than hosting it.
    pub fn from_owner_snapshot(snapshot: &McpStatusSnapshot, cwd: &Path) -> Self {
        Self::from_snapshot(snapshot, cwd.to_path_buf())
    }

    fn from_snapshot(snapshot: &McpStatusSnapshot, cwd: PathBuf) -> Self {
        // A failed load is the first thing to read under "Warnings".
        let mut warnings: Vec<String> = snapshot.error.iter().cloned().collect();
        warnings.extend(snapshot.warnings.iter().cloned());
        Self {
            servers: snapshot
                .servers
                .iter()
                .map(|server| ServerRow {
                    name: server.name.clone(),
                    transport: server.transport.clone(),
                    source: server.source.clone(),
                })
                .collect(),
            tools: snapshot
                .tools
                .iter()
                .map(|tool| ToolRow {
                    name: tool.name.clone(),
                    tokens: tool.tokens as usize,
                })
                .collect(),
            load_status: snapshot.loader.clone(),
            warnings,
            warnings_expanded: false,
            selected: 0,
            default_config_path: cwd.join(".mcp.json"),
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> McpDialogOutcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return McpDialogOutcome::None;
        }
        match key.code {
            KeyCode::Esc => McpDialogOutcome::Close,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                McpDialogOutcome::None
            }
            KeyCode::Down => {
                self.selected = self.selected.saturating_add(1).min(self.row_count() - 1);
                McpDialogOutcome::None
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(8);
                McpDialogOutcome::None
            }
            KeyCode::PageDown => {
                self.selected = self.selected.saturating_add(8).min(self.row_count() - 1);
                McpDialogOutcome::None
            }
            KeyCode::Home => {
                self.selected = 0;
                McpDialogOutcome::None
            }
            KeyCode::End => {
                self.selected = self.row_count() - 1;
                McpDialogOutcome::None
            }
            KeyCode::Char('w') if is_plain_char(key) => {
                self.warnings_expanded = !self.warnings_expanded;
                McpDialogOutcome::None
            }
            KeyCode::Enter if self.selected < self.servers.len() => {
                McpDialogOutcome::OpenConfig(self.config_path_for_server(self.selected))
            }
            KeyCode::Enter if self.selected + 1 == self.row_count() => {
                McpDialogOutcome::OpenConfig(self.default_config_path.clone())
            }
            _ => McpDialogOutcome::None,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let theme = get_active_theme();
        let normal = Style::default().fg(parse_theme_color(theme.text));
        let dim = Style::default().fg(parse_theme_color(theme.inactive));
        let selected = normal.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        let border = Style::default().fg(parse_theme_color(theme.subtle));

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(Span::styled(
                format!(" {TITLE} · {} ", self.load_status),
                normal.add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        let rows = self.render_rows(normal, dim, selected);
        let offset = self.scroll_offset(sections[0].height as usize);
        frame.render_widget(Paragraph::new(rows).scroll((offset as u16, 0)), sections[0]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Up/Down select · w warnings · Enter open config · Esc close",
                dim,
            ))),
            sections[1],
        );
    }

    fn render_rows(&self, normal: Style, dim: Style, selected: Style) -> Vec<Line<'static>> {
        let mut rows = vec![Line::styled(
            format!("Servers ({})", self.servers.len()),
            normal.add_modifier(Modifier::BOLD),
        )];
        if self.servers.is_empty() {
            rows.push(Line::styled("  none configured", dim));
        }
        for (index, server) in self.servers.iter().enumerate() {
            rows.push(Line::styled(
                format!(
                    "{} {} [{}]",
                    marker(self.selected == index),
                    server.name,
                    server.transport
                ),
                if self.selected == index {
                    selected
                } else {
                    normal
                },
            ));
            rows.push(Line::styled(format!("    source: {}", server.source), dim));
        }
        rows.push(Line::styled(
            format!(
                "Warnings ({}) [{}]",
                self.warnings.len(),
                if self.warnings_expanded {
                    "expanded"
                } else {
                    "collapsed"
                }
            ),
            normal.add_modifier(Modifier::BOLD),
        ));
        if self.warnings_expanded {
            if self.warnings.is_empty() {
                rows.push(Line::styled("  none", dim));
            } else {
                rows.extend(
                    self.warnings
                        .iter()
                        .map(|warning| Line::styled(format!("  - {warning}"), dim)),
                );
            }
        }
        rows.push(Line::styled(
            format!("Loaded tools ({})", self.tools.len()),
            normal.add_modifier(Modifier::BOLD),
        ));
        for (tool_offset, tool) in self.tools.iter().enumerate() {
            let index = self.servers.len() + tool_offset;
            rows.push(Line::styled(
                format!(
                    "{} {} (~{} tokens)",
                    marker(self.selected == index),
                    tool.name,
                    tool.tokens
                ),
                if self.selected == index {
                    selected
                } else {
                    normal
                },
            ));
        }
        let action_index = self.row_count() - 1;
        rows.push(Line::styled(
            format!(
                "{} Open MCP config file",
                marker(self.selected == action_index)
            ),
            if self.selected == action_index {
                selected
            } else {
                normal
            },
        ));
        rows
    }

    fn row_count(&self) -> usize {
        self.servers.len() + self.tools.len() + 1
    }

    /// Position of a selectable entry inside [`Self::render_rows`]. Selection
    /// indices only count servers, tools and the trailing action, while the
    /// rendered list also carries section headings, a `source:` line per
    /// server and the expanded warnings, so the two are not interchangeable.
    fn rendered_row(&self, selection: usize) -> usize {
        let servers_heading = 1 + usize::from(self.servers.is_empty());
        if selection < self.servers.len() {
            return servers_heading + selection * 2;
        }
        let warnings_block = 1 + if self.warnings_expanded {
            self.warnings.len().max(1)
        } else {
            0
        };
        let tools_heading = 1;
        servers_heading
            + self.servers.len() * 2
            + warnings_block
            + tools_heading
            + (selection - self.servers.len())
    }

    fn scroll_offset(&self, viewport: usize) -> usize {
        self.rendered_row(self.selected)
            .saturating_sub(viewport.saturating_sub(1))
    }

    fn config_path_for_server(&self, server_index: usize) -> PathBuf {
        self.servers
            .get(server_index)
            .map(|server| PathBuf::from(&server.source))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| self.default_config_path.clone())
    }
}

/// Letter shortcuts must ignore control chords so Ctrl+C is not read as `c`
/// while the dialog owns the keyboard.
fn is_plain_char(key: &KeyEvent) -> bool {
    !key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT)
}

fn marker(selected: bool) -> &'static str {
    if selected {
        ">"
    } else {
        " "
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::Terminal;

    fn rendered_text(dialog: &McpDialogState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| dialog.render(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn state() -> McpDialogState {
        McpDialogState {
            servers: vec![ServerRow {
                name: "local".into(),
                transport: "stdio".into(),
                source: "env".into(),
            }],
            tools: vec![ToolRow {
                name: "mcp__local__read".into(),
                tokens: 12,
            }],
            load_status: "ready".into(),
            warnings: vec!["slow handshake".into()],
            warnings_expanded: false,
            selected: 0,
            default_config_path: PathBuf::from(".mcp.json"),
        }
    }

    #[test]
    fn navigation_clamps_to_selectable_rows() {
        let mut dialog = state();
        dialog.handle_key(&KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(dialog.selected, 2);
        dialog.handle_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(dialog.selected, 2);
    }

    #[test]
    fn warning_toggle_is_read_only() {
        let mut dialog = state();
        assert_eq!(
            dialog.handle_key(&KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
            McpDialogOutcome::None
        );
        assert!(dialog.warnings_expanded);
    }

    #[test]
    fn enter_opens_server_source_or_default_action() {
        let mut dialog = state();
        assert_eq!(
            dialog.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            McpDialogOutcome::OpenConfig(PathBuf::from(".mcp.json"))
        );
        dialog.selected = dialog.row_count() - 1;
        assert_eq!(
            dialog.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            McpDialogOutcome::OpenConfig(PathBuf::from(".mcp.json"))
        );
    }

    #[test]
    fn control_chords_do_not_trigger_letter_shortcuts() {
        let mut dialog = state();
        dialog.handle_key(&KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(!dialog.warnings_expanded);
    }

    #[test]
    fn last_row_scrolls_into_view_past_headings_and_expanded_warnings() {
        let mut dialog = state();
        dialog.servers = (0..3)
            .map(|index| ServerRow {
                name: format!("server-{index}"),
                transport: "stdio".into(),
                source: format!("source-{index}"),
            })
            .collect();
        dialog.tools = (0..2)
            .map(|index| ToolRow {
                name: format!("mcp__local__tool-{index}"),
                tokens: 10,
            })
            .collect();
        dialog.warnings = vec!["first".into(), "second".into()];
        dialog.warnings_expanded = true;
        dialog.handle_key(&KeyEvent::new(KeyCode::End, KeyModifiers::NONE));

        assert!(rendered_text(&dialog, 60, 8).contains("Open MCP config file"));
    }

    #[test]
    fn escape_closes() {
        assert_eq!(
            state().handle_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            McpDialogOutcome::Close
        );
    }
}
