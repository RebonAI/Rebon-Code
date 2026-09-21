//! MCP config parsing warnings — formatter for server configs that
//! failed config validation.
//!
//! The module groups the parsing errors by server name (see
//! [`group_by_server`]) and formats a collapsible warning panel. It exposes:
//!
//! * [`ParsingWarning`] — the per-server warning data shape
//! * [`ParsingWarningKind`] — the kind-of-error discriminator
//! * [`format_parsing_warnings`] — the full panel text
//! * [`format_parsing_warning_row`] — a single row (server name + reason)

/// The class of error that caused a server config to be skipped.
/// One variant per class of validation issue (missing field, wrong
/// type, bad URL, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParsingWarningKind {
    /// A required field was missing.
    MissingRequired,
    /// A field had the wrong type (e.g. number instead of string).
    InvalidType,
    /// A url field was malformed.
    InvalidUrl,
    /// An enum discriminator had an unknown value.
    InvalidEnum,
    /// A string was too short (e.g. an empty `command`).
    StringTooShort,
    /// The server type was unknown entirely.
    UnknownServerType,
    /// Generic fallback for any other validation error.
    Other,
}

impl ParsingWarningKind {
    /// A short human-readable label for the kind. Pinned by tests.
    pub fn label(&self) -> &'static str {
        match self {
            ParsingWarningKind::MissingRequired => "missing required field",
            ParsingWarningKind::InvalidType => "invalid type",
            ParsingWarningKind::InvalidUrl => "invalid URL",
            ParsingWarningKind::InvalidEnum => "invalid enum value",
            ParsingWarningKind::StringTooShort => "string too short",
            ParsingWarningKind::UnknownServerType => "unknown server type",
            ParsingWarningKind::Other => "invalid",
        }
    }
}

/// A single parsing warning for a single server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsingWarning {
    /// The server name (the key in `mcpServers`).
    pub server_name: String,
    /// The kind of error.
    pub kind: ParsingWarningKind,
    /// The field path (`"command"`, `"oauth.clientId"`, etc.).
    pub path: String,
    /// A descriptive message from the validation error.
    pub message: String,
}

/// Format a single warning as a display row.
///
/// Form: `- {server_name}: {label} ({path}) — {message}`
///
/// Each row is a bullet in the panel, hence the leading `- `. The separator
/// between label and path is a parenthesized
/// path; between path and message is an em-dash with spaces.
pub fn format_parsing_warning_row(warning: &ParsingWarning) -> String {
    let path_section = if warning.path.is_empty() {
        String::new()
    } else {
        format!(" ({})", warning.path)
    };
    let message_section = if warning.message.is_empty() {
        String::new()
    } else {
        format!(" \u{2014} {}", warning.message)
    };
    format!(
        "- {server}: {label}{path}{msg}",
        server = warning.server_name,
        label = warning.kind.label(),
        path = path_section,
        msg = message_section,
    )
}

/// Format the full panel. Returns the per-server heading plus a
/// newline-joined bullet list of warnings.
///
/// If the list is empty, returns an empty string.
pub fn format_parsing_warnings(warnings: &[ParsingWarning]) -> String {
    if warnings.is_empty() {
        return String::new();
    }
    let mut out = String::from("Some MCP servers could not be loaded:\n");
    let rows: Vec<String> = warnings.iter().map(format_parsing_warning_row).collect();
    out.push_str(&rows.join("\n"));
    out
}

/// Group warnings by server name, preserving insertion order. Useful
/// when the consumer wants a per-server header rather than a flat list.
pub fn group_by_server(warnings: &[ParsingWarning]) -> Vec<(String, Vec<ParsingWarning>)> {
    let mut order: Vec<String> = Vec::new();
    let mut buckets: std::collections::HashMap<String, Vec<ParsingWarning>> =
        std::collections::HashMap::new();
    for w in warnings {
        if !buckets.contains_key(&w.server_name) {
            order.push(w.server_name.clone());
        }
        buckets
            .entry(w.server_name.clone())
            .or_default()
            .push(w.clone());
    }
    order
        .into_iter()
        .map(|name| {
            let group = buckets.remove(&name).unwrap_or_default();
            (name, group)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_warning(
        server: &str,
        kind: ParsingWarningKind,
        path: &str,
        message: &str,
    ) -> ParsingWarning {
        ParsingWarning {
            server_name: server.to_string(),
            kind,
            path: path.to_string(),
            message: message.to_string(),
        }
    }

    // --- Kind labels ---

    #[test]
    fn kind_labels_pinned() {
        assert_eq!(
            ParsingWarningKind::MissingRequired.label(),
            "missing required field"
        );
        assert_eq!(ParsingWarningKind::InvalidType.label(), "invalid type");
        assert_eq!(ParsingWarningKind::InvalidUrl.label(), "invalid URL");
        assert_eq!(
            ParsingWarningKind::InvalidEnum.label(),
            "invalid enum value"
        );
        assert_eq!(
            ParsingWarningKind::StringTooShort.label(),
            "string too short"
        );
        assert_eq!(
            ParsingWarningKind::UnknownServerType.label(),
            "unknown server type"
        );
        assert_eq!(ParsingWarningKind::Other.label(), "invalid");
    }

    // --- Single-row formatting ---

    #[test]
    fn row_full_shape() {
        let w = mk_warning(
            "linear",
            ParsingWarningKind::InvalidUrl,
            "url",
            "must start with https://",
        );
        assert_eq!(
            format_parsing_warning_row(&w),
            "- linear: invalid URL (url) \u{2014} must start with https://",
        );
    }

    #[test]
    fn row_without_path() {
        let w = mk_warning(
            "slack",
            ParsingWarningKind::UnknownServerType,
            "",
            "unknown discriminator",
        );
        assert_eq!(
            format_parsing_warning_row(&w),
            "- slack: unknown server type \u{2014} unknown discriminator",
        );
    }

    #[test]
    fn row_without_message() {
        let w = mk_warning("x", ParsingWarningKind::Other, "some.path", "");
        assert_eq!(format_parsing_warning_row(&w), "- x: invalid (some.path)");
    }

    #[test]
    fn row_with_neither_path_nor_message() {
        let w = mk_warning("x", ParsingWarningKind::InvalidType, "", "");
        assert_eq!(format_parsing_warning_row(&w), "- x: invalid type");
    }

    #[test]
    fn row_preserves_deep_path() {
        let w = mk_warning(
            "x",
            ParsingWarningKind::MissingRequired,
            "oauth.clientId",
            "required",
        );
        assert_eq!(
            format_parsing_warning_row(&w),
            "- x: missing required field (oauth.clientId) \u{2014} required",
        );
    }

    // --- Panel formatting ---

    #[test]
    fn panel_empty_is_empty_string() {
        assert_eq!(format_parsing_warnings(&[]), "");
    }

    #[test]
    fn panel_single_warning() {
        let warnings = vec![mk_warning(
            "linear",
            ParsingWarningKind::InvalidUrl,
            "url",
            "bad",
        )];
        let out = format_parsing_warnings(&warnings);
        assert!(out.starts_with("Some MCP servers could not be loaded:\n"));
        assert!(out.contains("- linear: invalid URL"));
    }

    #[test]
    fn panel_multiple_warnings_joined_with_newlines() {
        let warnings = vec![
            mk_warning("a", ParsingWarningKind::InvalidUrl, "url", "m1"),
            mk_warning("b", ParsingWarningKind::MissingRequired, "command", "m2"),
        ];
        let out = format_parsing_warnings(&warnings);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 rows
        assert_eq!(lines[0], "Some MCP servers could not be loaded:");
        assert!(lines[1].starts_with("- a:"));
        assert!(lines[2].starts_with("- b:"));
    }

    #[test]
    fn panel_preserves_input_order() {
        let warnings = vec![
            mk_warning("z", ParsingWarningKind::InvalidUrl, "", ""),
            mk_warning("a", ParsingWarningKind::InvalidUrl, "", ""),
            mk_warning("m", ParsingWarningKind::InvalidUrl, "", ""),
        ];
        let out = format_parsing_warnings(&warnings);
        let lines: Vec<&str> = out.lines().collect();
        // NO alphabetical sort.
        assert!(lines[1].starts_with("- z:"));
        assert!(lines[2].starts_with("- a:"));
        assert!(lines[3].starts_with("- m:"));
    }

    // --- group_by_server ---

    #[test]
    fn group_by_server_empty() {
        assert!(group_by_server(&[]).is_empty());
    }

    #[test]
    fn group_by_server_single() {
        let warnings = vec![mk_warning("x", ParsingWarningKind::InvalidUrl, "", "")];
        let groups = group_by_server(&warnings);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "x");
        assert_eq!(groups[0].1.len(), 1);
    }

    #[test]
    fn group_by_server_multiple_warnings_same_server() {
        let warnings = vec![
            mk_warning("x", ParsingWarningKind::InvalidUrl, "url", ""),
            mk_warning("x", ParsingWarningKind::MissingRequired, "oauth", ""),
        ];
        let groups = group_by_server(&warnings);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1.len(), 2);
    }

    #[test]
    fn group_by_server_preserves_first_seen_order() {
        let warnings = vec![
            mk_warning("b", ParsingWarningKind::InvalidUrl, "", ""),
            mk_warning("a", ParsingWarningKind::InvalidUrl, "", ""),
            mk_warning("b", ParsingWarningKind::MissingRequired, "", ""),
        ];
        let groups = group_by_server(&warnings);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "b");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "a");
        assert_eq!(groups[1].1.len(), 1);
    }

    #[test]
    fn group_by_server_interleaved() {
        let warnings = vec![
            mk_warning("a", ParsingWarningKind::InvalidUrl, "", ""),
            mk_warning("b", ParsingWarningKind::InvalidUrl, "", ""),
            mk_warning("a", ParsingWarningKind::InvalidType, "", ""),
            mk_warning("c", ParsingWarningKind::InvalidUrl, "", ""),
            mk_warning("b", ParsingWarningKind::MissingRequired, "", ""),
        ];
        let groups = group_by_server(&warnings);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].0, "a");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "b");
        assert_eq!(groups[1].1.len(), 2);
        assert_eq!(groups[2].0, "c");
        assert_eq!(groups[2].1.len(), 1);
    }

    #[test]
    fn panel_header_text_pinned() {
        let warnings = vec![mk_warning("x", ParsingWarningKind::Other, "", "")];
        let out = format_parsing_warnings(&warnings);
        // Pin the literal header copy — a refactor must not silently
        // rename it without updating the test.
        assert!(out.contains("Some MCP servers could not be loaded:"));
    }
}
