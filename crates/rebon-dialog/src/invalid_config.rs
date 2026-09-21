//! Exit/reset branch over an invalid JSON config file.
//!
//! ## Behaviour
//!
//! * The fixed title + error color.
//! * The two-option list (`Exit and fix manually` / `Reset with default
//!   configuration`).
//! * The cancel-defaults-to-exit branch.
//! * The body text projection (which references the file path).
//! * The `SAFE_ERROR_THEME_NAME` constant.
//!
//! ## Outbound seam
//!
//! The consumer reads the file path + error description from a
//! `ConfigParseError` and, on reset, writes `error.default_config`
//! back to the file. This module models the action enum; the consumer
//! drives the IO.

use crate::common::{DialogColor, SelectOption};

/// The dialog title.
pub const TITLE: &str = "Configuration Error";

/// The prompt shown above the options.
pub const PROMPT_TEXT: &str = "Choose an option:";

/// Name of the theme used while the error is shown.
pub const SAFE_ERROR_THEME_NAME: &str = "dark";

/// The dialog frame color.
pub const DIALOG_COLOR: DialogColor = DialogColor::Error;

/// Build the templated body text.
pub fn build_body(file_path: &str) -> String {
    format!("The configuration file at {file_path} contains invalid JSON.")
}

/// Option values: `exit` or `reset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidConfigValue {
    /// "Exit and fix manually".
    Exit,
    /// "Reset with default configuration".
    Reset,
}

/// Action emitted by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidConfigAction {
    /// Exit so the user can fix the file (the consumer exits with code 1).
    Exit,
    /// Reset the file (the consumer writes the default config back to
    /// the file path and then exits with code 0).
    Reset,
}

/// Build the option list.
pub fn build_options() -> Vec<SelectOption<InvalidConfigValue>> {
    vec![
        SelectOption::new("Exit and fix manually", InvalidConfigValue::Exit),
        SelectOption::new(
            "Reset with default configuration",
            InvalidConfigValue::Reset,
        ),
    ]
}

/// Reducer.
pub fn handle_event(value: InvalidConfigValue) -> InvalidConfigAction {
    match value {
        InvalidConfigValue::Exit => InvalidConfigAction::Exit,
        InvalidConfigValue::Reset => InvalidConfigAction::Reset,
    }
}

/// Cancel handler — defaults to exit (the same action as choosing `exit`).
pub fn handle_cancel() -> InvalidConfigAction {
    InvalidConfigAction::Exit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_pinned() {
        assert_eq!(TITLE, "Configuration Error");
    }

    #[test]
    fn dialog_color_error() {
        assert_eq!(DIALOG_COLOR, DialogColor::Error);
    }

    #[test]
    fn prompt_text_pinned() {
        assert_eq!(PROMPT_TEXT, "Choose an option:");
    }

    #[test]
    fn safe_theme_pinned() {
        assert_eq!(SAFE_ERROR_THEME_NAME, "dark");
    }

    #[test]
    fn build_body_templates_path() {
        assert_eq!(
            build_body("/home/u/.rebon/config.json"),
            "The configuration file at /home/u/.rebon/config.json contains invalid JSON."
        );
    }

    #[test]
    fn build_options_order() {
        let opts = build_options();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].value, InvalidConfigValue::Exit);
        assert_eq!(opts[1].value, InvalidConfigValue::Reset);
    }

    #[test]
    fn handle_exit() {
        assert_eq!(
            handle_event(InvalidConfigValue::Exit),
            InvalidConfigAction::Exit
        );
    }

    #[test]
    fn handle_reset() {
        assert_eq!(
            handle_event(InvalidConfigValue::Reset),
            InvalidConfigAction::Reset
        );
    }

    #[test]
    fn handle_cancel_defaults_exit() {
        assert_eq!(handle_cancel(), InvalidConfigAction::Exit);
    }
}
