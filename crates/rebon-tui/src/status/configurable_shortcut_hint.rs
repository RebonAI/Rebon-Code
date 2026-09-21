//! Pass-through to [`KeyboardShortcutHint`] with a resolved
//! shortcut.
//!
//! The consumer looks up the user-configured shortcut for a keybinding
//! action, its context, and a fallback value, and feeds the resolved
//! text in together with the action description, the parens flag, and
//! the bold flag.
//!
//! Pure logic implemented here:
//!
//! 1. The shortcut normalization (the consumer pre-resolves the
//!    configured binding and feeds the result).
//! 2. The pass-through to the shortcut-hint fields.

use super::shortcut_hint::KeyboardShortcutHint;

/// Inputs to the pass-through. `resolved_shortcut` is the shortcut text
/// the consumer resolved for the action, its context, and its fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurableShortcutHintInputs {
    /// Resolved shortcut text.
    pub resolved_shortcut: String,
    /// Action description, e.g. `"expand"`.
    pub description: String,
    /// Whether to wrap in parentheses.
    pub parens: Option<bool>,
    /// Whether to render in bold.
    pub bold: Option<bool>,
}

/// Build the [`KeyboardShortcutHint`] for the resolved shortcut
/// and action.
pub fn configurable_shortcut_hint(inputs: ConfigurableShortcutHintInputs) -> KeyboardShortcutHint {
    KeyboardShortcutHint {
        shortcut: format_shortcut(&inputs.resolved_shortcut),
        action: inputs.description,
        parens: inputs.parens,
        bold: inputs.bold,
    }
}

fn format_shortcut(shortcut: &str) -> String {
    shortcut
        .split_whitespace()
        .map(|chord| {
            chord
                .split('+')
                .map(format_shortcut_part)
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    fn passes_shortcut_and_description_through() {
        let hint = configurable_shortcut_hint(ConfigurableShortcutHintInputs {
            resolved_shortcut: "ctrl+o".into(),
            description: "expand".into(),
            parens: None,
            bold: None,
        });
        assert_eq!(hint.shortcut, "Ctrl+O");
        assert_eq!(hint.action, "expand");
    }

    #[test]
    fn parens_and_bold_default_to_none() {
        let hint = configurable_shortcut_hint(ConfigurableShortcutHintInputs {
            resolved_shortcut: "ctrl+t".into(),
            description: "toggle".into(),
            parens: None,
            bold: None,
        });
        assert_eq!(hint.parens, None);
        assert_eq!(hint.bold, None);
    }

    #[test]
    fn parens_true_is_preserved() {
        let hint = configurable_shortcut_hint(ConfigurableShortcutHintInputs {
            resolved_shortcut: "ctrl+t".into(),
            description: "toggle".into(),
            parens: Some(true),
            bold: None,
        });
        assert_eq!(hint.parens, Some(true));
    }

    #[test]
    fn bold_true_is_preserved() {
        let hint = configurable_shortcut_hint(ConfigurableShortcutHintInputs {
            resolved_shortcut: "ctrl+t".into(),
            description: "toggle".into(),
            parens: None,
            bold: Some(true),
        });
        assert_eq!(hint.bold, Some(true));
    }

    #[test]
    fn parens_false_is_distinct_from_none() {
        let hint = configurable_shortcut_hint(ConfigurableShortcutHintInputs {
            resolved_shortcut: "ctrl+t".into(),
            description: "toggle".into(),
            parens: Some(false),
            bold: None,
        });
        assert_eq!(hint.parens, Some(false));
    }
}
