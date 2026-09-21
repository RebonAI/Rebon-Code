//! The theme picker: a fixed seven-theme option list plus the
//! preview/save/cancel state machine over it.
//!
//! ## Covered behavior
//!
//! * The full theme option list, with and without the auto theme.
//! * The two title variants (`"Let's get started."` and `"Theme"`).
//! * The fixed subtitle string.
//! * The preview / save / cancel transitions.
//! * The skip-exit-handling branch of cancel.
//! * The Enter/Esc footer hint values.
//!
//! ## Out of scope
//!
//! * Rendering the list.
//! * Exiting the process: cancel is reported as
//!   [`ThemePickerAction::CancelPreview`] carrying `exit`, and the
//!   consumer decides what that means.
//! * The demo diff preview — visual only. Only its path
//!   ([`DEMO_DIFF_PATH`]) is published here.

use crate::common::SelectOption;

/// Subtitle shown beneath the picker title.
pub const SUBTITLE: &str = "Choose the text style that looks best with your terminal";

/// Title shown when the picker is part of onboarding.
pub const INTRO_TITLE: &str = "Let's get started.";

/// Title shown when the picker stands alone.
pub const STANDALONE_TITLE: &str = "Theme";

/// Path of the demo diff shown in the preview.
pub const DEMO_DIFF_PATH: &str = "demo.js";

/// The full set of theme setting values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeSetting {
    /// Matches the terminal's palette (only offered when the caller
    /// enables the auto theme).
    Auto,
    /// Dark mode.
    Dark,
    /// Light mode.
    Light,
    /// Dark mode, colorblind-friendly.
    DarkDaltonized,
    /// Light mode, colorblind-friendly.
    LightDaltonized,
    /// Dark mode, ANSI colors only.
    DarkAnsi,
    /// Light mode, ANSI colors only.
    LightAnsi,
}

impl ThemeSetting {
    /// Machine-readable id used as the `value` in the option list; the
    /// same string the theme setting stores.
    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::DarkDaltonized => "dark-daltonized",
            Self::LightDaltonized => "light-daltonized",
            Self::DarkAnsi => "dark-ansi",
            Self::LightAnsi => "light-ansi",
        }
    }

    /// Display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto (match terminal)",
            Self::Dark => "Dark mode",
            Self::Light => "Light mode",
            Self::DarkDaltonized => "Dark mode (colorblind-friendly)",
            Self::LightDaltonized => "Light mode (colorblind-friendly)",
            Self::DarkAnsi => "Dark mode (ANSI colors only)",
            Self::LightAnsi => "Light mode (ANSI colors only)",
        }
    }
}

/// Build the full option list. `include_auto` puts the auto theme first.
pub fn build_options(include_auto: bool) -> Vec<SelectOption<ThemeSetting>> {
    let mut out = Vec::with_capacity(7);
    if include_auto {
        out.push(SelectOption::new(
            ThemeSetting::Auto.label(),
            ThemeSetting::Auto,
        ));
    }
    for v in [
        ThemeSetting::Dark,
        ThemeSetting::Light,
        ThemeSetting::DarkDaltonized,
        ThemeSetting::LightDaltonized,
        ThemeSetting::DarkAnsi,
        ThemeSetting::LightAnsi,
    ] {
        out.push(SelectOption::new(v.label(), v));
    }
    out
}

/// Action emitted by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemePickerAction {
    /// Update the preview theme (fires on focus change).
    PreviewTheme(ThemeSetting),
    /// Save the previewed theme and report the selection.
    SaveAndSelect(ThemeSetting),
    /// Cancel the preview. When `exit == true` (exit handling not
    /// skipped), the consumer should also shut down cleanly; when
    /// `false`, the consumer should run its cancel handler if it has
    /// one.
    CancelPreview {
        /// Whether cancelling should also exit the process.
        exit: bool,
    },
}

/// Handle a focus-change event.
pub fn on_focus(setting: ThemeSetting) -> ThemePickerAction {
    ThemePickerAction::PreviewTheme(setting)
}

/// Handle a confirm event.
pub fn on_select(setting: ThemeSetting) -> ThemePickerAction {
    ThemePickerAction::SaveAndSelect(setting)
}

/// Handle a cancel event. `skip_exit_handling == true` is the
/// onboarding branch (no process exit).
pub fn on_cancel(skip_exit_handling: bool) -> ThemePickerAction {
    ThemePickerAction::CancelPreview {
        exit: !skip_exit_handling,
    }
}

/// Resolve the shown title.
pub fn resolve_title(show_intro_text: bool) -> &'static str {
    if show_intro_text {
        INTRO_TITLE
    } else {
        STANDALONE_TITLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_pinned() {
        assert_eq!(
            SUBTITLE,
            "Choose the text style that looks best with your terminal"
        );
    }

    #[test]
    fn title_table() {
        assert_eq!(resolve_title(true), "Let's get started.");
        assert_eq!(resolve_title(false), "Theme");
    }

    #[test]
    fn theme_setting_id_table() {
        assert_eq!(ThemeSetting::Auto.id(), "auto");
        assert_eq!(ThemeSetting::Dark.id(), "dark");
        assert_eq!(ThemeSetting::Light.id(), "light");
        assert_eq!(ThemeSetting::DarkDaltonized.id(), "dark-daltonized");
        assert_eq!(ThemeSetting::LightDaltonized.id(), "light-daltonized");
        assert_eq!(ThemeSetting::DarkAnsi.id(), "dark-ansi");
        assert_eq!(ThemeSetting::LightAnsi.id(), "light-ansi");
    }

    #[test]
    fn theme_setting_labels() {
        assert_eq!(ThemeSetting::Auto.label(), "Auto (match terminal)");
        assert_eq!(ThemeSetting::Dark.label(), "Dark mode");
        assert_eq!(ThemeSetting::Light.label(), "Light mode");
        assert_eq!(
            ThemeSetting::DarkDaltonized.label(),
            "Dark mode (colorblind-friendly)"
        );
        assert_eq!(
            ThemeSetting::LightDaltonized.label(),
            "Light mode (colorblind-friendly)"
        );
        assert_eq!(
            ThemeSetting::DarkAnsi.label(),
            "Dark mode (ANSI colors only)"
        );
        assert_eq!(
            ThemeSetting::LightAnsi.label(),
            "Light mode (ANSI colors only)"
        );
    }

    #[test]
    fn build_options_with_auto() {
        let opts = build_options(true);
        assert_eq!(opts.len(), 7);
        assert_eq!(opts[0].value, ThemeSetting::Auto);
        assert_eq!(opts[1].value, ThemeSetting::Dark);
        assert_eq!(opts[6].value, ThemeSetting::LightAnsi);
    }

    #[test]
    fn build_options_without_auto() {
        let opts = build_options(false);
        assert_eq!(opts.len(), 6);
        assert_eq!(opts[0].value, ThemeSetting::Dark);
        assert_eq!(opts[5].value, ThemeSetting::LightAnsi);
    }

    #[test]
    fn on_focus_returns_preview() {
        assert_eq!(
            on_focus(ThemeSetting::Dark),
            ThemePickerAction::PreviewTheme(ThemeSetting::Dark)
        );
    }

    #[test]
    fn on_select_saves() {
        assert_eq!(
            on_select(ThemeSetting::Light),
            ThemePickerAction::SaveAndSelect(ThemeSetting::Light)
        );
    }

    #[test]
    fn on_cancel_skip_exit_does_not_exit() {
        assert_eq!(
            on_cancel(true),
            ThemePickerAction::CancelPreview { exit: false }
        );
    }

    #[test]
    fn on_cancel_default_exits() {
        assert_eq!(
            on_cancel(false),
            ThemePickerAction::CancelPreview { exit: true }
        );
    }

    #[test]
    fn demo_diff_path_pinned() {
        assert_eq!(DEMO_DIFF_PATH, "demo.js");
    }
}
