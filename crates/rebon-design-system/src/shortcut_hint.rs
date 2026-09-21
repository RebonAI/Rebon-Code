/// Whether the hint is wrapped in parentheses by default.
pub const DEFAULT_PARENS: bool = false;
/// Whether the shortcut span is emphasised by default.
pub const DEFAULT_BOLD: bool = false;

/// Literal joining the shortcut to its action: `{shortcut} to {action}`.
pub const CONNECTOR: &str = " to ";

/// Platform whose modifier names are used when a shortcut is formatted for
/// display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutPlatform {
    /// macOS: `Cmd`, `Option`, `Ctrl`.
    Mac,
    /// Linux and other Unix-like systems.
    Linux,
    /// Windows.
    Windows,
}

impl ShortcutPlatform {
    /// The platform this binary was compiled for. Anything that is neither
    /// macOS nor Windows counts as [`ShortcutPlatform::Linux`].
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// Format a canonical shortcut for display on the platform this binary
/// targets.
pub fn format_shortcut_for_current_platform(shortcut: &str) -> String {
    format_shortcut_for_platform(shortcut, ShortcutPlatform::current())
}

/// Like [`format_shortcut_for_current_platform`], but with spaces around the
/// `+` separators.
pub fn format_shortcut_spaced_for_current_platform(shortcut: &str) -> String {
    format_shortcut_spaced_for_platform(shortcut, ShortcutPlatform::current())
}

/// Format a canonical shortcut for display on one specific platform.
pub fn format_shortcut_for_platform(shortcut: &str, platform: ShortcutPlatform) -> String {
    format_shortcut_with_separator(shortcut, platform, "+")
}

/// Format a canonical shortcut for one specific platform, with spaces
/// around the `+` separators.
pub fn format_shortcut_spaced_for_platform(shortcut: &str, platform: ShortcutPlatform) -> String {
    format_shortcut_with_separator(shortcut, platform, " + ")
}

fn format_shortcut_with_separator(
    shortcut: &str,
    platform: ShortcutPlatform,
    separator: &str,
) -> String {
    let shortcut = normalize_plus_spacing(shortcut);
    let shortcut = normalize_modifier_hyphens(&shortcut);
    shortcut
        .split_whitespace()
        .map(|chord| format_shortcut_chord(chord, platform, separator))
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_modifier_hyphens(shortcut: &str) -> String {
    shortcut
        .split_whitespace()
        .map(|chord| {
            let Some((modifier, _)) = chord.split_once('-') else {
                return chord.to_string();
            };
            match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "cmd" | "command" | "alt" | "option" | "shift" | "meta"
                | "super" => chord.replace('-', "+"),
                _ => chord.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_plus_spacing(shortcut: &str) -> String {
    let chars = shortcut.chars().collect::<Vec<_>>();
    let mut normalized = String::with_capacity(shortcut.len());
    for (index, ch) in chars.iter().copied().enumerate() {
        if ch.is_whitespace() {
            let prev = chars[..index]
                .iter()
                .rev()
                .find(|candidate| !candidate.is_whitespace());
            let next = chars[index + 1..]
                .iter()
                .find(|candidate| !candidate.is_whitespace());
            if prev == Some(&'+') || next == Some(&'+') {
                continue;
            }
        }
        normalized.push(ch);
    }
    normalized
}

fn format_shortcut_chord(chord: &str, platform: ShortcutPlatform, separator: &str) -> String {
    chord
        .split('+')
        .map(|part| format_shortcut_part(part.trim(), platform))
        .collect::<Vec<_>>()
        .join(separator)
}

fn format_shortcut_part(part: &str, platform: ShortcutPlatform) -> String {
    if part.contains('/') {
        return part
            .split('/')
            .map(|alternative| format_shortcut_atom(alternative.trim(), platform))
            .collect::<Vec<_>>()
            .join("/");
    }
    format_shortcut_atom(part, platform)
}

fn format_shortcut_atom(part: &str, platform: ShortcutPlatform) -> String {
    if part.is_empty() {
        return String::new();
    }

    if let Some((key, suffix)) = part.split_once(" (") {
        let suffix = format!(" ({suffix}");
        return format!("{}{}", format_shortcut_atom(key.trim(), platform), suffix);
    }

    let lower = part.to_ascii_lowercase();
    match lower.as_str() {
        "ctrl" | "control" => "Ctrl".to_string(),
        "cmd" | "command" => match platform {
            ShortcutPlatform::Mac => "Cmd".to_string(),
            ShortcutPlatform::Linux | ShortcutPlatform::Windows => "Ctrl".to_string(),
        },
        "meta" | "super" => match platform {
            ShortcutPlatform::Mac => "Cmd".to_string(),
            ShortcutPlatform::Linux => "Meta".to_string(),
            ShortcutPlatform::Windows => "Win".to_string(),
        },
        "alt" => match platform {
            ShortcutPlatform::Mac => "Option".to_string(),
            ShortcutPlatform::Linux | ShortcutPlatform::Windows => "Alt".to_string(),
        },
        "option" => match platform {
            ShortcutPlatform::Mac => "Option".to_string(),
            ShortcutPlatform::Linux | ShortcutPlatform::Windows => "Alt".to_string(),
        },
        "shift" => "Shift".to_string(),
        "esc" | "escape" => "Esc".to_string(),
        "enter" => "Enter".to_string(),
        "return" => "Return".to_string(),
        "tab" => "Tab".to_string(),
        "space" => "Space".to_string(),
        "backspace" => "Backspace".to_string(),
        "delete" | "del" => "Delete".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" | "page_up" | "page-up" => "PageUp".to_string(),
        "pagedown" | "page_down" | "page-down" => "PageDown".to_string(),
        _ if lower.len() == 1 && lower.as_bytes()[0].is_ascii_alphabetic() => {
            lower.to_ascii_uppercase()
        }
        _ if is_function_key(&lower) => lower.to_ascii_uppercase(),
        _ => part.to_string(),
    }
}

fn is_function_key(part: &str) -> bool {
    part.len() >= 2 && part.starts_with('f') && part[1..].chars().all(|ch| ch.is_ascii_digit())
}

/// Output of [`format_shortcut_hint`], split so a renderer can style the
/// shortcut span by itself.
///
/// `before`, `shortcut` and `after` concatenate to the full hint, which is
/// exactly what `plain_text` holds for renderers that apply no styling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutHint {
    /// Text before the shortcut span: `""` or `"("`.
    pub before: String,
    /// The formatted shortcut itself.
    pub shortcut: String,
    /// Text after the shortcut span: `" to <action>"` or
    /// `" to <action>)"`.
    pub after: String,
    /// Whether the shortcut span should be emphasised.
    pub bold: bool,
    /// The whole hint as one unstyled string, i.e. `before + shortcut +
    /// after`.
    pub plain_text: String,
}

/// Format a keyboard-shortcut hint, normalizing the shortcut text for the
/// platform this binary targets.
///
/// * `shortcut` — the canonical key or chord, e.g. `"ctrl+o"`, `"Enter"`,
///   `"↑/↓"`.
/// * `action` — what the key does, e.g. `"expand"`, `"select"`.
/// * `parens` — wrap the whole hint in parentheses.
/// * `bold` — mark the shortcut span for emphasis.
///
/// Neither flag has an implicit default; [`DEFAULT_PARENS`] and
/// [`DEFAULT_BOLD`] are what callers that want the plain form pass.
pub fn format_shortcut_hint(
    shortcut: &str,
    action: &str,
    parens: bool,
    bold: bool,
) -> ShortcutHint {
    format_shortcut_hint_for_platform(shortcut, action, parens, bold, ShortcutPlatform::current())
}

/// Format a keyboard-shortcut hint for one specific platform.
pub fn format_shortcut_hint_for_platform(
    shortcut: &str,
    action: &str,
    parens: bool,
    bold: bool,
    platform: ShortcutPlatform,
) -> ShortcutHint {
    let (before, after) = if parens {
        ("(".to_string(), format!("{CONNECTOR}{action})"))
    } else {
        (String::new(), format!("{CONNECTOR}{action}"))
    };
    let display_shortcut = format_shortcut_for_platform(shortcut, platform);
    let plain_text = format!("{before}{display_shortcut}{after}");
    ShortcutHint {
        before,
        shortcut: display_shortcut,
        after,
        bold,
        plain_text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_are_pinned() {
        assert_eq!(DEFAULT_PARENS, false);
        assert_eq!(DEFAULT_BOLD, false);
    }

    #[test]
    fn connector_is_pinned() {
        assert_eq!(CONNECTOR, " to ");
    }

    #[test]
    fn no_parens_no_bold_basic_form() {
        let h = format_shortcut_hint("esc", "cancel", false, false);
        assert_eq!(h.before, "");
        assert_eq!(h.shortcut, "Esc");
        assert_eq!(h.after, " to cancel");
        assert_eq!(h.bold, false);
        assert_eq!(h.plain_text, "Esc to cancel");
    }

    #[test]
    fn parens_form_wraps_in_parentheses() {
        let h = format_shortcut_hint("ctrl+o", "expand", true, false);
        assert_eq!(h.before, "(");
        assert_eq!(h.shortcut, "Ctrl+O");
        assert_eq!(h.after, " to expand)");
        assert_eq!(h.plain_text, "(Ctrl+O to expand)");
    }

    #[test]
    fn bold_only_affects_shortcut_field() {
        let h = format_shortcut_hint("Enter", "confirm", false, true);
        assert_eq!(h.bold, true);
        assert_eq!(h.shortcut, "Enter");
        // before and after must NOT be bold
        assert_eq!(h.before, "");
        assert_eq!(h.after, " to confirm");
    }

    #[test]
    fn bold_with_parens_combination() {
        let h = format_shortcut_hint("Enter", "confirm", true, true);
        assert_eq!(h.bold, true);
        assert_eq!(h.before, "(");
        assert_eq!(h.after, " to confirm)");
        assert_eq!(h.plain_text, "(Enter to confirm)");
    }

    #[test]
    fn empty_shortcut_does_not_crash() {
        let h = format_shortcut_hint("", "exit", false, false);
        assert_eq!(h.plain_text, " to exit");
    }

    #[test]
    fn empty_action_does_not_crash() {
        let h = format_shortcut_hint("esc", "", false, false);
        assert_eq!(h.plain_text, "Esc to ");
    }

    #[test]
    fn canonical_shortcuts_are_platform_formatted() {
        assert_eq!(
            format_shortcut_hint_for_platform(
                "ctrl+o",
                "expand",
                true,
                false,
                ShortcutPlatform::Linux
            )
            .plain_text,
            "(Ctrl+O to expand)"
        );
        assert_eq!(
            format_shortcut_hint_for_platform("cmd+o", "open", false, false, ShortcutPlatform::Mac)
                .plain_text,
            "Cmd+O to open"
        );
        assert_eq!(
            format_shortcut_hint_for_platform(
                "cmd+o",
                "open",
                false,
                false,
                ShortcutPlatform::Windows
            )
            .plain_text,
            "Ctrl+O to open"
        );
    }

    #[test]
    fn spaced_shortcuts_normalize_existing_plus_spacing() {
        assert_eq!(
            format_shortcut_spaced_for_platform("ctrl + z", ShortcutPlatform::Linux),
            "Ctrl + Z"
        );
        assert_eq!(
            format_shortcut_for_platform("Ctrl-D", ShortcutPlatform::Linux),
            "Ctrl+D"
        );
        assert_eq!(
            format_shortcut_spaced_for_platform(
                "backslash (\\)+return (return)",
                ShortcutPlatform::Linux
            ),
            "backslash (\\) + Return (return)"
        );
    }

    #[test]
    fn arrow_glyph_shortcut_round_trips() {
        // Arrow chords pass through unchanged — they have no modifier or
        // atom to normalize.
        let h = format_shortcut_hint("↑/↓", "navigate", false, false);
        assert_eq!(h.plain_text, "↑/↓ to navigate");
    }

    #[test]
    fn current_platform_shortcuts_are_formatted() {
        let cases: &[(&str, &str, bool, bool, &str)] = &[
            ("esc", "cancel", false, false, "Esc to cancel"),
            ("ctrl+o", "expand", true, false, "(Ctrl+O to expand)"),
            ("Enter", "confirm", false, true, "Enter to confirm"),
            ("Enter", "confirm", false, false, "Enter to confirm"),
            ("Esc", "cancel", false, false, "Esc to cancel"),
        ];
        for (s, a, p, b, expected) in cases {
            let h = format_shortcut_hint(s, a, *p, *b);
            assert_eq!(h.plain_text, *expected, "case ({s}, {a}, {p}, {b})");
        }
    }

    #[test]
    fn parens_does_not_double_wrap() {
        // Calling twice should not nest parentheses.
        let h = format_shortcut_hint("a", "b", true, false);
        assert_eq!(h.plain_text.matches('(').count(), 1);
        assert_eq!(h.plain_text.matches(')').count(), 1);
    }
}
