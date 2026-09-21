//! TUI host for `rebon-dialog::global_search`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use rebon_customselect::{
    NavigationAction, NavigationProps, NavigationState, OptionWithDescription,
};
use rebon_design_system::theme::get_active_theme;
use rebon_dialog::global_search::{self, GlobalSearchAction, GlobalSearchMatch};
use rebon_tui::parse_theme_color;

use crate::tui::dialog_support::{read_preview_lines, truncate_to_width};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Preview {
    file: String,
    line: u64,
    start_line: usize,
    lines: Vec<String>,
}

/// Search request the runner should execute in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub generation: u64,
    pub query: String,
}

/// Cloneable render + reducer state for the global-search dialog.
#[derive(Debug, Clone)]
pub struct GlobalSearchDialogState {
    pub query: String,
    pub is_searching: bool,
    search_error: Option<String>,
    matches: Vec<GlobalSearchMatch>,
    truncated: bool,
    nav: Option<NavigationState<String>>,
    preview: Option<Preview>,
    generation: u64,
    last_query_change: Instant,
    pending_search: bool,
}

/// Result of handling a key inside the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalSearchDialogOutcome {
    None,
    Apply(GlobalSearchAction),
}

impl GlobalSearchDialogState {
    pub fn open() -> Self {
        Self {
            query: String::new(),
            is_searching: false,
            search_error: None,
            matches: Vec::new(),
            truncated: false,
            nav: None,
            preview: None,
            generation: 0,
            last_query_change: Instant::now(),
            pending_search: false,
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> GlobalSearchDialogOutcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return GlobalSearchDialogOutcome::None;
        }
        match key.code {
            KeyCode::Esc => GlobalSearchDialogOutcome::Apply(global_search::handle_cancel()),
            KeyCode::Enter => self
                .selected_match()
                .map(|m| GlobalSearchDialogOutcome::Apply(global_search::handle_select(m)))
                .unwrap_or(GlobalSearchDialogOutcome::None),
            KeyCode::Tab => self
                .selected_match()
                .map(|m| GlobalSearchDialogOutcome::Apply(global_search::handle_insert(m, true)))
                .unwrap_or(GlobalSearchDialogOutcome::None),
            KeyCode::BackTab => self
                .selected_match()
                .map(|m| GlobalSearchDialogOutcome::Apply(global_search::handle_insert(m, false)))
                .unwrap_or(GlobalSearchDialogOutcome::None),
            KeyCode::Up => {
                rebon_dialog::model::dispatch_nav(
                    &mut self.nav,
                    NavigationAction::FocusPreviousOption,
                );
                GlobalSearchDialogOutcome::None
            }
            KeyCode::Down => {
                rebon_dialog::model::dispatch_nav(&mut self.nav, NavigationAction::FocusNextOption);
                GlobalSearchDialogOutcome::None
            }
            KeyCode::PageUp => {
                rebon_dialog::model::dispatch_nav(
                    &mut self.nav,
                    NavigationAction::FocusPreviousPage,
                );
                GlobalSearchDialogOutcome::None
            }
            KeyCode::PageDown => {
                rebon_dialog::model::dispatch_nav(&mut self.nav, NavigationAction::FocusNextPage);
                GlobalSearchDialogOutcome::None
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.note_query_change();
                GlobalSearchDialogOutcome::None
            }
            KeyCode::Delete => {
                self.query.clear();
                self.note_query_change();
                GlobalSearchDialogOutcome::None
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
            {
                self.query.push(ch);
                self.note_query_change();
                GlobalSearchDialogOutcome::None
            }
            _ => GlobalSearchDialogOutcome::None,
        }
    }

    pub fn maybe_take_search_request(&mut self) -> Option<SearchRequest> {
        if !self.pending_search {
            return None;
        }
        if self.query.trim().is_empty() {
            self.pending_search = false;
            self.is_searching = false;
            return None;
        }
        if self.last_query_change.elapsed().as_millis() < global_search::DEBOUNCE_MS as u128 {
            return None;
        }
        self.pending_search = false;
        Some(SearchRequest {
            generation: self.generation,
            query: self.query.clone(),
        })
    }

    pub fn apply_search_results(
        &mut self,
        generation: u64,
        matches: Vec<GlobalSearchMatch>,
        truncated: bool,
        cwd: &Path,
    ) {
        if generation != self.generation {
            return;
        }
        self.matches = matches;
        self.search_error = None;
        self.truncated = truncated;
        self.is_searching = false;
        let focus = self.selected_match().map(global_search::match_key);
        self.reset_nav(focus);
        self.sync_preview(cwd);
    }

    pub fn apply_search_failure(&mut self, generation: u64, reason: impl Into<String>) {
        if generation != self.generation {
            return;
        }
        self.matches.clear();
        self.search_error = Some(reason.into());
        self.truncated = false;
        self.nav = None;
        self.preview = None;
        self.is_searching = false;
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let ds = get_active_theme();
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
            .title(Span::styled(
                format!(" {} ", global_search::TITLE),
                title_style,
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(4),
                Constraint::Length(1),
            ])
            .split(inner);

        let query_line = if self.query.is_empty() {
            Line::from(vec![
                Span::styled(" Search: ", dim),
                Span::styled(global_search::PLACEHOLDER, dim),
            ])
        } else {
            Line::from(vec![
                Span::styled(" Search: ", dim),
                Span::styled(self.query.clone(), normal),
            ])
        };
        frame.render_widget(Paragraph::new(query_line), sections[0]);

        let (_, max_path_width, max_text_width, preview_width) =
            global_search::compute_layout(inner.width);
        let (list_area, preview_area) = if global_search::preview_on_right(inner.width) {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(50),
                    Constraint::Length(1),
                    Constraint::Percentage(50),
                ])
                .split(sections[1]);
            (cols[0], cols[2])
        } else {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(
                        global_search::compute_visible_rows(sections[1].height) as u16 + 2,
                    ),
                    Constraint::Length(1),
                    Constraint::Min(4),
                ])
                .split(sections[1]);
            (rows[0], rows[2])
        };

        self.render_list(
            frame,
            list_area,
            max_path_width,
            max_text_width,
            selected,
            normal,
            dim,
        );
        self.render_preview(frame, preview_area, preview_width, normal, dim);

        let footer = Line::from(vec![
            Span::styled(" Enter ", selected),
            Span::styled(global_search::SELECT_ACTION, dim),
            Span::styled(" · Tab ", selected),
            Span::styled("mention", dim),
            Span::styled(" · shift+tab ", selected),
            Span::styled("insert path", dim),
            Span::styled(" · Esc ", selected),
            Span::styled("close", dim),
        ]);
        frame.render_widget(Paragraph::new(footer), sections[2]);
    }

    pub fn sync_preview(&mut self, cwd: &Path) {
        let Some(m) = self.selected_match().cloned() else {
            self.preview = None;
            return;
        };
        let full_path = if Path::new(&m.file).is_absolute() {
            PathBuf::from(&m.file)
        } else {
            cwd.join(&m.file)
        };
        let start_line = m
            .line
            .saturating_sub(global_search::PREVIEW_CONTEXT_LINES as u64 + 1)
            as usize;
        let lines = read_preview_lines(
            &full_path,
            start_line,
            global_search::PREVIEW_CONTEXT_LINES * 2 + 1,
        );
        self.preview = Some(Preview {
            file: m.file,
            line: m.line,
            start_line,
            lines,
        });
    }

    fn note_query_change(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.last_query_change = Instant::now();
        self.pending_search = !self.query.trim().is_empty();
        self.is_searching = self.pending_search;
        self.search_error = None;
        self.matches.clear();
        self.truncated = false;
        self.nav = None;
        self.preview = None;
    }

    fn selected_match(&self) -> Option<&GlobalSearchMatch> {
        let key = self
            .nav
            .as_ref()
            .and_then(NavigationState::validated_focused_value)?;
        self.matches
            .iter()
            .find(|m| global_search::match_key(m) == key)
    }

    fn reset_nav(&mut self, focus: Option<String>) {
        if self.matches.is_empty() {
            self.nav = None;
            return;
        }
        let options = self
            .matches
            .iter()
            .map(|m| {
                OptionWithDescription::text(
                    global_search::match_key(m),
                    global_search::match_key(m),
                )
            })
            .collect();
        self.nav = Some(NavigationState::new(NavigationProps {
            visible_option_count: Some(global_search::VISIBLE_RESULTS),
            options,
            initial_focus_value: None,
            focus_value: focus,
        }));
    }

    fn render_list(
        &self,
        frame: &mut Frame,
        area: Rect,
        max_path_width: usize,
        max_text_width: usize,
        selected: Style,
        normal: Style,
        dim: Style,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(dim)
            .title(Span::styled(
                global_search::match_label(self.matches.len(), self.truncated, self.is_searching),
                dim,
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.matches.is_empty() {
            let message = self
                .search_error
                .as_deref()
                .unwrap_or_else(|| global_search::empty_message(&self.query, self.is_searching));
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate_to_width(message, inner.width as usize),
                    dim,
                ))),
                inner,
            );
            return;
        }

        let focused = self
            .nav
            .as_ref()
            .and_then(NavigationState::validated_focused_value);
        let rows = self
            .nav
            .as_ref()
            .map(NavigationState::visible_options)
            .unwrap_or_default();
        for (idx, row) in rows.into_iter().enumerate() {
            if idx as u16 >= inner.height {
                break;
            }
            let key = row.option.value().clone();
            let Some(m) = self
                .matches
                .iter()
                .find(|item| global_search::match_key(item) == key)
            else {
                continue;
            };
            let is_focused = focused.as_ref() == Some(&key);
            let style = if is_focused { selected } else { normal };
            let prefix = if is_focused { ">" } else { " " };
            let label = format!(
                "{}:{} {}",
                truncate_to_width(&m.file, max_path_width),
                m.line,
                truncate_to_width(m.text.trim_start(), max_text_width),
            );
            let line = Line::from(vec![
                Span::styled(format!("{prefix} "), style),
                Span::styled(
                    truncate_to_width(&label, inner.width.saturating_sub(2) as usize),
                    style,
                ),
            ]);
            let row_area = Rect::new(inner.x, inner.y + idx as u16, inner.width, 1);
            frame.render_widget(Paragraph::new(line), row_area);
        }
    }

    fn render_preview(
        &self,
        frame: &mut Frame,
        area: Rect,
        preview_width: usize,
        normal: Style,
        dim: Style,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let block = Block::default().borders(Borders::ALL).border_style(dim);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(preview) = &self.preview else {
            let message = if self.is_searching {
                "Searching..."
            } else {
                "(preview unavailable)"
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(message, dim))),
                inner,
            );
            return;
        };

        let mut y = inner.y;
        if y < inner.bottom() {
            let header = Rect::new(inner.x, y, inner.width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate_to_width(&format!("{}:{}", preview.file, preview.line), preview_width),
                    dim,
                ))),
                header,
            );
            y += 1;
        }
        for (idx, line) in preview.lines.iter().enumerate() {
            if y >= inner.bottom() {
                break;
            }
            let absolute_line = preview.start_line + idx + 1;
            let marker = if absolute_line as u64 == preview.line {
                ">"
            } else {
                " "
            };
            let rendered = format!(
                "{marker}{absolute_line:>4} {}",
                truncate_to_width(line, preview_width.saturating_sub(6)),
            );
            let row = Rect::new(inner.x, y, inner.width, 1);
            let style = if absolute_line as u64 == preview.line {
                normal.add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(rendered, style))),
                row,
            );
            y += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn typing_marks_search_pending() {
        let mut state = GlobalSearchDialogState::open();
        state.handle_key(&key(KeyCode::Char('f')));
        assert_eq!(state.query, "f");
        assert!(state.is_searching);
        assert!(state.pending_search);
    }

    #[test]
    fn applying_results_focuses_first_match() {
        let mut state = GlobalSearchDialogState::open();
        state.handle_key(&key(KeyCode::Char('f')));
        state.apply_search_results(
            state.generation,
            vec![GlobalSearchMatch {
                file: "src/main.rs".into(),
                line: 7,
                text: "fn foo()".into(),
            }],
            false,
            Path::new("."),
        );
        assert_eq!(state.selected_match().unwrap().file, "src/main.rs");
        assert!(state.search_error.is_none());
    }

    #[test]
    fn applying_failure_preserves_actionable_message() {
        let mut state = GlobalSearchDialogState::open();
        state.handle_key(&key(KeyCode::Char('f')));
        state.apply_search_failure(
            state.generation,
            "Search unavailable: install ripgrep or set REBON_RIPGREP_PATH.",
        );

        assert!(!state.is_searching);
        assert_eq!(
            state.search_error.as_deref(),
            Some("Search unavailable: install ripgrep or set REBON_RIPGREP_PATH.")
        );
        assert!(state.matches.is_empty());
    }
}
