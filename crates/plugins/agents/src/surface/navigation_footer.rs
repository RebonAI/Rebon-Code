//! Agents-menu navigation footer — pinned default footer text and the
//! exit-on-ctrl-c overlay rule.
//!
//! Footer labels and keybinding policy decisions for navigation controls.
//! The footer renders one line of dim text. When the user has pressed
//! Ctrl+C once, the line is replaced with `Press <key> again to exit`;
//! otherwise the supplied (or default) instructions are shown.

/// The default instructions string used by the agents-menu surface
/// when the consumer doesn't pass `instructions`.
pub const DEFAULT_NAVIGATION_INSTRUCTIONS: &str =
    "Press \u{2191}\u{2193} to navigate \u{00B7} Enter to select \u{00B7} Esc to go back";

/// The exit-pending overlay state for Ctrl-C / Ctrl-D. This module
/// doesn't track the lifecycle — the consumer hands the current state
/// in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitPending {
    /// True if Ctrl+C / Ctrl+D has been pressed once and the user
    /// must press it again to exit.
    pub pending: bool,
    /// Display name of the key to press again (e.g. `Ctrl+C`).
    pub key_name: String,
}

/// Build the line of footer text shown to the user.
pub fn footer_text(instructions: Option<&str>, exit: &ExitPending) -> String {
    if exit.pending {
        format!("Press {} again to exit", format_shortcut(&exit.key_name))
    } else {
        instructions
            .unwrap_or(DEFAULT_NAVIGATION_INSTRUCTIONS)
            .to_string()
    }
}

fn format_shortcut(shortcut: &str) -> String {
    shortcut
        .split_whitespace()
        .map(|chord| {
            let normalized = normalize_modifier_hyphen(chord);
            normalized
                .split('+')
                .map(format_shortcut_part)
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_modifier_hyphen(chord: &str) -> String {
    let Some((modifier, _)) = chord.split_once('-') else {
        return chord.to_string();
    };
    match modifier.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "cmd" | "command" | "alt" | "option" | "shift" | "meta" | "super" => {
            chord.replace('-', "+")
        }
        _ => chord.to_string(),
    }
}

fn format_shortcut_part(part: &str) -> String {
    let lower = part.to_ascii_lowercase();
    match lower.as_str() {
        "ctrl" | "control" => "Ctrl".to_string(),
        "shift" => "Shift".to_string(),
        "alt" | "option" => "Alt".to_string(),
        "cmd" | "command" => "Cmd".to_string(),
        "meta" | "super" => "Meta".to_string(),
        "tab" => "Tab".to_string(),
        "esc" | "escape" => "Esc".to_string(),
        "return" => "Return".to_string(),
        "enter" => "Enter".to_string(),
        _ if lower.len() == 1 && lower.as_bytes()[0].is_ascii_alphabetic() => {
            lower.to_ascii_uppercase()
        }
        _ => part.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_instructions_pinned() {
        assert!(DEFAULT_NAVIGATION_INSTRUCTIONS.contains("navigate"));
        assert!(DEFAULT_NAVIGATION_INSTRUCTIONS.contains("select"));
        assert!(DEFAULT_NAVIGATION_INSTRUCTIONS.contains("go back"));
    }

    #[test]
    fn footer_uses_default_when_none() {
        let exit = ExitPending {
            pending: false,
            key_name: "Ctrl+C".into(),
        };
        assert_eq!(footer_text(None, &exit), DEFAULT_NAVIGATION_INSTRUCTIONS);
    }

    #[test]
    fn footer_uses_supplied_instructions() {
        let exit = ExitPending {
            pending: false,
            key_name: "Ctrl+C".into(),
        };
        assert_eq!(footer_text(Some("custom"), &exit), "custom");
    }

    #[test]
    fn footer_overrides_to_exit_when_pending() {
        let exit = ExitPending {
            pending: true,
            key_name: "Ctrl+C".into(),
        };
        assert_eq!(
            footer_text(Some("ignored"), &exit),
            "Press Ctrl+C again to exit"
        );
    }

    #[test]
    fn footer_overrides_with_ctrl_d() {
        let exit = ExitPending {
            pending: true,
            key_name: "Ctrl+D".into(),
        };
        assert_eq!(footer_text(None, &exit), "Press Ctrl+D again to exit");
    }
}
