//! Quick-open dialog runtime: applies selected actions and owns the
//! shared prompt insertion helper used by path/search dialogs.

use std::path::{Path, PathBuf};

use rebon_dialog::quick_open::QuickOpenAction;
use rebon_tui::promptinput::clamp_cursor_offset;

use crate::tui::app::AppState;
use crate::tui::external_editor::open_file_in_external_editor;

pub(super) fn apply_quick_open_action(app: &mut AppState, cwd: &Path, action: QuickOpenAction) {
    match action {
        QuickOpenAction::OpenInEditor { path } => {
            let full = cwd.join(PathBuf::from(path));
            let _ = open_file_in_external_editor(&full, None);
        }
        QuickOpenAction::InsertMention { text } | QuickOpenAction::InsertPath { text } => {
            insert_with_spacing(app, &text);
        }
        QuickOpenAction::Cancel => {}
    }
}

pub(super) fn insert_with_spacing(app: &mut AppState, text: &str) {
    let cursor = clamp_cursor_offset(&app.input, app.cursor_offset);
    let needs_leading_space = cursor > 0
        && app.input[..cursor]
            .chars()
            .last()
            .map(|ch| !ch.is_whitespace())
            .unwrap_or(false);
    let insertion = if needs_leading_space {
        format!(" {text}")
    } else {
        text.to_string()
    };
    let before = &app.input[..cursor];
    let after = &app.input[cursor..];
    app.input = format!("{before}{insertion}{after}");
    app.cursor_offset = cursor + insertion.len();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_with_spacing_preserves_text_after_cursor() {
        let mut app = AppState::default();
        app.input = "go  now".into();
        app.cursor_offset = 3;

        insert_with_spacing(&mut app, "@file");

        assert_eq!(app.input, "go @file now");
        assert_eq!(app.cursor_offset, 8);
    }

    #[test]
    fn insert_with_spacing_clamps_cursor_past_end() {
        let mut app = AppState::default();
        app.input = "abc".into();
        app.cursor_offset = usize::MAX;

        insert_with_spacing(&mut app, "@file");

        assert_eq!(app.input, "abc @file");
        assert_eq!(app.cursor_offset, 9);
    }

    #[test]
    fn insert_with_spacing_snaps_cursor_to_utf8_boundary() {
        let mut app = AppState::default();
        app.input = "你a".into();
        app.cursor_offset = 2;

        insert_with_spacing(&mut app, "@file");

        assert_eq!(app.input, "@file你a");
        assert_eq!(app.cursor_offset, 5);
    }
}
