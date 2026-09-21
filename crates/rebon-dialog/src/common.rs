//! Shared shapes used by every dialog module.
//!
//! This is a "shape namespace" — every dialog is independent, but a
//! few primitives recur across enough modules to be worth pinning
//! here.

use std::fmt;

/// A `{ label, value }` pair, matching the consumer's `Select`
/// widget input. Each dialog module instantiates this with its own
/// per-dialog `value` enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption<V> {
    /// The label shown in the rendered list.
    pub label: String,
    /// The value passed to the reducer when this option is selected.
    pub value: V,
}

impl<V> SelectOption<V> {
    /// Create a new option from `(label, value)`.
    pub fn new(label: impl Into<String>, value: V) -> Self {
        Self {
            label: label.into(),
            value,
        }
    }
}

/// The semantic color/role of a dialog frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogColor {
    /// Default neutral.
    Default,
    /// Warning (yellow).
    Warning,
    /// Error (red).
    Error,
    /// Permission prompt (cyan).
    Permission,
    /// Success (green).
    Success,
}

impl Default for DialogColor {
    fn default() -> Self {
        Self::Default
    }
}

impl fmt::Display for DialogColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DialogColor::Default => "default",
            DialogColor::Warning => "warning",
            DialogColor::Error => "error",
            DialogColor::Permission => "permission",
            DialogColor::Success => "success",
        };
        f.write_str(s)
    }
}

/// Truncate `s` to at most `max_chars` Unicode chars (not bytes,
/// not display width). Display-width-aware truncation would differ
/// for wide glyphs, but for ASCII titles / labels the two agree.
/// Trailing `…` is appended only if the input was truncated.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let take = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_option_new() {
        let opt: SelectOption<&'static str> = SelectOption::new("Hello", "world");
        assert_eq!(opt.label, "Hello");
        assert_eq!(opt.value, "world");
    }

    #[test]
    fn dialog_color_display_table() {
        assert_eq!(DialogColor::Default.to_string(), "default");
        assert_eq!(DialogColor::Warning.to_string(), "warning");
        assert_eq!(DialogColor::Error.to_string(), "error");
        assert_eq!(DialogColor::Permission.to_string(), "permission");
        assert_eq!(DialogColor::Success.to_string(), "success");
    }

    #[test]
    fn dialog_color_default_is_default() {
        assert_eq!(DialogColor::default(), DialogColor::Default);
    }

    #[test]
    fn truncate_chars_preserves_short_input() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello");
    }

    #[test]
    fn truncate_chars_truncates_long_input() {
        assert_eq!(truncate_chars("hello world", 5), "hell…");
    }

    #[test]
    fn truncate_chars_zero_max() {
        assert_eq!(truncate_chars("hello", 0), "");
    }

    #[test]
    fn truncate_chars_one_max() {
        assert_eq!(truncate_chars("hello", 1), "…");
    }

    #[test]
    fn truncate_chars_handles_unicode() {
        assert_eq!(truncate_chars("héllo wörld", 5).chars().count(), 5);
    }
}
