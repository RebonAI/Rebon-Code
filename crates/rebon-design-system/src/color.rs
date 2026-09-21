use crate::theme::{get_theme, ThemeName};

/// Which part of a cell a color applies to, so callers can route to the
/// right colorizer method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorType {
    /// Text color. The default.
    Foreground,
    /// Cell fill color.
    Background,
}

impl Default for ColorType {
    fn default() -> Self {
        ColorType::Foreground
    }
}

/// The raw-color prefixes [`is_raw_color_value`] recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RawColorPrefix {
    /// `rgb(R,G,B)` literal — what the RGB themes emit most often.
    Rgb,
    /// `#rrggbb` literal.
    Hex,
    /// `ansi256(N)` literal.
    Ansi256,
    /// `ansi:NAME` literal — used by the ANSI themes (`ansi:red`,
    /// `ansi:blueBright`, …).
    AnsiNamed,
}

/// Every raw-color prefix paired with its variant, in match order.
/// Order matters: the first prefix that matches is the one reported.
pub const RAW_COLOR_PREFIXES: &[(&str, RawColorPrefix)] = &[
    ("rgb(", RawColorPrefix::Rgb),
    ("#", RawColorPrefix::Hex),
    ("ansi256(", RawColorPrefix::Ansi256),
    ("ansi:", RawColorPrefix::AnsiNamed),
];

/// Classify `value` as a raw color literal or as a theme key.
///
/// Returns the matching prefix when `value` starts with one of the four
/// literals, otherwise `None` — which also covers the empty string.
/// Matching is case-sensitive and follows [`RAW_COLOR_PREFIXES`] order.
pub fn is_raw_color_value(value: &str) -> Option<RawColorPrefix> {
    for (prefix, kind) in RAW_COLOR_PREFIXES {
        if value.starts_with(prefix) {
            return Some(*kind);
        }
    }
    None
}

/// What [`resolve_color`] decided a color setting means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedColor {
    /// No color was requested; emit the text untouched.
    PassThrough,
    /// A raw color literal. Hand `value` straight to the colorizer.
    Raw {
        /// The prefix that matched.
        prefix: RawColorPrefix,
        /// The full literal (`rgb(255,0,0)`, `#ff0000`, …).
        value: String,
    },
    /// A theme-key lookup, with the palette's literal in `value`.
    Themed {
        /// The key the caller passed in.
        key: String,
        /// The palette's resolved literal for that key.
        value: String,
    },
}

/// Decide what a color setting means under one theme. Generating actual
/// ANSI escapes is the consumer renderer's job; this only classifies.
///
/// `None` short-circuits to [`ResolvedColor::PassThrough`]. A value that
/// starts with one of the four raw prefixes comes back as
/// [`ResolvedColor::Raw`]. Anything else is looked up as a theme key and
/// reported as [`ResolvedColor::Themed`].
///
/// An unknown key resolves to an empty `value` rather than an error;
/// colorizers are expected to silently no-op on an empty color.
pub fn resolve_color(c: Option<&str>, theme_name: ThemeName) -> ResolvedColor {
    let Some(c) = c else {
        return ResolvedColor::PassThrough;
    };
    if let Some(prefix) = is_raw_color_value(c) {
        return ResolvedColor::Raw {
            prefix,
            value: c.to_string(),
        };
    }
    let theme = get_theme(theme_name);
    let value = theme.lookup(c).map(|s| s.to_string()).unwrap_or_default();
    ResolvedColor::Themed {
        key: c.to_string(),
        value,
    }
}

/// Run `text` through the caller's colorizer using a resolved color.
///
/// `colorize` receives `(text, value, color_type)` and returns the styled
/// string — this crate never writes escape sequences itself.
/// [`ResolvedColor::PassThrough`] skips the callback and returns `text`
/// unchanged.
pub fn apply_color<F>(
    text: &str,
    resolved: &ResolvedColor,
    color_type: ColorType,
    colorize: F,
) -> String
where
    F: FnOnce(&str, &str, ColorType) -> String,
{
    match resolved {
        ResolvedColor::PassThrough => text.to_string(),
        ResolvedColor::Raw { value, .. } => colorize(text, value, color_type),
        ResolvedColor::Themed { value, .. } => colorize(text, value, color_type),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_colorize(text: &str, value: &str, ty: ColorType) -> String {
        format!("[{}|{}|{:?}]", text, value, ty)
    }

    // ────────────────────────────────────────────────────────────────
    // is_raw_color_value
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn raw_rgb_prefix_matches() {
        assert_eq!(
            is_raw_color_value("rgb(255,0,0)"),
            Some(RawColorPrefix::Rgb)
        );
    }

    #[test]
    fn raw_hex_prefix_matches() {
        assert_eq!(is_raw_color_value("#ff00aa"), Some(RawColorPrefix::Hex));
    }

    #[test]
    fn raw_ansi256_prefix_matches() {
        assert_eq!(
            is_raw_color_value("ansi256(123)"),
            Some(RawColorPrefix::Ansi256)
        );
    }

    #[test]
    fn raw_ansi_named_prefix_matches() {
        assert_eq!(
            is_raw_color_value("ansi:redBright"),
            Some(RawColorPrefix::AnsiNamed)
        );
    }

    #[test]
    fn theme_key_does_not_match_any_prefix() {
        assert!(is_raw_color_value("rebon").is_none());
        assert!(is_raw_color_value("permission").is_none());
        assert!(is_raw_color_value("professionalBlue").is_none());
    }

    #[test]
    fn empty_string_does_not_match_any_prefix() {
        // The empty string doesn't start with `#`, `rgb(`, `ansi256(`,
        // or `ansi:`.
        assert!(is_raw_color_value("").is_none());
    }

    #[test]
    fn case_sensitive_prefix() {
        // Prefix matching is case-sensitive. `RGB(` does not
        // match `rgb(`.
        assert!(is_raw_color_value("RGB(255,0,0)").is_none());
        assert!(is_raw_color_value("ANSI:red").is_none());
    }

    #[test]
    fn raw_color_prefixes_table_order_is_pinned() {
        // Pinning the prefix order so a future refactor doesn't
        // shuffle them and change which variant `is_raw_color_value`
        // returns.
        let names: Vec<&str> = RAW_COLOR_PREFIXES.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["rgb(", "#", "ansi256(", "ansi:"]);
    }

    // ────────────────────────────────────────────────────────────────
    // resolve_color
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn resolve_none_passes_through() {
        assert_eq!(
            resolve_color(None, ThemeName::Dark),
            ResolvedColor::PassThrough
        );
    }

    #[test]
    fn resolve_raw_rgb() {
        let r = resolve_color(Some("rgb(1,2,3)"), ThemeName::Dark);
        assert_eq!(
            r,
            ResolvedColor::Raw {
                prefix: RawColorPrefix::Rgb,
                value: "rgb(1,2,3)".to_string()
            }
        );
    }

    #[test]
    fn resolve_raw_hex() {
        let r = resolve_color(Some("#abcdef"), ThemeName::Light);
        assert_eq!(
            r,
            ResolvedColor::Raw {
                prefix: RawColorPrefix::Hex,
                value: "#abcdef".to_string()
            }
        );
    }

    #[test]
    fn resolve_themed_key_dark_brand() {
        let r = resolve_color(Some("rebon"), ThemeName::Dark);
        match r {
            ResolvedColor::Themed { key, value } => {
                assert_eq!(key, "rebon");
                assert_eq!(value, "rgb(138,181,227)");
            }
            other => panic!("expected Themed, got {:?}", other),
        }
    }

    #[test]
    fn resolve_themed_key_light_text_is_black() {
        let r = resolve_color(Some("text"), ThemeName::Light);
        match r {
            ResolvedColor::Themed { value, .. } => {
                assert_eq!(value, "rgb(0,0,0)");
            }
            other => panic!("expected Themed, got {:?}", other),
        }
    }

    #[test]
    fn resolve_themed_key_dark_text_is_white() {
        let r = resolve_color(Some("text"), ThemeName::Dark);
        match r {
            ResolvedColor::Themed { value, .. } => {
                assert_eq!(value, "rgb(255,255,255)");
            }
            other => panic!("expected Themed, got {:?}", other),
        }
    }

    #[test]
    fn resolve_unknown_theme_key_returns_empty_value() {
        // An unknown key resolves to an empty value; colorizers are
        // expected to silently no-op on it.
        let r = resolve_color(Some("not_a_real_key"), ThemeName::Dark);
        match r {
            ResolvedColor::Themed { key, value } => {
                assert_eq!(key, "not_a_real_key");
                assert_eq!(value, "");
            }
            other => panic!("expected Themed, got {:?}", other),
        }
    }

    #[test]
    fn resolve_raw_takes_priority_over_theme_key() {
        // Even if a theme had a key called `rgb(...)`, the raw-color
        // short-circuit fires first.
        let r = resolve_color(Some("rgb(0,0,0)"), ThemeName::Light);
        assert!(matches!(r, ResolvedColor::Raw { .. }));
    }

    // ────────────────────────────────────────────────────────────────
    // apply_color
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn apply_pass_through_returns_text_unchanged() {
        let r = ResolvedColor::PassThrough;
        let out = apply_color("hi", &r, ColorType::Foreground, echo_colorize);
        assert_eq!(out, "hi");
    }

    #[test]
    fn apply_raw_calls_colorize_with_value() {
        let r = ResolvedColor::Raw {
            prefix: RawColorPrefix::Rgb,
            value: "rgb(1,2,3)".to_string(),
        };
        let out = apply_color("hi", &r, ColorType::Foreground, echo_colorize);
        assert_eq!(out, "[hi|rgb(1,2,3)|Foreground]");
    }

    #[test]
    fn apply_themed_calls_colorize_with_resolved_value() {
        let r = ResolvedColor::Themed {
            key: "rebon".to_string(),
            value: "rgb(138,181,227)".to_string(),
        };
        let out = apply_color("hi", &r, ColorType::Background, echo_colorize);
        assert_eq!(out, "[hi|rgb(138,181,227)|Background]");
    }

    #[test]
    fn color_type_default_is_foreground() {
        // Pin: the color type defaults to foreground.
        assert_eq!(ColorType::default(), ColorType::Foreground);
    }
}
