//! Configuration-issues panel for keybindings.
//!
//! The consumer feeds the cached keybinding warnings together with the
//! customization gate; the layout splits the entries into errors and
//! warnings by `severity`, picks the header color based on whether any
//! errors are present, and emits one row per entry with an optional
//! `→ suggestion` sub-row.
//!
//! Pure logic implemented here:
//!
//! 1. The two-state severity tag.
//! 2. The error/warning split that preserves input order.
//! 3. The header color decision (error color when at least one error is
//!    present, warning color otherwise).
//! 4. The visibility gate (hidden when disabled or empty).
//! 5. The path label projection (the consumer feeds the resolved
//!    keybindings path).

/// Severity tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeybindingWarningSeverity {
    /// `'error'` — a hard validation failure.
    Error,
    /// `'warning'` — a soft validation issue.
    Warning,
}

/// One validation entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingWarning {
    /// Severity tag.
    pub severity: KeybindingWarningSeverity,
    /// Human-readable message.
    pub message: String,
    /// Optional remediation suggestion. When present, the row gets a
    /// `→ <suggestion>` sub-row.
    pub suggestion: Option<String>,
}

/// One rendered row in the warnings layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingWarningsRow {
    /// Severity of this row.
    pub severity: KeybindingWarningSeverity,
    /// `[Error]` or `[Warning]` literal.
    pub label: &'static str,
    /// Message text.
    pub message: String,
    /// Optional suggestion, already prefixed with `→ `.
    pub suggestion: Option<String>,
}

/// Theme color for the panel header: the error color when at least one
/// error is present, the warning color otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeybindingHeaderColor {
    /// `'error'` — used when at least one error is present.
    Error,
    /// `'warning'` — used when only warnings are present.
    Warning,
}

/// Top-level layout of the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingWarningsLayout {
    /// Header text. Pinned: `"Keybinding Configuration Issues"`.
    pub header: &'static str,
    /// Header theme color.
    pub header_color: KeybindingHeaderColor,
    /// Path label, e.g. `"Location: /home/user/.claude/keybindings.json"`.
    pub location: String,
    /// Errors first (preserving input order), then warnings.
    pub rows: Vec<KeybindingWarningsRow>,
}

/// Build the panel layout. Returns `None` when the panel should be
/// hidden (gate disabled, no warnings).
pub fn keybinding_warnings_layout(
    is_customization_enabled: bool,
    warnings: &[KeybindingWarning],
    keybindings_path: &str,
) -> Option<KeybindingWarningsLayout> {
    if !is_customization_enabled {
        return None;
    }
    if warnings.is_empty() {
        return None;
    }

    let mut errors = Vec::new();
    let mut warns = Vec::new();
    for w in warnings {
        match w.severity {
            KeybindingWarningSeverity::Error => errors.push(w),
            KeybindingWarningSeverity::Warning => warns.push(w),
        }
    }

    let header_color = if !errors.is_empty() {
        KeybindingHeaderColor::Error
    } else {
        KeybindingHeaderColor::Warning
    };

    let mut rows = Vec::with_capacity(warnings.len());
    for w in errors.into_iter().chain(warns.into_iter()) {
        rows.push(KeybindingWarningsRow {
            severity: w.severity,
            label: match w.severity {
                KeybindingWarningSeverity::Error => "[Error]",
                KeybindingWarningSeverity::Warning => "[Warning]",
            },
            message: w.message.clone(),
            suggestion: w.suggestion.as_ref().map(|s| format!("\u{2192} {s}")),
        });
    }

    Some(KeybindingWarningsLayout {
        header: "Keybinding Configuration Issues",
        header_color,
        location: format!("Location: {keybindings_path}"),
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(msg: &str, sug: Option<&str>) -> KeybindingWarning {
        KeybindingWarning {
            severity: KeybindingWarningSeverity::Error,
            message: msg.into(),
            suggestion: sug.map(Into::into),
        }
    }

    fn warn(msg: &str, sug: Option<&str>) -> KeybindingWarning {
        KeybindingWarning {
            severity: KeybindingWarningSeverity::Warning,
            message: msg.into(),
            suggestion: sug.map(Into::into),
        }
    }

    #[test]
    fn hidden_when_customization_disabled() {
        let warnings = vec![err("bad", None)];
        assert_eq!(keybinding_warnings_layout(false, &warnings, "/path"), None);
    }

    #[test]
    fn hidden_when_no_warnings() {
        assert_eq!(keybinding_warnings_layout(true, &[], "/path"), None);
    }

    #[test]
    fn header_is_error_color_when_any_errors() {
        let warnings = vec![err("bad", None), warn("soft", None)];
        let layout = keybinding_warnings_layout(true, &warnings, "/path").unwrap();
        assert_eq!(layout.header_color, KeybindingHeaderColor::Error);
    }

    #[test]
    fn header_is_warning_color_when_only_warnings() {
        let warnings = vec![warn("soft", None)];
        let layout = keybinding_warnings_layout(true, &warnings, "/path").unwrap();
        assert_eq!(layout.header_color, KeybindingHeaderColor::Warning);
    }

    #[test]
    fn header_text_is_pinned() {
        let warnings = vec![err("x", None)];
        let layout = keybinding_warnings_layout(true, &warnings, "/path").unwrap();
        assert_eq!(layout.header, "Keybinding Configuration Issues");
    }

    #[test]
    fn location_includes_path() {
        let warnings = vec![err("x", None)];
        let layout = keybinding_warnings_layout(true, &warnings, "/home/user/keys.json").unwrap();
        assert_eq!(layout.location, "Location: /home/user/keys.json");
    }

    #[test]
    fn errors_appear_before_warnings_regardless_of_input_order() {
        let warnings = vec![
            warn("w1", None),
            err("e1", None),
            warn("w2", None),
            err("e2", None),
        ];
        let layout = keybinding_warnings_layout(true, &warnings, "/p").unwrap();
        let labels: Vec<_> = layout.rows.iter().map(|r| r.label).collect();
        assert_eq!(labels, vec!["[Error]", "[Error]", "[Warning]", "[Warning]"]);
    }

    #[test]
    fn relative_order_within_severity_is_preserved() {
        let warnings = vec![err("first", None), err("second", None)];
        let layout = keybinding_warnings_layout(true, &warnings, "/p").unwrap();
        assert_eq!(layout.rows[0].message, "first");
        assert_eq!(layout.rows[1].message, "second");
    }

    #[test]
    fn suggestion_gets_arrow_prefix() {
        let warnings = vec![err("bad", Some("try this"))];
        let layout = keybinding_warnings_layout(true, &warnings, "/p").unwrap();
        assert_eq!(
            layout.rows[0].suggestion.as_deref(),
            Some("\u{2192} try this")
        );
    }

    #[test]
    fn missing_suggestion_stays_none() {
        let warnings = vec![err("bad", None)];
        let layout = keybinding_warnings_layout(true, &warnings, "/p").unwrap();
        assert_eq!(layout.rows[0].suggestion, None);
    }

    #[test]
    fn label_text_is_expected() {
        let warnings = vec![err("e", None), warn("w", None)];
        let layout = keybinding_warnings_layout(true, &warnings, "/p").unwrap();
        assert_eq!(layout.rows[0].label, "[Error]");
        assert_eq!(layout.rows[1].label, "[Warning]");
    }

    #[test]
    fn row_severity_matches_label() {
        let warnings = vec![err("e", None), warn("w", None)];
        let layout = keybinding_warnings_layout(true, &warnings, "/p").unwrap();
        assert_eq!(layout.rows[0].severity, KeybindingWarningSeverity::Error);
        assert_eq!(layout.rows[1].severity, KeybindingWarningSeverity::Warning);
    }
}
