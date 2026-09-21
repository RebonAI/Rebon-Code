//! Local copy of the keyboard-shortcut hint data shape.
//!
//! The design-system layer owns the hint widget itself and exposes its
//! formatter (its own `ShortcutHint` output), not this field shape, so the
//! shape is owned here as a plain struct. This crate builds its inputs for
//! the configurable shortcut hint and the session-background hint. The
//! consumer routes it back into the design-system layer when rendering.

/// The shortcut-hint fields, pinned as a pure data shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardShortcutHint {
    /// The shortcut text, e.g. `"ctrl+b"`.
    pub shortcut: String,
    /// The action description, e.g. `"background"`.
    pub action: String,
    /// Whether to wrap the shortcut in parentheses.
    pub parens: Option<bool>,
    /// Whether to render the row in bold.
    pub bold: Option<bool>,
}

impl KeyboardShortcutHint {
    /// Convenience constructor: just shortcut + action, no parens, no
    /// bold.
    pub fn new(shortcut: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            shortcut: shortcut.into(),
            action: action.into(),
            parens: None,
            bold: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_constructor_defaults() {
        let p = KeyboardShortcutHint::new("ctrl+b", "background");
        assert_eq!(p.shortcut, "ctrl+b");
        assert_eq!(p.action, "background");
        assert_eq!(p.parens, None);
        assert_eq!(p.bold, None);
    }

    #[test]
    fn explicit_parens_and_bold_are_preserved() {
        let p = KeyboardShortcutHint {
            shortcut: "ctrl+o".into(),
            action: "expand".into(),
            parens: Some(true),
            bold: Some(true),
        };
        assert_eq!(p.parens, Some(true));
        assert_eq!(p.bold, Some(true));
    }
}
