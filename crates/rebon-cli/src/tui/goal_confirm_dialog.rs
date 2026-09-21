use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use rebon_tui::{parse_theme_color, RenderTheme};
use rebon_width::truncate_to_ellipsis;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalConfirmDialogState {
    pub prompt: String,
    pub max_sessions: Option<u32>,
    pub selected: GoalConfirmSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalConfirmSelection {
    Replace,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalConfirmOutcome {
    None,
    Replace {
        prompt: String,
        max_sessions: Option<u32>,
    },
    Cancel,
}

impl GoalConfirmDialogState {
    pub fn new(prompt: String, max_sessions: Option<u32>) -> Self {
        Self {
            prompt,
            max_sessions,
            selected: GoalConfirmSelection::Replace,
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> GoalConfirmOutcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return GoalConfirmOutcome::None;
        }
        match key.code {
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Tab
            | KeyCode::BackTab => {
                self.selected = match self.selected {
                    GoalConfirmSelection::Replace => GoalConfirmSelection::Cancel,
                    GoalConfirmSelection::Cancel => GoalConfirmSelection::Replace,
                };
                GoalConfirmOutcome::None
            }
            KeyCode::Enter => match self.selected {
                GoalConfirmSelection::Replace => GoalConfirmOutcome::Replace {
                    prompt: self.prompt.clone(),
                    max_sessions: self.max_sessions,
                },
                GoalConfirmSelection::Cancel => GoalConfirmOutcome::Cancel,
            },
            KeyCode::Esc => GoalConfirmOutcome::Cancel,
            _ => GoalConfirmOutcome::None,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, _theme: &RenderTheme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let dialog = centered_rect(area, 72, 42);
        frame.render_widget(Clear, dialog);

        let ds = rebon_design_system::theme::get_active_theme();
        let border_style = Style::default().fg(parse_theme_color(ds.permission));
        let title_style = Style::default()
            .fg(parse_theme_color(ds.text))
            .add_modifier(Modifier::BOLD);
        let inactive = Style::default().fg(parse_theme_color(ds.inactive));
        let text = Style::default().fg(parse_theme_color(ds.text));
        let selected = Style::default()
            .fg(parse_theme_color(ds.inverseText))
            .bg(parse_theme_color(ds.text))
            .add_modifier(Modifier::BOLD);
        let replace_style = if self.selected == GoalConfirmSelection::Replace {
            selected
        } else {
            Style::default().fg(parse_theme_color(ds.success))
        };
        let cancel_style = if self.selected == GoalConfirmSelection::Cancel {
            selected
        } else {
            Style::default().fg(parse_theme_color(ds.warning))
        };

        let prompt = truncate_to_ellipsis(&self.prompt, dialog.width.saturating_sub(6) as usize);
        let replace_prefix = if self.selected == GoalConfirmSelection::Replace {
            "› "
        } else {
            "  "
        };
        let cancel_prefix = if self.selected == GoalConfirmSelection::Cancel {
            "› "
        } else {
            "  "
        };
        let lines = vec![
            Line::from(Span::styled(
                "A completed or archived goal already exists in this session.",
                text,
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("New goal: ", inactive),
                Span::styled(prompt, text),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                format!("{replace_prefix}Replace goal"),
                replace_style,
            )),
            Line::from(Span::styled(format!("{cancel_prefix}Cancel"), cancel_style)),
            Line::from(""),
            Line::from(Span::styled(
                "Enter: confirm · ↑/↓: choose · Esc: cancel",
                inactive,
            )),
        ];

        let block = Block::default()
            .title(Line::from(Span::styled(" Replace goal? ", title_style)))
            .borders(Borders::ALL)
            .border_style(border_style);
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

fn centered_rect(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let width = area
        .width
        .saturating_mul(width_pct)
        .saturating_div(100)
        .max(20);
    let height = area
        .height
        .saturating_mul(height_pct)
        .saturating_div(100)
        .max(8);
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    #[test]
    fn enter_confirms_selected_replace() {
        let mut dialog = GoalConfirmDialogState::new("ship it".into(), Some(3));
        let outcome = dialog.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            outcome,
            GoalConfirmOutcome::Replace {
                prompt: "ship it".into(),
                max_sessions: Some(3)
            }
        );
    }

    #[test]
    fn arrows_toggle_to_cancel() {
        let mut dialog = GoalConfirmDialogState::new("ship it".into(), None);
        dialog.handle_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(dialog.selected, GoalConfirmSelection::Cancel);
        assert_eq!(
            dialog.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            GoalConfirmOutcome::Cancel
        );
    }
}
