//! The prompt-input help menu.
//!
//! This module renders three fixed columns of help text, keeping the
//! row content and gating rules but returning the menu as plain strings.

use rebon_design_system::format_shortcut_spaced_for_current_platform;

/// Pure inputs needed to build the help menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpMenuInput {
    /// Give every column the same fixed width.
    pub fixed_width: bool,
    /// Gap between columns.
    pub gap: Option<usize>,
    /// Horizontal padding inside the container.
    pub padding_x: Option<usize>,
    /// Configured transcript shortcut.
    pub transcript_shortcut: String,
    /// Configured todos shortcut.
    pub todos_shortcut: String,
    /// Configured undo shortcut.
    pub undo_shortcut: String,
    /// Configured stash shortcut.
    pub stash_shortcut: String,
    /// Configured cycle-mode shortcut.
    pub cycle_mode_shortcut: String,
    /// Configured model-picker shortcut.
    pub model_picker_shortcut: String,
    /// Configured fast-mode shortcut.
    pub fast_mode_shortcut: String,
    /// Configured external-editor shortcut.
    pub external_editor_shortcut: String,
    /// Configured terminal shortcut.
    pub terminal_shortcut: String,
    /// Configured image-paste shortcut.
    pub image_paste_shortcut: String,
    /// Newline hint text for the current platform.
    pub newline_instructions: String,
    /// Compile-time terminal-panel feature gate.
    pub terminal_panel_feature_enabled: bool,
    /// Runtime rollout gate for terminal panel.
    pub terminal_panel_rollout_enabled: bool,
    /// Whether fast mode is enabled.
    pub fast_mode_enabled: bool,
    /// Whether fast mode is available.
    pub fast_mode_available: bool,
    /// Whether keybinding customization is enabled.
    pub keybinding_customization_enabled: bool,
    /// True when the current platform is Windows.
    pub is_windows: bool,
    /// Compile-time internal build flag.
    pub internal_build: bool,
}

/// One visual column in the help menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpMenuColumn {
    /// Fixed width when `fixed_width` is on.
    pub width: Option<usize>,
    /// Visible rows in display order.
    pub rows: Vec<String>,
}

/// Plain-data version of the help menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpMenuLayout {
    /// Container gap.
    pub gap: Option<usize>,
    /// Container padding.
    pub padding_x: Option<usize>,
    /// Three rendered columns.
    pub columns: Vec<HelpMenuColumn>,
}

/// Project the inputs into the three-column layout.
pub fn build_help_menu_layout(input: &HelpMenuInput) -> HelpMenuLayout {
    let first_column = HelpMenuColumn {
        width: input.fixed_width.then_some(24),
        rows: vec![
            String::from("! for bash mode"),
            String::from("/ for commands"),
            String::from("@ for file paths"),
            String::from("& for background"),
            String::from("/btw for side question"),
        ],
    };

    let mut second_column_rows = vec![
        String::from("double tap esc to clear input"),
        format!(
            "{} {}",
            format_shortcut_spaced_for_current_platform(&input.cycle_mode_shortcut),
            if input.internal_build {
                "to cycle modes"
            } else {
                "to auto-accept edits"
            }
        ),
        format!(
            "{} for verbose output",
            format_shortcut_spaced_for_current_platform(&input.transcript_shortcut)
        ),
        format!(
            "{} to toggle tasks",
            format_shortcut_spaced_for_current_platform(&input.todos_shortcut)
        ),
    ];
    if input.terminal_panel_feature_enabled && input.terminal_panel_rollout_enabled {
        second_column_rows.push(format!(
            "{} for terminal",
            format_shortcut_spaced_for_current_platform(&input.terminal_shortcut)
        ));
    }
    second_column_rows.push(input.newline_instructions.clone());
    let second_column = HelpMenuColumn {
        width: input.fixed_width.then_some(35),
        rows: second_column_rows,
    };

    let mut third_column_rows = vec![format!(
        "{} to undo",
        format_shortcut_spaced_for_current_platform(&input.undo_shortcut)
    )];
    if !input.is_windows {
        third_column_rows.push(format!(
            "{} to suspend",
            format_shortcut_spaced_for_current_platform("ctrl+z")
        ));
    }
    third_column_rows.push(format!(
        "{} to paste images",
        format_shortcut_spaced_for_current_platform(&input.image_paste_shortcut)
    ));
    third_column_rows.push(format!(
        "{} to switch model",
        format_shortcut_spaced_for_current_platform(&input.model_picker_shortcut)
    ));
    if input.fast_mode_enabled && input.fast_mode_available {
        third_column_rows.push(format!(
            "{} to toggle fast mode",
            format_shortcut_spaced_for_current_platform(&input.fast_mode_shortcut)
        ));
    }
    third_column_rows.push(format!(
        "{} to stash prompt",
        format_shortcut_spaced_for_current_platform(&input.stash_shortcut)
    ));
    third_column_rows.push(format!(
        "{} to edit in $EDITOR",
        format_shortcut_spaced_for_current_platform(&input.external_editor_shortcut)
    ));
    if input.keybinding_customization_enabled {
        third_column_rows.push(String::from("/keybindings to customize"));
    }
    let third_column = HelpMenuColumn {
        width: None,
        rows: third_column_rows,
    };

    HelpMenuLayout {
        gap: input.gap,
        padding_x: input.padding_x,
        columns: vec![first_column, second_column, third_column],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> HelpMenuInput {
        HelpMenuInput {
            fixed_width: true,
            gap: Some(2),
            padding_x: Some(1),
            transcript_shortcut: "ctrl+o".into(),
            todos_shortcut: "ctrl+t".into(),
            undo_shortcut: "ctrl+_".into(),
            stash_shortcut: "ctrl+s".into(),
            cycle_mode_shortcut: "shift+tab".into(),
            model_picker_shortcut: "alt+p".into(),
            fast_mode_shortcut: "alt+o".into(),
            external_editor_shortcut: "ctrl+g".into(),
            terminal_shortcut: "meta+j".into(),
            image_paste_shortcut: "ctrl+v".into(),
            newline_instructions: "Shift + Return for newline".into(),
            terminal_panel_feature_enabled: true,
            terminal_panel_rollout_enabled: true,
            fast_mode_enabled: true,
            fast_mode_available: true,
            keybinding_customization_enabled: true,
            is_windows: false,
            internal_build: false,
        }
    }

    #[test]
    fn fixed_width_columns_are_pinned() {
        let layout = build_help_menu_layout(&base());
        assert_eq!(layout.columns[0].width, Some(24));
        assert_eq!(layout.columns[1].width, Some(35));
        assert_eq!(layout.columns[2].width, None);
    }

    #[test]
    fn shortcut_text_is_spaced_for_display() {
        let layout = build_help_menu_layout(&base());
        assert!(layout.columns[1].rows[1].starts_with("Shift + Tab"));
        assert!(layout.columns[1].rows[2].starts_with("Ctrl + O"));
        assert!(layout.columns[2].rows[0].starts_with("Ctrl + _"));
    }

    #[test]
    fn terminal_row_is_gated_by_feature_and_rollout() {
        let mut input = base();
        input.terminal_panel_rollout_enabled = false;
        let layout = build_help_menu_layout(&input);
        assert!(!layout.columns[1]
            .rows
            .iter()
            .any(|row| row.contains("for terminal")));
    }

    #[test]
    fn windows_hides_suspend_and_fast_mode_row_is_gated() {
        let mut input = base();
        input.is_windows = true;
        input.fast_mode_available = false;
        let layout = build_help_menu_layout(&input);
        assert!(!layout.columns[2]
            .rows
            .iter()
            .any(|row| row == "Ctrl + Z to suspend"));
        assert!(!layout.columns[2]
            .rows
            .iter()
            .any(|row| row.contains("toggle fast mode")));
    }

    #[test]
    fn keybindings_row_and_cycle_text_follow_runtime_flags() {
        let mut input = base();
        input.keybinding_customization_enabled = false;
        input.internal_build = true;
        let layout = build_help_menu_layout(&input);
        assert!(layout.columns[1].rows[1].ends_with("to cycle modes"));
        assert!(!layout.columns[2]
            .rows
            .iter()
            .any(|row| row.contains("/keybindings")));
    }
}
