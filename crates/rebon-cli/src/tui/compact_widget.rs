//! Progress widget for an immediate `/compact`.
//!
//! Compaction is one long provider call — 30 seconds is normal — with no
//! streamed output to watch. Before this widget the only feedback was the
//! spinner verb flipping to "Compacting", which is indistinguishable from a
//! model turn that has gone quiet. So the run gets its own surface above the
//! prompt, in the same slot the `/ultraplan` widget uses: a phase label, a
//! bar, and an elapsed clock, so a slow run reads as slow rather than hung.
//!
//! The bar is time-paced inside the summarising phase (see
//! [`CompactRunState::progress_percent`]) because the provider reports
//! nothing in between. It deliberately never reaches 100%: only the
//! finished run clears this state, and the transcript report is what says
//! "done".

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use rebon_tui::{parse_theme_color, RenderTheme};

use crate::tui::app::{AppState, CompactRunState};

/// Rows this widget paints when a run is active.
const COMPACT_WIDGET_ROWS: u16 = 1;
/// Bar width in cells, before it is clamped to the available area.
const BAR_CELLS: u16 = 24;

/// Rendered view of an in-flight compaction. Pure data so the layout and
/// the paint agree without either re-deriving the other's numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactViewModel {
    pub phase_label: &'static str,
    pub percent: u16,
    pub elapsed: String,
    pub messages_before: usize,
}

pub fn build_view_model(app: &AppState) -> Option<CompactViewModel> {
    let run = app.compact_run.as_ref()?;
    Some(CompactViewModel {
        phase_label: run.phase.label(),
        percent: run.progress_percent(),
        elapsed: format_elapsed(run),
        messages_before: run.messages_before,
    })
}

pub fn desired_height(app: &AppState, max_height: u16) -> u16 {
    if app.compact_run.is_none() || max_height == 0 {
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
    frame.render_widget(
        Paragraph::new(view_model_line(&vm, theme, area.width)),
        area,
    );
}

fn view_model_line(vm: &CompactViewModel, theme: &RenderTheme, width: u16) -> Line<'static> {
    let ds = rebon_design_system::theme::get_active_theme();
    let accent = parse_theme_color(ds.chromeYellow);
    let muted = theme
        .system_info
        .fg
        .unwrap_or_else(|| parse_theme_color(ds.subtle));

    // The label and trailing detail are fixed cost; the bar absorbs whatever
    // width is left so a narrow terminal degrades to a shorter bar instead of
    // wrapping the row.
    let detail = format!(
        " {:>3}%  {}  ·  {}  ·  {} messages",
        vm.percent, vm.elapsed, vm.phase_label, vm.messages_before
    );
    let bar_cells = BAR_CELLS
        .min(width.saturating_sub(detail.chars().count() as u16 + 12))
        .max(4);
    let (filled, empty) = bar_segments(vm.percent, bar_cells);

    Line::from(vec![
        Span::styled(
            "Compacting ".to_string(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled("▕".to_string(), Style::default().fg(muted)),
        Span::styled(filled, Style::default().fg(accent)),
        Span::styled(empty, Style::default().fg(muted)),
        Span::styled("▏".to_string(), Style::default().fg(muted)),
        Span::styled(detail, Style::default().fg(muted)),
    ])
}

/// Split a bar into its filled and empty runs. Rounds down so the bar only
/// shows a cell once that cell's worth of progress actually happened.
fn bar_segments(percent: u16, cells: u16) -> (String, String) {
    let filled = (u32::from(percent.min(100)) * u32::from(cells) / 100) as u16;
    (
        "█".repeat(filled as usize),
        "░".repeat(cells.saturating_sub(filled) as usize),
    )
}

fn format_elapsed(run: &CompactRunState) -> String {
    let seconds = run.started_at.elapsed().as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{CompactPhase, COMPACT_NOMINAL_DURATION};
    use std::time::Instant;

    fn run_state(phase: CompactPhase, elapsed: std::time::Duration) -> CompactRunState {
        CompactRunState {
            phase,
            started_at: Instant::now() - elapsed,
            messages_before: 128,
            respond_to_command_id: None,
        }
    }

    #[test]
    fn no_run_means_no_widget_rows() {
        let app = AppState::new();
        assert_eq!(desired_height(&app, 10), 0);
        assert!(build_view_model(&app).is_none());
    }

    #[test]
    fn an_active_run_claims_exactly_one_row_and_clamps_to_the_budget() {
        let mut app = AppState::new();
        app.compact_run = Some(run_state(
            CompactPhase::Summarizing,
            std::time::Duration::ZERO,
        ));
        assert_eq!(desired_height(&app, 10), 1);
        assert_eq!(desired_height(&app, 0), 0);
    }

    #[test]
    fn the_bar_advances_with_elapsed_time_but_never_completes() {
        let start =
            run_state(CompactPhase::Summarizing, std::time::Duration::ZERO).progress_percent();
        let nominal =
            run_state(CompactPhase::Summarizing, COMPACT_NOMINAL_DURATION).progress_percent();
        let overrun =
            run_state(CompactPhase::Summarizing, COMPACT_NOMINAL_DURATION * 20).progress_percent();

        assert!(start < nominal, "{start} !< {nominal}");
        assert!(nominal < overrun, "{nominal} !< {overrun}");
        assert!(
            overrun < 100,
            "a running compaction must never read as done"
        );
    }

    #[test]
    fn phases_progress_monotonically() {
        let reading = run_state(CompactPhase::Reading, std::time::Duration::ZERO);
        let summarizing = run_state(CompactPhase::Summarizing, std::time::Duration::ZERO);
        assert!(reading.progress_percent() < summarizing.progress_percent());
        assert_eq!(reading.phase.label(), "Reading session history");
    }

    #[test]
    fn the_bar_fills_proportionally_and_keeps_its_width() {
        for percent in [0u16, 33, 50, 100] {
            let (filled, empty) = bar_segments(percent, 20);
            assert_eq!(
                filled.chars().count() + empty.chars().count(),
                20,
                "bar width must not depend on progress"
            );
        }
        assert_eq!(bar_segments(50, 20).0.chars().count(), 10);
        assert_eq!(bar_segments(0, 20).0.chars().count(), 0);
    }

    #[test]
    fn a_narrow_terminal_shortens_the_bar_instead_of_overflowing_the_row() {
        let vm = CompactViewModel {
            phase_label: "Summarizing conversation",
            percent: 42,
            elapsed: "0:12".into(),
            messages_before: 9,
        };
        let theme = RenderTheme::plain();
        for width in [20u16, 40, 80, 200] {
            let line = view_model_line(&vm, &theme, width);
            let bar = line.spans[2].content.chars().count() + line.spans[3].content.chars().count();
            assert!(bar >= 4, "the bar must stay legible at width {width}");
            assert!(
                bar <= BAR_CELLS as usize,
                "the bar must not grow past its cap at width {width}"
            );
        }
    }
}
