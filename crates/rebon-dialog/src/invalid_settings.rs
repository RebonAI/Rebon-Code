//! Exit/continue branch over a list of validation errors.
//!
//! ## Behaviour
//!
//! * The fixed title + warning color.
//! * The two-option list (`Exit and fix manually` / `Continue without these settings`).
//! * The validation-errors list pre-built input shape.
//! * The footer hint text.
//! * The cancel-defaults-to-exit branch.

use crate::common::{DialogColor, SelectOption};

/// The dialog title.
pub const TITLE: &str = "Settings Error";

/// The footer hint text.
pub const FOOTER_TEXT: &str =
    "Files with errors are skipped entirely, not just the invalid settings.";

/// The dialog frame color.
pub const DIALOG_COLOR: DialogColor = DialogColor::Warning;

/// A pre-built validation error row. This module consumes a flat
/// string-based shape so it carries no dependency on the settings
/// validation types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationErrorInput {
    /// File path (e.g. "/home/u/.rebon/settings.json").
    pub path: String,
    /// Human-readable error message.
    pub message: String,
}

/// Option values: `exit` or `continue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidSettingsValue {
    /// "Exit and fix manually".
    Exit,
    /// "Continue without these settings".
    Continue,
}

/// Actions emitted by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidSettingsAction {
    /// Exit so the user can fix the files.
    Exit,
    /// Continue without the invalid files.
    Continue,
}

/// Build the option list (Exit first, Continue second).
pub fn build_options() -> Vec<SelectOption<InvalidSettingsValue>> {
    vec![
        SelectOption::new("Exit and fix manually", InvalidSettingsValue::Exit),
        SelectOption::new(
            "Continue without these settings",
            InvalidSettingsValue::Continue,
        ),
    ]
}

/// Reducer.
pub fn handle_event(value: InvalidSettingsValue) -> InvalidSettingsAction {
    match value {
        InvalidSettingsValue::Exit => InvalidSettingsAction::Exit,
        InvalidSettingsValue::Continue => InvalidSettingsAction::Continue,
    }
}

/// Cancel handler — defaults to exit (the same action as choosing `exit`).
pub fn handle_cancel() -> InvalidSettingsAction {
    InvalidSettingsAction::Exit
}

/// Display projection: how many error files are listed?
pub fn error_count(errors: &[ValidationErrorInput]) -> usize {
    errors.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(path: &str, message: &str) -> ValidationErrorInput {
        ValidationErrorInput {
            path: path.into(),
            message: message.into(),
        }
    }

    #[test]
    fn title_pinned() {
        assert_eq!(TITLE, "Settings Error");
    }

    #[test]
    fn footer_text_pinned() {
        assert!(FOOTER_TEXT.contains("Files with errors"));
    }

    #[test]
    fn dialog_color_is_warning() {
        assert_eq!(DIALOG_COLOR, DialogColor::Warning);
    }

    #[test]
    fn build_options_order_exit_first() {
        let opts = build_options();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "Exit and fix manually");
        assert_eq!(opts[0].value, InvalidSettingsValue::Exit);
        assert_eq!(opts[1].label, "Continue without these settings");
        assert_eq!(opts[1].value, InvalidSettingsValue::Continue);
    }

    #[test]
    fn handle_event_exit() {
        assert_eq!(
            handle_event(InvalidSettingsValue::Exit),
            InvalidSettingsAction::Exit
        );
    }

    #[test]
    fn handle_event_continue() {
        assert_eq!(
            handle_event(InvalidSettingsValue::Continue),
            InvalidSettingsAction::Continue
        );
    }

    #[test]
    fn handle_cancel_defaults_to_exit() {
        assert_eq!(handle_cancel(), InvalidSettingsAction::Exit);
    }

    #[test]
    fn error_count_empty() {
        assert_eq!(error_count(&[]), 0);
    }

    #[test]
    fn error_count_multi() {
        let errors = vec![
            err("/a.json", "missing key"),
            err("/b.json", "bad type"),
            err("/c.json", "syntax"),
        ];
        assert_eq!(error_count(&errors), 3);
    }
}
