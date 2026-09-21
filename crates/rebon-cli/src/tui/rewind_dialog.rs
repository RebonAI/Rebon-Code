use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use rebon_design_system::theme::get_active_theme;
use rebon_picker::message_selector::{
    build_restore_code_confirmation, build_restore_options, first_visible_index, handle_escape,
    handle_restore_option, handle_select_current, is_summarize_option,
    project_user_message_display, restore_option_conversation_text, DiffStats,
    MessageSelectorAction, MessageSelectorScreen, MessageSelectorState, RestoreCodeConfirmation,
    RestoreOption, UserMessageDisplay,
};
use rebon_tui::parse_theme_color;

use crate::tui::dialog_support::truncate_to_width;

const TITLE: &str = "Rewind";
const INCLUDE_SUMMARIZE_UP_TO: bool =
    rebon_picker::message_selector::INCLUDE_SUMMARIZE_UP_TO_FOR_ANT;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindMessageOption {
    pub label: String,
    pub raw_text: String,
    pub diff_stats: Option<DiffStats>,
    pub file_restore: rebon_session::FileRestoreCapability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewindDialogOutcome {
    None,
    Close,
    Restore {
        selected_index: usize,
        option: RestoreOption,
        feedback: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct RewindDialogState {
    pub selector: MessageSelectorState,
    pub options: Vec<RewindMessageOption>,
    pub selected_message_index: usize,
    pub selected_restore_index: usize,
    pub summarize_from_feedback: String,
    pub summarize_up_to_feedback: String,
    pub restore_options: Vec<RestoreOption>,
}

impl RewindDialogState {
    pub fn open(options: Vec<RewindMessageOption>) -> Self {
        let total_options = options.len().max(1);
        let mut this = Self {
            selector: MessageSelectorState::new(total_options, false),
            options,
            selected_message_index: total_options.saturating_sub(1),
            selected_restore_index: 0,
            summarize_from_feedback: String::new(),
            summarize_up_to_feedback: String::new(),
            restore_options: Vec::new(),
        };
        this.sync_restore_options();
        this
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> RewindDialogOutcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return RewindDialogOutcome::None;
        }

        match &self.selector.screen {
            MessageSelectorScreen::PickList => self.handle_pick_list_key(key),
            MessageSelectorScreen::Confirm => self.handle_confirm_key(key),
            MessageSelectorScreen::Error(_) => match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.selector.screen = MessageSelectorScreen::PickList;
                    RewindDialogOutcome::None
                }
                _ => RewindDialogOutcome::None,
            },
        }
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
            .title(Span::styled(format!(" {TITLE} "), title_style));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        match &self.selector.screen {
            MessageSelectorScreen::PickList => {
                self.render_pick_list(frame, inner, selected, normal, dim)
            }
            MessageSelectorScreen::Confirm => {
                self.render_confirm(frame, inner, selected, normal, dim)
            }
            MessageSelectorScreen::Error(message) => {
                self.render_error(frame, inner, selected, normal, dim, message)
            }
        }
    }

    pub fn selected_diff_stats(&self) -> Option<&DiffStats> {
        self.options
            .get(self.selected_message_index)
            .and_then(|option| option.diff_stats.as_ref())
    }

    fn sync_restore_options(&mut self) {
        let can_restore_code =
            self.options
                .get(self.selected_message_index)
                .is_some_and(|option| {
                    matches!(
                        option.file_restore,
                        rebon_session::FileRestoreCapability::Clean(_)
                    )
                });
        self.restore_options = build_restore_options(can_restore_code, INCLUDE_SUMMARIZE_UP_TO);
        if self.selected_restore_index >= self.restore_options.len() {
            self.selected_restore_index = 0;
        }
    }

    fn handle_pick_list_key(&mut self, key: &KeyEvent) -> RewindDialogOutcome {
        match key.code {
            KeyCode::Esc => match handle_escape(&self.selector) {
                MessageSelectorAction::Close => RewindDialogOutcome::Close,
                _ => RewindDialogOutcome::None,
            },
            KeyCode::Up => {
                self.selector.move_up();
                self.selected_message_index = self.selector.selected_index;
                self.sync_restore_options();
                RewindDialogOutcome::None
            }
            KeyCode::Down => {
                self.selector.move_down();
                self.selected_message_index = self.selector.selected_index;
                self.sync_restore_options();
                RewindDialogOutcome::None
            }
            KeyCode::Home => {
                self.selector.jump_to_top();
                self.selected_message_index = self.selector.selected_index;
                self.sync_restore_options();
                RewindDialogOutcome::None
            }
            KeyCode::End => {
                self.selector.jump_to_bottom();
                self.selected_message_index = self.selector.selected_index;
                self.sync_restore_options();
                RewindDialogOutcome::None
            }
            KeyCode::Enter => match handle_select_current(self.selector.selected_index, true) {
                MessageSelectorAction::OpenConfirm { message_index } => {
                    self.selected_message_index = message_index;
                    self.selector.selected_index = message_index;
                    self.selector.screen = MessageSelectorScreen::Confirm;
                    self.sync_restore_options();
                    self.selected_restore_index = 0;
                    RewindDialogOutcome::None
                }
                MessageSelectorAction::Restore { option, feedback } => {
                    RewindDialogOutcome::Restore {
                        selected_index: self.selector.selected_index,
                        option,
                        feedback,
                    }
                }
                _ => RewindDialogOutcome::None,
            },
            _ => RewindDialogOutcome::None,
        }
    }

    fn handle_confirm_key(&mut self, key: &KeyEvent) -> RewindDialogOutcome {
        match key.code {
            KeyCode::Esc => match handle_escape(&self.selector) {
                MessageSelectorAction::DismissConfirm => {
                    self.selector.screen = MessageSelectorScreen::PickList;
                    RewindDialogOutcome::None
                }
                MessageSelectorAction::Close => RewindDialogOutcome::Close,
                _ => RewindDialogOutcome::None,
            },
            KeyCode::Up => {
                self.selected_restore_index = self.selected_restore_index.saturating_sub(1);
                RewindDialogOutcome::None
            }
            KeyCode::Down => {
                if self.selected_restore_index + 1 < self.restore_options.len() {
                    self.selected_restore_index += 1;
                }
                RewindDialogOutcome::None
            }
            KeyCode::Enter => {
                let Some(option) = self
                    .restore_options
                    .get(self.selected_restore_index)
                    .copied()
                else {
                    return RewindDialogOutcome::None;
                };
                match handle_restore_option(
                    option,
                    &self.summarize_from_feedback,
                    &self.summarize_up_to_feedback,
                ) {
                    MessageSelectorAction::DismissConfirm => {
                        self.selector.screen = MessageSelectorScreen::PickList;
                        RewindDialogOutcome::None
                    }
                    MessageSelectorAction::Restore { option, feedback } => {
                        RewindDialogOutcome::Restore {
                            selected_index: self.selected_message_index,
                            option,
                            feedback,
                        }
                    }
                    _ => RewindDialogOutcome::None,
                }
            }
            KeyCode::Char(ch) => {
                if let Some(option) = self
                    .restore_options
                    .get(self.selected_restore_index)
                    .copied()
                {
                    if is_summarize_option(option)
                        && !key
                            .modifiers
                            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL)
                    {
                        match option {
                            RestoreOption::Summarize => self.summarize_from_feedback.push(ch),
                            RestoreOption::SummarizeUpTo => self.summarize_up_to_feedback.push(ch),
                            _ => {}
                        }
                    }
                }
                RewindDialogOutcome::None
            }
            KeyCode::Backspace => {
                if let Some(option) = self
                    .restore_options
                    .get(self.selected_restore_index)
                    .copied()
                {
                    match option {
                        RestoreOption::Summarize => {
                            self.summarize_from_feedback.pop();
                        }
                        RestoreOption::SummarizeUpTo => {
                            self.summarize_up_to_feedback.pop();
                        }
                        _ => {}
                    }
                }
                RewindDialogOutcome::None
            }
            _ => RewindDialogOutcome::None,
        }
    }

    fn render_pick_list(
        &self,
        frame: &mut Frame,
        area: Rect,
        selected: Style,
        normal: Style,
        dim: Style,
    ) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(1)])
            .split(area);

        let block = Block::default().borders(Borders::ALL).border_style(dim);
        let inner = block.inner(sections[0]);
        frame.render_widget(block, sections[0]);

        if self.options.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "No messages available to rewind.",
                    dim,
                ))),
                inner,
            );
        } else {
            let first = first_visible_index(self.selected_message_index, self.options.len());
            let visible = self
                .options
                .iter()
                .enumerate()
                .skip(first)
                .take(inner.height as usize);
            for (row_idx, (idx, option)) in visible.enumerate() {
                let is_focused = idx == self.selected_message_index;
                let style = if is_focused { selected } else { normal };
                let prefix = if is_focused { ">" } else { " " };
                let line = Line::from(vec![
                    Span::styled(format!("{prefix} "), style),
                    Span::styled(
                        truncate_to_width(&option.label, inner.width.saturating_sub(2) as usize),
                        style,
                    ),
                ]);
                frame.render_widget(
                    Paragraph::new(line),
                    Rect::new(inner.x, inner.y + row_idx as u16, inner.width, 1),
                );
            }
        }

        let footer = Line::from(vec![
            Span::styled(" Enter ", selected),
            Span::styled("choose", dim),
            Span::styled(" · Esc ", selected),
            Span::styled("close", dim),
        ]);
        frame.render_widget(Paragraph::new(footer), sections[1]);
    }

    fn render_confirm(
        &self,
        frame: &mut Frame,
        area: Rect,
        selected: Style,
        normal: Style,
        dim: Style,
    ) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(4),
                Constraint::Length(2),
                Constraint::Length(1),
            ])
            .split(area);

        let selected_message = self
            .options
            .get(self.selected_message_index)
            .map(|option| option.label.as_str())
            .unwrap_or("Current prompt");
        let header = Line::from(vec![
            Span::styled("Selected: ", dim),
            Span::styled(selected_message.to_string(), normal),
        ]);
        frame.render_widget(Paragraph::new(header), sections[0]);

        let list_block = Block::default().borders(Borders::ALL).border_style(dim);
        let list_inner = list_block.inner(sections[1]);
        frame.render_widget(list_block, sections[1]);
        for (idx, option) in self.restore_options.iter().enumerate() {
            if idx as u16 >= list_inner.height {
                break;
            }
            let is_focused = idx == self.selected_restore_index;
            let style = if is_focused { selected } else { normal };
            let prefix = if is_focused { ">" } else { " " };
            let line = Line::from(vec![
                Span::styled(format!("{prefix} "), style),
                Span::styled(option.label(), style),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(list_inner.x, list_inner.y + idx as u16, list_inner.width, 1),
            );
        }

        if let Some(option) = self
            .restore_options
            .get(self.selected_restore_index)
            .copied()
        {
            let mut lines = vec![Line::from(Span::styled(
                restore_option_conversation_text(option),
                dim,
            ))];
            if matches!(option, RestoreOption::Both | RestoreOption::Code) {
                match build_restore_code_confirmation(self.selected_diff_stats()) {
                    RestoreCodeConfirmation::NotLoaded => {}
                    RestoreCodeConfirmation::NoChanges => lines.push(Line::from(Span::styled(
                        "The code has not changed (nothing will be restored).",
                        dim,
                    ))),
                    RestoreCodeConfirmation::Restorable {
                        file_label,
                        diff_text,
                    } => lines.push(Line::from(Span::styled(
                        format!("The code will be restored {diff_text} in {file_label}."),
                        dim,
                    ))),
                }
            } else if !is_summarize_option(option) {
                lines.push(Line::from(Span::styled("The code will be unchanged.", dim)));
            }
            if is_summarize_option(option) {
                let feedback = match option {
                    RestoreOption::Summarize => &self.summarize_from_feedback,
                    RestoreOption::SummarizeUpTo => &self.summarize_up_to_feedback,
                    _ => unreachable!(),
                };
                lines.push(Line::from(vec![
                    Span::styled("Feedback: ", dim),
                    Span::styled(feedback.clone(), normal),
                ]));
            }
            frame.render_widget(Paragraph::new(lines), sections[2]);
        }

        let footer = Line::from(vec![
            Span::styled(" Enter ", selected),
            Span::styled("apply", dim),
            Span::styled(" · Esc ", selected),
            Span::styled("back", dim),
        ]);
        frame.render_widget(Paragraph::new(footer), sections[3]);
    }

    fn render_error(
        &self,
        frame: &mut Frame,
        area: Rect,
        selected: Style,
        _normal: Style,
        dim: Style,
        message: &str,
    ) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(1)])
            .split(area);
        frame.render_widget(Paragraph::new(message.to_string()), sections[0]);
        let footer = Line::from(vec![
            Span::styled(" Enter ", selected),
            Span::styled("back", dim),
            Span::styled(" · Esc ", selected),
            Span::styled("back", dim),
        ]);
        frame.render_widget(Paragraph::new(footer), sections[1]);
    }
}

pub fn build_message_option_label(raw_text: &str, columns: usize) -> String {
    match project_user_message_display(
        false,
        raw_text.trim().is_empty(),
        raw_text,
        columns,
        Some(8),
    ) {
        UserMessageDisplay::Current => "Current prompt".to_string(),
        UserMessageDisplay::Empty => "<empty message>".to_string(),
        UserMessageDisplay::BashInput { input } => format!("! {input}"),
        UserMessageDisplay::Command { name, args } => {
            if args.is_empty() {
                format!("/{name}")
            } else {
                format!("/{name} {args}")
            }
        }
        UserMessageDisplay::Skill { name } => format!("Skill({name})"),
        UserMessageDisplay::Text { text } => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn clean_capability() -> rebon_session::FileRestoreCapability {
        rebon_session::FileRestoreCapability::Clean(rebon_session::FileRestorePreflight {
            message_id: "u-1".into(),
            paths: Vec::new(),
            writes: 1,
            deletes: 0,
            unchanged: 0,
            conflicts: 0,
        })
    }

    fn unavailable_capability() -> rebon_session::FileRestoreCapability {
        rebon_session::FileRestoreCapability::Unavailable(
            rebon_session::FileRestoreUnavailableReason::MissingManifest,
        )
    }

    #[test]
    fn open_enables_code_restore_options_when_capability_is_clean() {
        let state = RewindDialogState::open(vec![RewindMessageOption {
            label: "msg".into(),
            raw_text: "hello".into(),
            diff_stats: Some(DiffStats {
                files_changed: vec!["src/main.rs".into()],
                insertions: 3,
                deletions: 1,
            }),
            file_restore: clean_capability(),
        }]);

        assert!(state.restore_options.contains(&RestoreOption::Code));
        assert!(state.restore_options.contains(&RestoreOption::Both));
    }

    #[test]
    fn moving_selection_recomputes_code_restore_options() {
        let mut state = RewindDialogState::open(vec![
            RewindMessageOption {
                label: "plain".into(),
                raw_text: "one".into(),
                diff_stats: None,
                file_restore: unavailable_capability(),
            },
            RewindMessageOption {
                label: "changed".into(),
                raw_text: "two".into(),
                diff_stats: Some(DiffStats {
                    files_changed: vec!["src/main.rs".into()],
                    insertions: 1,
                    deletions: 1,
                }),
                file_restore: clean_capability(),
            },
        ]);

        assert!(state.restore_options.contains(&RestoreOption::Code));
        state.handle_key(&key(KeyCode::Up));
        assert!(!state.restore_options.contains(&RestoreOption::Code));
        assert!(!state.restore_options.contains(&RestoreOption::Both));
    }
}
