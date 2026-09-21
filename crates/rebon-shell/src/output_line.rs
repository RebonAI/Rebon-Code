//! Projection for one line of shell output: JSON pretty-printing, URL
//! linkification, underline-ANSI stripping, and the full-versus-truncated
//! decision.
//!
//! The order of the steps is fixed and observable: JSON is reformatted first,
//! linkification runs over that reformatted text, and truncation runs last so
//! the wrap width and the expand hint are measured against the final
//! characters. The result is pure data - the caller paints the string this
//! returns.

use crate::expand_shell_output::{expand_shell_output_enabled, ExpandShellOutputContextValue};
use serde_json::Value;

/// Content longer than this many bytes is not reformatted as JSON.
pub const MAX_JSON_FORMAT_LENGTH: usize = 10_000;
/// Number of wrapped lines shown before the rest is summarised.
pub const MAX_LINES_TO_SHOW: usize = 3;
/// Columns kept free when wrapping, so lines do not overflow the row.
pub const PADDING_TO_PREVENT_OVERFLOW: usize = 10;
/// The `http(s)` URL rule, as a regex for callers that have an engine.
///
/// This crate does not: it is deliberately near-dependency-free, so
/// [`linkify_urls_in_text`] scans for the same rule by hand. The two are the
/// same fact written twice on purpose — `url_pattern_and_scanner_agree` pins
/// them together, so a change to either side has to move both.
pub const URL_IN_JSON_PATTERN: &str = r#"https?://[^\s"'<>\\]+"#;
/// OSC 8 prefix.
pub const OSC8_START: &str = "\u{1b}]8;;";
/// OSC 8 terminator.
pub const OSC8_END: &str = "\u{7}";
const CTRL_O_EXPAND_HINT: &str = "(Ctrl+O to expand)";

/// The semantic tone the row is drawn with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTone {
    /// Neither warning nor error.
    Neutral,
    /// Warning styling.
    Warning,
    /// Error styling.
    Error,
}

/// Input bag for the projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLineInput {
    /// Raw shell content.
    pub content: String,
    /// Whether the verbose output style is in use.
    pub verbose: bool,
    /// Error tone wins over warning tone.
    pub is_error: bool,
    /// Warning tone when `is_error` is false.
    pub is_warning: bool,
    /// Whether URL linkification is enabled for this row.
    pub linkify_urls: bool,
    /// Whether the terminal supports hyperlinks.
    pub supports_hyperlinks: bool,
    /// Terminal column count.
    pub terminal_columns: usize,
    /// Whether the row sits inside a virtualized list, which suppresses
    /// the expand hint.
    pub in_virtual_list: bool,
    /// Whether the expand-shell-output flag is set.
    pub expand_shell_output: ExpandShellOutputContextValue,
}

/// Output display shape. The consumer renders the `formatted` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLineDisplay {
    /// Final formatted line content.
    pub formatted: String,
    /// Semantic tone to apply.
    pub tone: OutputTone,
    /// Whether the line was truncated.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TruncationResult {
    remaining_lines: usize,
}

const JS_MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// Attempt to pretty-print a single line of JSON.
///
/// Bails out and returns the input unchanged when the value contains an
/// integer outside the exactly-representable range (the
/// `contains_js_unsafe_integer` guard), or when re-serializing would not
/// reproduce the original text.
pub fn try_format_json(line: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Value>(line) else {
        return line.to_string();
    };
    if contains_js_unsafe_integer(&parsed) {
        return line.to_string();
    }
    let pretty =
        serde_json::to_string_pretty(&parsed).expect("serde_json::Value always serializes");
    let normalized_original = normalize_json_source(line);
    let normalized_stringified = normalize_json_source(&pretty);
    if normalized_original != normalized_stringified {
        line.to_string()
    } else {
        pretty
    }
}

/// Apply [`try_format_json`] to every line when the whole content fits
/// within `MAX_JSON_FORMAT_LENGTH`.
pub fn try_json_format_content(content: &str) -> String {
    if content.len() > MAX_JSON_FORMAT_LENGTH {
        return content.to_string();
    }
    content
        .split('\n')
        .map(try_format_json)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build an OSC 8 hyperlink, honouring the caller's hyperlink-support flag.
pub fn create_hyperlink(url: &str, content: Option<&str>, supports_hyperlinks: bool) -> String {
    if !supports_hyperlinks {
        return url.to_string();
    }
    let display_text = content.unwrap_or(url);
    format!(
        "{start}{url}{end}{text}{start}{end}",
        start = OSC8_START,
        url = url,
        end = OSC8_END,
        text = display_text,
    )
}

/// Linkify URLs inside plain text. The stop characters are conservative:
/// whitespace, quotes, angle brackets, and backslash.
pub fn linkify_urls_in_text(content: &str, supports_hyperlinks: bool) -> String {
    if !supports_hyperlinks {
        return content.to_string();
    }

    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    while let Some((start, end)) = find_next_url(content, cursor) {
        out.push_str(&content[cursor..start]);
        let url = &content[start..end];
        out.push_str(&create_hyperlink(url, Some(url), true));
        cursor = end;
    }
    out.push_str(&content[cursor..]);
    out
}

/// Strip underline-only ANSI sequences from `content`.
pub fn strip_underline_ansi(content: &str) -> String {
    let bytes = content.as_bytes();
    let mut i = 0usize;
    let mut out = String::with_capacity(content.len());

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            if let Some(end) = find_csi_end(bytes, i + 2) {
                let params = &content[i + 2..end];
                if csi_sets_underline(params) {
                    i = end + 1;
                    continue;
                }
            }
        }

        let ch = content[i..]
            .chars()
            .next()
            .expect("index always on char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

/// Project one line of shell output into the string to paint.
pub fn project_output_line(input: &OutputLineInput) -> OutputLineDisplay {
    let should_show_full = input.verbose || expand_shell_output_enabled(input.expand_shell_output);

    let mut formatted = try_json_format_content(&input.content);
    if input.linkify_urls {
        formatted = linkify_urls_in_text(&formatted, input.supports_hyperlinks);
    }
    let formatted = if should_show_full {
        strip_underline_ansi(&formatted)
    } else {
        strip_underline_ansi(&render_truncated_content(
            &formatted,
            input.terminal_columns,
            input.in_virtual_list,
        ))
    };

    OutputLineDisplay {
        formatted,
        tone: if input.is_error {
            OutputTone::Error
        } else if input.is_warning {
            OutputTone::Warning
        } else {
            OutputTone::Neutral
        },
        truncated: !should_show_full
            && is_output_line_truncated(&input.content, input.terminal_columns),
    }
}

/// Compare text for the round-trip guard. An escaped `/` and insignificant
/// whitespace are formatting noise, not a difference, so both sides are
/// normalized before the comparison that decides whether pretty-printing is
/// safe to apply.
fn normalize_json_source(text: &str) -> String {
    text.replace("\\/", "/")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

fn contains_js_unsafe_integer(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            number
                .as_i64()
                .is_some_and(|n| !(-JS_MAX_SAFE_INTEGER..=JS_MAX_SAFE_INTEGER).contains(&n))
                || number
                    .as_u64()
                    .is_some_and(|n| n > JS_MAX_SAFE_INTEGER as u64)
        }
        Value::Array(items) => items.iter().any(contains_js_unsafe_integer),
        Value::Object(map) => map.values().any(contains_js_unsafe_integer),
        _ => false,
    }
}

fn find_next_url(content: &str, from: usize) -> Option<(usize, usize)> {
    let haystack = &content[from..];
    let http = haystack.find("http://");
    let https = haystack.find("https://");
    let rel_start = match (http, https) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    let start = from + rel_start;
    let mut end = start;
    for (offset, ch) in content[start..].char_indices() {
        if ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>' | '\\') {
            break;
        }
        end = start + offset + ch.len_utf8();
    }
    Some((start, end))
}

fn find_csi_end(bytes: &[u8], mut idx: usize) -> Option<usize> {
    while idx < bytes.len() {
        if (0x40..=0x7e).contains(&bytes[idx]) {
            return Some(idx);
        }
        idx += 1;
    }
    None
}

fn csi_sets_underline(params: &str) -> bool {
    params.split(';').any(|part| part == "4")
}

fn render_truncated_content(
    content: &str,
    terminal_width: usize,
    suppress_expand_hint: bool,
) -> String {
    let trimmed = content.trim_end();
    if trimmed.is_empty() {
        return String::new();
    }

    // `.max(10)` keeps the wrap width sane on a very narrow terminal, where
    // subtracting the padding would otherwise reach zero.
    let wrap_width = terminal_width
        .saturating_sub(PADDING_TO_PREVENT_OVERFLOW)
        .max(10);
    let wrapped_lines = wrap_text(trimmed, wrap_width);
    let remaining_lines = wrapped_lines.len().saturating_sub(MAX_LINES_TO_SHOW);

    // A single hidden line is not worth a summary row; show it instead.
    if remaining_lines == 1 {
        return wrapped_lines
            .into_iter()
            .take(MAX_LINES_TO_SHOW + 1)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string();
    }

    let mut parts = vec![wrapped_lines
        .into_iter()
        .take(MAX_LINES_TO_SHOW)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()];
    if remaining_lines > 0 {
        let mut hint = format!("\u{2026} +{remaining_lines} lines");
        if !suppress_expand_hint {
            hint.push(' ');
            hint.push_str(CTRL_O_EXPAND_HINT);
        }
        parts.push(hint);
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn wrap_text(text: &str, wrap_width: usize) -> Vec<String> {
    let mut wrapped_lines = Vec::new();
    for line in text.split('\n') {
        let visible_width = line.chars().count();
        if visible_width <= wrap_width {
            wrapped_lines.push(line.trim_end().to_string());
            continue;
        }

        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(wrap_width) {
            let chunk_text: String = chunk.iter().collect();
            wrapped_lines.push(chunk_text.trim_end().to_string());
        }
    }
    wrapped_lines
}

fn wrap_text_for_measure(text: &str, wrap_width: usize) -> TruncationResult {
    let wrapped_lines = wrap_text(text, wrap_width);
    TruncationResult {
        remaining_lines: wrapped_lines.len().saturating_sub(MAX_LINES_TO_SHOW),
    }
}

fn is_output_line_truncated(content: &str, terminal_columns: usize) -> bool {
    let trimmed = content.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    let wrap_width = terminal_columns
        .saturating_sub(PADDING_TO_PREVENT_OVERFLOW)
        .max(10);
    let measured = wrap_text_for_measure(trimmed, wrap_width);
    measured.remaining_lines > 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published regex and the hand-rolled scanner have to stop on the
    /// same characters. Written out rather than compiled, because this crate
    /// keeps its dependency list to `serde_json` on purpose.
    #[test]
    fn url_pattern_and_scanner_agree() {
        // Every delimiter the pattern's negated class names.
        for stop in [' ', '\t', '\n', '"', '\'', '<', '>', '\\'] {
            let text = format!("see https://example.com/a{stop}tail");
            let linked = linkify_urls_in_text(&text, true);
            let url_end = linked
                .find(OSC8_END)
                .expect("the scanner linkified something");
            assert!(
                linked[..url_end].ends_with("https://example.com/a"),
                "scanner ran past {stop:?}: {linked:?}"
            );
        }
        // And a character the class does not name is kept inside the URL.
        let linked = linkify_urls_in_text("https://example.com/a(b)", true);
        assert!(linked.contains("https://example.com/a(b)"), "{linked:?}");

        // The pattern still spells that rule, so a caller compiling it gets
        // the same answer this crate gives.
        assert_eq!(URL_IN_JSON_PATTERN, r#"https?://[^\s"'<>\\]+"#);
    }

    fn input(content: &str) -> OutputLineInput {
        OutputLineInput {
            content: content.to_string(),
            verbose: false,
            is_error: false,
            is_warning: false,
            linkify_urls: false,
            supports_hyperlinks: true,
            terminal_columns: 80,
            in_virtual_list: false,
            expand_shell_output: ExpandShellOutputContextValue::default(),
        }
    }

    #[test]
    fn try_format_json_pretty_prints_safe_json() {
        let line = r#"{"count":1,"label":"A"}"#;
        let pretty = try_format_json(line);
        assert_eq!(pretty, "{\n  \"count\": 1,\n  \"label\": \"A\"\n}");
    }

    #[test]
    fn try_format_json_keeps_original_for_invalid_json() {
        let line = "{oops";
        assert_eq!(try_format_json(line), line);
    }

    #[test]
    fn try_format_json_keeps_original_when_integer_exceeds_js_safe_range() {
        let line = r#"{"value":9007199254740992}"#;
        assert_eq!(try_format_json(line), line);
    }

    #[test]
    fn try_format_json_allows_max_safe_integer() {
        let line = r#"{"value":9007199254740991}"#;
        let pretty = try_format_json(line);
        assert!(pretty.contains('\n'));
    }

    #[test]
    fn try_json_format_content_skips_content_over_threshold() {
        let long = "a".repeat(MAX_JSON_FORMAT_LENGTH + 1);
        assert_eq!(try_json_format_content(&long), long);
    }

    #[test]
    fn try_json_format_content_maps_each_line() {
        let content = concat!(r#"{"a":1}"#, "\n", r#"{"b":2}"#);
        let pretty = try_json_format_content(content);
        assert!(pretty.contains("\"a\": 1"));
        assert!(pretty.contains("\"b\": 2"));
    }

    #[test]
    fn create_hyperlink_falls_back_when_terminal_lacks_support() {
        assert_eq!(
            create_hyperlink("https://example.com", None, false),
            "https://example.com"
        );
    }

    #[test]
    fn create_hyperlink_wraps_with_osc8_when_supported() {
        let link = create_hyperlink("https://example.com", Some("x"), true);
        assert_eq!(link, "\u{1b}]8;;https://example.com\u{7}x\u{1b}]8;;\u{7}");
    }

    #[test]
    fn linkify_urls_in_text_wraps_http_and_https() {
        let output = linkify_urls_in_text("go https://a.com then http://b.com", true);
        assert!(output.contains("\u{1b}]8;;https://a.com\u{7}https://a.com"));
        assert!(output.contains("\u{1b}]8;;http://b.com\u{7}http://b.com"));
    }

    #[test]
    fn linkify_urls_in_text_stops_at_quotes_and_backslash() {
        let output = linkify_urls_in_text(r#"{"u":"https://a.com\"tail"}"#, true);
        assert!(output.contains("https://a.com"));
        assert!(!output.contains("https://a.com\\"));
    }

    #[test]
    fn linkify_urls_in_text_returns_plain_text_when_hyperlinks_unsupported() {
        let input = "check https://a.com";
        assert_eq!(linkify_urls_in_text(input, false), input);
    }

    #[test]
    fn strip_underline_ansi_removes_simple_underline_sequence() {
        let input = "\u{1b}[4munderlined\u{1b}[0m";
        assert_eq!(strip_underline_ansi(input), "underlined\u{1b}[0m");
    }

    #[test]
    fn strip_underline_ansi_removes_underline_from_mixed_csi_sequence() {
        let input = "\u{1b}[31;4mred\u{1b}[0m";
        assert_eq!(strip_underline_ansi(input), "red\u{1b}[0m");
    }

    #[test]
    fn strip_underline_ansi_keeps_non_underline_sequences() {
        let input = "\u{1b}[31mred\u{1b}[0m";
        assert_eq!(strip_underline_ansi(input), input);
    }

    #[test]
    fn project_output_line_verbose_shows_full_output() {
        let mut input = input("one\ntwo\nthree\nfour");
        input.verbose = true;
        let display = project_output_line(&input);
        assert!(!display.truncated);
        assert_eq!(display.formatted, "one\ntwo\nthree\nfour");
    }

    #[test]
    fn project_output_line_expand_context_shows_full_output() {
        let mut input = input("one\ntwo\nthree\nfour");
        input.expand_shell_output = ExpandShellOutputContextValue::from_bool(true);
        let display = project_output_line(&input);
        assert!(!display.truncated);
        assert_eq!(display.formatted, "one\ntwo\nthree\nfour");
    }

    #[test]
    fn project_output_line_truncates_multiline_content() {
        let display = project_output_line(&input("one\ntwo\nthree\nfour\nfive"));
        assert!(display.truncated);
        assert!(display.formatted.contains("\u{2026} +2 lines"));
        assert!(display.formatted.contains(CTRL_O_EXPAND_HINT));
    }

    #[test]
    fn project_output_line_suppresses_expand_hint_in_virtual_list() {
        let mut input = input("one\ntwo\nthree\nfour\nfive");
        input.in_virtual_list = true;
        let display = project_output_line(&input);
        assert!(display.truncated);
        assert!(!display.formatted.contains(CTRL_O_EXPAND_HINT));
    }

    #[test]
    fn project_output_line_warning_tone_wins_when_no_error() {
        let mut input = input("line");
        input.is_warning = true;
        let display = project_output_line(&input);
        assert_eq!(display.tone, OutputTone::Warning);
    }

    #[test]
    fn project_output_line_error_tone_beats_warning() {
        let mut input = input("line");
        input.is_warning = true;
        input.is_error = true;
        let display = project_output_line(&input);
        assert_eq!(display.tone, OutputTone::Error);
    }

    #[test]
    fn project_output_line_linkifies_when_requested() {
        let mut input = input("https://a.com");
        input.verbose = true;
        input.linkify_urls = true;
        let display = project_output_line(&input);
        assert!(display.formatted.contains(OSC8_START));
    }

    #[test]
    fn single_wrapped_extra_line_is_shown_instead_of_hint() {
        let mut input = input("abcdefghijabcdefghijabcdefghija");
        input.terminal_columns = 20;
        let display = project_output_line(&input);
        assert!(!display.truncated);
        assert_eq!(display.formatted.lines().count(), 4);
    }
}
