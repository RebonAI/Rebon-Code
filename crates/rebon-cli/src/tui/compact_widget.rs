//! Status widget for an immediate `/compact` or idle auto-compaction.
//!
//! In-turn compaction uses the turn spinner instead.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use rebon_tui::{parse_theme_color, RenderTheme};

use crate::tui::app::AppState;

const COMPACT_WIDGET_ROWS: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactViewModel {
    pub dot_bright: bool,
}

pub fn build_view_model(app: &AppState) -> Option<CompactViewModel> {
    let run = app.compact_run.as_ref().filter(|run| !run.invalidated)?;
    Some(CompactViewModel {
        dot_bright: (run.started_at.elapsed().as_millis() / 500) % 2 == 0,
    })
}

pub fn desired_height(app: &AppState, max_height: u16) -> u16 {
    if build_view_model(app).is_none() || max_height == 0 {
        return 0;
    }
    COMPACT_WIDGET_ROWS.min(max_height)
}

pub fn render(frame: &mut Frame, area: Rect, app: &AppState, theme: &RenderTheme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(vm) = build_view_model(app) else {
        return;
    };
    frame.render_widget(Paragraph::new(view_model_line(&vm, theme)), area);
}

fn view_model_line(vm: &CompactViewModel, theme: &RenderTheme) -> Line<'static> {
    let ds = rebon_design_system::theme::get_active_theme();
    let accent = parse_theme_color(ds.chromeYellow);
    let muted = theme
        .system_info
        .fg
        .unwrap_or_else(|| parse_theme_color(ds.subtle));
    let dot_color = if vm.dot_bright { accent } else { muted };
    Line::from(vec![
        Span::styled("● ", Style::default().fg(dot_color)),
        Span::styled("Auto Compacting…", Style::default().fg(accent)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{CompactPhase, CompactRunState};
    use std::time::Instant;

    fn run_state(phase: CompactPhase) -> CompactRunState {
        CompactRunState {
            phase,
            started_at: Instant::now(),
            respond_to_command_id: None,
            automatic: false,
            invalidated: false,
        }
    }

    #[test]
    fn no_run_means_no_widget_rows() {
        let app = AppState::new();
        assert_eq!(desired_height(&app, 10), 0);
        assert!(build_view_model(&app).is_none());
    }

    #[test]
    fn compact_run_shows_a_status_until_invalidated_or_finished() {
        for automatic in [false, true] {
            let mut app = AppState::new();
            let mut run = run_state(CompactPhase::Summarizing);
            run.automatic = automatic;
            app.compact_run = Some(run);
            assert_eq!(desired_height(&app, 10), 1);
            assert_eq!(desired_height(&app, 0), 0);
            assert!(build_view_model(&app).is_some());
            app.compact_run.as_mut().unwrap().invalidated = true;
            assert_eq!(desired_height(&app, 10), 0);
            app.compact_run = None;
            assert_eq!(desired_height(&app, 10), 0);
        }
    }

    #[test]
    fn compaction_changes_the_dot_style_without_changing_the_label() {
        let theme = RenderTheme::plain();
        let mut vm = CompactViewModel { dot_bright: true };
        let bright = view_model_line(&vm, &theme);
        vm.dot_bright = false;
        let dim = view_model_line(&vm, &theme);
        assert_ne!(bright.spans[0].style, dim.spans[0].style);
        assert_eq!(bright.spans[1].content, "Auto Compacting…");
        assert_eq!(bright.spans[1], dim.spans[1]);
    }

    #[test]
    fn manual_compaction_uses_the_same_status_as_idle_auto_compaction() {
        let theme = RenderTheme::plain();
        for phase in [CompactPhase::Reading, CompactPhase::Summarizing] {
            let mut app = AppState::new();
            app.compact_run = Some(run_state(phase));
            let mut manual = build_view_model(&app).unwrap();
            app.compact_run.as_mut().unwrap().automatic = true;
            let mut automatic = build_view_model(&app).unwrap();
            for dot_bright in [false, true] {
                manual.dot_bright = dot_bright;
                automatic.dot_bright = dot_bright;
                let manual_line = view_model_line(&manual, &theme);
                assert_eq!(manual_line, view_model_line(&automatic, &theme));
                assert_eq!(manual_line.spans.len(), 2);
                assert_eq!(manual_line.spans[1].content, "Auto Compacting…");
            }
        }
    }

    #[test]
    fn in_turn_compaction_uses_only_the_turn_spinner() {
        let mut app = AppState::new();
        app.spinner_verb_before_compacting = Some("Thinking".into());
        for manual in [false, true] {
            app.turn_compact_manual = manual;
            assert_eq!(desired_height(&app, 10), 0);
            assert!(build_view_model(&app).is_none());
        }
    }
}
