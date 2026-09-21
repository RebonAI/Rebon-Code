//! The terminal's half of the dialog host.
//!
//! The stack, the key route and the view queries live in
//! `rebon_dialog::host`; painting a declarative `ViewSpec` lives in
//! `rebon_tui::dialog_view`. What is left here is the one thing only
//! this surface can answer: where a dialog sits on a full screen.
//!
//! No dialog paints itself here any more. Every panel on the stack — the
//! ones the harness registers and the ones a plugin does — describes a
//! `ViewSpec`, so this module has no renderer of its own to dispatch to.

use ratatui::layout::Rect;
use ratatui::Frame;

pub use rebon_dialog::host::{DialogStack as DialogHost, StackKey as HostKey};
pub use rebon_tui::dialog_view::handle_key;

/// Paint the top dialog on a full screen, where this surface wants it,
/// and report whether anything was painted.
pub fn render_screen(host: &mut DialogHost, frame: &mut Frame, area: Rect, overlay: Rect) -> bool {
    let rect = match host.top_id() {
        Some(rebon_ui_seat::ids::dialog::MODEL) => host
            .top_desired_height()
            .map(|desired| compact_picker_rect(area, desired)),
        _ => host
            .top_desired_height()
            .map(|desired| wide_rect(area, desired)),
    };
    render_at(host, frame, rect.unwrap_or(overlay))
}

/// Paint the top dialog into the inline prompt area, which it fills
/// exactly rather than centring anything inside it.
pub fn render_inline(host: &mut DialogHost, frame: &mut Frame, area: Rect) -> bool {
    render_at(host, frame, area)
}

/// Whether the top dialog paints in the inline prompt area.
///
/// Every dialog on the stack does, through the shared painters: no panel
/// this surface hosts keeps a renderer of its own any more.
pub fn has_inline_view(host: &DialogHost) -> bool {
    host.is_open()
}

/// Paint the top dialog into exactly `rect`, then tell the stack how
/// many body rows it got so the next key can page by a real screenful.
fn render_at(host: &mut DialogHost, frame: &mut Frame, rect: Rect) -> bool {
    // Told before it is asked: a panel whose layout depends on the frame
    // answers `view` for the size it is actually getting.
    host.note_viewport(
        rect.height.saturating_sub(2).max(1),
        rect.width.saturating_sub(2),
    );
    let Some(rows) = rebon_tui::dialog_view::render_top_view(host, frame, rect) else {
        return false;
    };
    host.note_viewport(rows.max(1), rect.width.saturating_sub(2));
    true
}

/// A compact centred rect for a picker that should not stretch across
/// the overlay.
pub fn compact_picker_rect(area: Rect, desired_height: u16) -> Rect {
    centred(area, area.width.min(64).max(20), desired_height)
}

/// The default centred rect: as wide as the screen allows up to 96.
fn wide_rect(area: Rect, desired_height: u16) -> Rect {
    centred(
        area,
        area.width.saturating_sub(2).min(96).max(20),
        desired_height,
    )
}

fn centred(area: Rect, width: u16, desired_height: u16) -> Rect {
    // Leave a row of breathing space top and bottom where there is any,
    // and never report a rect that leaves the area it was given.
    let width = width.min(area.width);
    let height = desired_height
        .min(area.height.saturating_sub(2).max(1))
        .min(area.height);
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
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::Terminal;
    use rebon_dialog::doctor_dialog::{DoctorDialogState, DoctorReport, DIALOG_ID as DOCTOR};
    use rebon_dialog::effort_dialog::{EffortDialogState, DIALOG_ID as EFFORT};
    use rebon_plugin_agents::dialog::{AgentsDialogState, DIALOG_ID as AGENTS};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn top_row(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect()
    }

    #[test]
    fn a_key_release_is_swallowed_without_reaching_the_reducer() {
        let mut host = DialogHost::default();
        host.push(EffortDialogState::open("m", None));
        let release = KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert_eq!(handle_key(&mut host, &release), HostKey::Consumed);
        // The selection did not move, so the release never reduced.
        assert_eq!(host.top_desired_height(), Some(9));
        assert_eq!(host.top_id(), Some(EFFORT));
    }

    #[test]
    fn an_unmapped_key_is_swallowed_and_an_empty_host_passes_keys_through() {
        let mut host = DialogHost::default();
        assert_eq!(
            handle_key(&mut host, &press(KeyCode::F(5))),
            HostKey::NotConsumed
        );
        host.push(EffortDialogState::open("m", None));
        assert_eq!(
            handle_key(&mut host, &press(KeyCode::F(5))),
            HostKey::Consumed
        );
    }

    #[test]
    fn enter_on_the_effort_picker_routes_a_closing_action() {
        let mut host = DialogHost::default();
        host.push(EffortDialogState::open("m", None));
        let HostKey::Action(action) = handle_key(&mut host, &press(KeyCode::Enter)) else {
            panic!("expected a select action");
        };
        assert_eq!((action.dialog, action.value()), (EFFORT, "high"));
        assert!(!host.is_open(), "a closing action pops the dialog");
    }

    #[test]
    fn a_panel_view_paints_in_both_hosts() {
        let mut host = DialogHost::default();
        host.push(DoctorDialogState::open(&DoctorReport {
            summary: vec![("version".into(), "1.0".into())],
            sections: Vec::new(),
        }));
        assert_eq!(host.top_id(), Some(DOCTOR));

        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        let mut painted = false;
        terminal
            .draw(|frame| {
                painted = render_screen(&mut host, frame, frame.area(), frame.area());
            })
            .unwrap();
        assert!(painted);
        assert!(top_row(&terminal).contains("Doctor"));

        // A panel that reports no desired height fills the inline area
        // exactly rather than centring inside it. `/doctor` never opens
        // in inline mode, but the two that do — settings and the plugin
        // manager — take this path, so it must not paint nothing.
        terminal
            .draw(|frame| painted = render_inline(&mut host, frame, frame.area()))
            .unwrap();
        assert!(painted);
    }

    /// A panel a plugin owns, painted by the same generic painter as
    /// the harness's own: nothing about `/agents` is registered on this
    /// surface, so a panel that stopped describing a `ViewSpec` would
    /// open invisible here and swallow every key.
    #[test]
    fn the_plugin_owned_agents_panel_paints_through_the_generic_painter() {
        let project = tempfile::TempDir::new().expect("temp project");
        let mut host = DialogHost::default();
        host.push(AgentsDialogState::open(
            project.path(),
            vec!["Read".into()],
            None,
        ));
        assert_eq!(host.top_id(), Some(AGENTS));

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let mut painted = false;
        terminal
            .draw(|frame| {
                painted = render_screen(&mut host, frame, frame.area(), frame.area());
            })
            .unwrap();
        assert!(painted, "the agents panel described no paintable view");
        assert!(top_row(&terminal).contains("File-backed agents"));

        terminal
            .draw(|frame| painted = render_inline(&mut host, frame, frame.area()))
            .unwrap();
        assert!(painted);
    }

    #[test]
    fn the_effort_picker_paints_in_both_hosts() {
        let mut host = DialogHost::default();
        host.push(EffortDialogState::open("gpt-5.6-sol", None));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let mut painted = false;
        terminal
            .draw(|frame| {
                painted = render_screen(&mut host, frame, frame.area(), frame.area());
            })
            .unwrap();
        assert!(painted);
        terminal
            .draw(|frame| painted = render_inline(&mut host, frame, frame.area()))
            .unwrap();
        assert!(painted);
        assert!(top_row(&terminal).contains("Select Reasoning Level for gpt-5.6-sol"));
    }

    #[test]
    fn screen_placement_centres_a_list_and_keeps_the_model_picker_compact() {
        let area = Rect::new(0, 0, 120, 40);
        let effort = centred(area, area.width.saturating_sub(2).min(96).max(20), 9);
        assert_eq!((effort.width, effort.height, effort.x), (96, 9, 12));
        assert_eq!(compact_picker_rect(area, 16).width, 64);
        // A narrow terminal clamps both shapes to what it has.
        assert_eq!(compact_picker_rect(Rect::new(0, 0, 30, 10), 5).width, 30);
    }

    #[test]
    fn a_placed_rect_never_leaves_the_area_it_was_given() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(2, 3, 1, 1),
            Rect::new(0, 0, 8, 2),
            Rect::new(4, 4, 200, 60),
        ] {
            for rect in [
                compact_picker_rect(area, 18),
                centred(area, area.width.saturating_sub(2).min(96).max(20), 18),
            ] {
                assert!(rect.x >= area.x && rect.y >= area.y, "{rect:?} vs {area:?}");
                assert!(rect.right() <= area.right(), "{rect:?} vs {area:?}");
                assert!(rect.bottom() <= area.bottom(), "{rect:?} vs {area:?}");
            }
        }
    }
}
