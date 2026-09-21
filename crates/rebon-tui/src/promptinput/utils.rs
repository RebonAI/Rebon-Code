//! Small prompt-input helpers: cursor clamping, the vim-mode check, the
//! newline hint text, and the non-space printable key test.

use rebon_design_system::format_shortcut_spaced_for_current_platform;

/// Environment/config inputs used by [`get_newline_instructions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewlineInstructionInput {
    /// Terminal program name (e.g. `Apple_Terminal`).
    pub terminal: String,
    /// Whether the host platform is macOS (`'darwin'`).
    pub is_darwin: bool,
    /// Whether a Shift+Enter key binding is installed in the terminal.
    pub shift_enter_key_binding_installed: bool,
    /// Whether the user has already used backslash-return for a newline.
    pub has_used_backslash_return: bool,
}

/// Minimal key projection used by [`is_non_space_printable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyInput {
    /// Ctrl is held.
    pub ctrl: bool,
    /// Meta / Alt is held.
    pub meta: bool,
    /// Escape key.
    pub escape: bool,
    /// Return / Enter key.
    pub return_key: bool,
    /// Tab key.
    pub tab: bool,
    /// Backspace key.
    pub backspace: bool,
    /// Delete key.
    pub delete: bool,
    /// Up arrow.
    pub up_arrow: bool,
    /// Down arrow.
    pub down_arrow: bool,
    /// Left arrow.
    pub left_arrow: bool,
    /// Right arrow.
    pub right_arrow: bool,
    /// Page Up.
    pub page_up: bool,
    /// Page Down.
    pub page_down: bool,
    /// Home.
    pub home: bool,
    /// End.
    pub end: bool,
}

/// Clamp a byte cursor to the nearest UTF-8 character boundary at or before it.
pub fn clamp_cursor_offset(input: &str, requested: usize) -> usize {
    let mut cursor = requested.min(input.len());
    while cursor > 0 && !input.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

/// Whether the configured editor mode is `vim`.
pub fn is_vim_mode_enabled(editor_mode: Option<&str>) -> bool {
    editor_mode == Some("vim")
}

/// The "<shortcut> for newline" hint: Shift+Return on Apple Terminal or when
/// the key binding is installed, otherwise the backslash-return form (short
/// once the user has used it).
pub fn get_newline_instructions(input: &NewlineInstructionInput) -> String {
    if input.terminal == "Apple_Terminal" && input.is_darwin {
        return format!(
            "{} for newline",
            format_shortcut_spaced_for_current_platform("shift+return")
        );
    }
    if input.shift_enter_key_binding_installed {
        return format!(
            "{} for newline",
            format_shortcut_spaced_for_current_platform("shift+return")
        );
    }
    if input.has_used_backslash_return {
        format!(
            "{} for newline",
            format_shortcut_spaced_for_current_platform("\\+return")
        )
    } else {
        format!(
            "{} for newline",
            format_shortcut_spaced_for_current_platform("backslash (\\)+return (return)")
        )
    }
}

/// Whether a key press inserts printable text that does not start with
/// whitespace or an escape: no modifier, navigation, or editing key is set.
pub fn is_non_space_printable(input: &str, key: KeyInput) -> bool {
    if key.ctrl
        || key.meta
        || key.escape
        || key.return_key
        || key.tab
        || key.backspace
        || key.delete
        || key.up_arrow
        || key.down_arrow
        || key.left_arrow
        || key.right_arrow
        || key.page_up
        || key.page_down
        || key.home
        || key.end
    {
        return false;
    }

    !input.is_empty()
        && !input.chars().next().is_some_and(char::is_whitespace)
        && !input.starts_with('\u{1b}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_offset_clamps_past_end() {
        assert_eq!(clamp_cursor_offset("abc", usize::MAX), 3);
        assert_eq!(clamp_cursor_offset("", 7), 0);
    }

    #[test]
    fn cursor_offset_snaps_to_previous_utf8_boundary() {
        assert_eq!(clamp_cursor_offset("你a", 1), 0);
        assert_eq!(clamp_cursor_offset("你a", 2), 0);
        assert_eq!(clamp_cursor_offset("你a", 3), 3);
        assert_eq!(clamp_cursor_offset("你a", 4), 4);
    }

    #[test]
    fn vim_mode_enabled_only_for_vim() {
        assert!(is_vim_mode_enabled(Some("vim")));
        assert!(!is_vim_mode_enabled(Some("emacs")));
        assert!(!is_vim_mode_enabled(None));
    }

    #[test]
    fn newline_instructions_prefers_apple_terminal_path() {
        let input = NewlineInstructionInput {
            terminal: "Apple_Terminal".into(),
            is_darwin: true,
            shift_enter_key_binding_installed: false,
            has_used_backslash_return: false,
        };
        assert_eq!(
            get_newline_instructions(&input),
            "Shift + Return for newline"
        );
    }

    #[test]
    fn newline_instructions_uses_shift_enter_when_installed() {
        let input = NewlineInstructionInput {
            terminal: "iTerm2".into(),
            is_darwin: false,
            shift_enter_key_binding_installed: true,
            has_used_backslash_return: false,
        };
        assert_eq!(
            get_newline_instructions(&input),
            "Shift + Return for newline"
        );
    }

    #[test]
    fn newline_instructions_uses_backslash_history_if_seen() {
        let input = NewlineInstructionInput {
            terminal: "xterm".into(),
            is_darwin: false,
            shift_enter_key_binding_installed: false,
            has_used_backslash_return: true,
        };
        assert_eq!(get_newline_instructions(&input), "\\ + Return for newline");
    }

    #[test]
    fn newline_instructions_falls_back_to_long_form() {
        let input = NewlineInstructionInput {
            terminal: "xterm".into(),
            is_darwin: false,
            shift_enter_key_binding_installed: false,
            has_used_backslash_return: false,
        };
        assert_eq!(
            get_newline_instructions(&input),
            "backslash (\\) + Return (return) for newline"
        );
    }

    #[test]
    fn non_space_printable_rejects_control_navigation_and_whitespace() {
        assert!(!is_non_space_printable(
            "x",
            KeyInput {
                ctrl: true,
                ..KeyInput::default()
            }
        ));
        assert!(!is_non_space_printable(" ", KeyInput::default()));
        assert!(!is_non_space_printable("\u{1b}[A", KeyInput::default()));
        assert!(!is_non_space_printable("", KeyInput::default()));
    }

    #[test]
    fn non_space_printable_accepts_plain_printable_input() {
        assert!(is_non_space_printable("a", KeyInput::default()));
        assert!(is_non_space_printable("/", KeyInput::default()));
    }
}
