//! Tool list view — projection from an MCP server's tools into
//! selectable option rows.
//!
//! The projection:
//! 1. Filters a slice of [`ToolInfo`] by server name via
//!    [`filter_tools_by_server`].
//! 2. Maps each tool to a [`ToolRow`]:
//!    - `label` = the tool's pre-formatted `display_name`
//!    - `value` = the row index rendered as a string
//!    - `description` = the annotation labels joined with `", "`, or
//!      `None` when there are none
//!    - `description_color` = `Error` if destructive, else `Success`
//!      if read-only, else `None`
//! 3. Formats a title `Tools for {server_name}` and subtitle
//!    `{n} tool(s)` (plural form).
//!
//! The three annotation flags come from the tool metadata:
//! `is_read_only`, `is_destructive`, `is_open_world`. They are pushed
//! in that exact order.

/// The three annotation flags a tool can advertise. Pinned in the
/// push order used by [`build_tool_row`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolAnnotationKind {
    /// The tool promises not to mutate state. Colors the description green.
    ReadOnly,
    /// The tool may mutate / destroy data. Colors the description red.
    Destructive,
    /// The tool may access arbitrary external resources. Neutral color.
    OpenWorld,
}

impl ToolAnnotationKind {
    /// The lowercase hyphenated label used in the description.
    pub fn label(&self) -> &'static str {
        match self {
            ToolAnnotationKind::ReadOnly => "read-only",
            ToolAnnotationKind::Destructive => "destructive",
            ToolAnnotationKind::OpenWorld => "open-world",
        }
    }
}

/// Color hints for a row's description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptionColor {
    Error,
    Success,
}

/// A single tool row in the list view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRow {
    /// The human-readable display name (post-stripping of the
    /// `mcp__servername__` prefix).
    pub label: String,
    /// The row index, rendered as a string, used as the option value.
    pub value: String,
    /// The joined annotations list (`"read-only, destructive"`), or
    /// None when the tool has no annotations.
    pub description: Option<String>,
    /// Color hint — Error wins over Success when both are present.
    pub description_color: Option<DescriptionColor>,
}

/// The shape of the minimal tool info the projection needs. The
/// consumer (which has the full `Tool` value) builds these and hands
/// them in; this projection never touches the full `Tool` type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInfo {
    /// The normalized tool name (`mcp__servername__tool`).
    pub name: String,
    /// The already-formatted display name. The consumer performs the
    /// extraction (strip the `mcp__servername__` prefix, falling back
    /// to the raw name) before handing it in.
    pub display_name: String,
    pub is_read_only: bool,
    pub is_destructive: bool,
    pub is_open_world: bool,
    /// The server that owns this tool. Used by [`filter_tools_by_server`].
    pub server_name: String,
}

/// Filter a tool list by server name. Does a straight string
/// comparison on the `server_name` field of each [`ToolInfo`].
pub fn filter_tools_by_server<'a>(tools: &'a [ToolInfo], server_name: &str) -> Vec<&'a ToolInfo> {
    tools
        .iter()
        .filter(|t| t.server_name == server_name)
        .collect()
}

/// Build a row for a single tool at its given index.
///
/// The index is rendered to a string: it becomes the row's option
/// value.
pub fn build_tool_row(tool: &ToolInfo, index: usize) -> ToolRow {
    let mut annotations = Vec::new();
    if tool.is_read_only {
        annotations.push(ToolAnnotationKind::ReadOnly);
    }
    if tool.is_destructive {
        annotations.push(ToolAnnotationKind::Destructive);
    }
    if tool.is_open_world {
        annotations.push(ToolAnnotationKind::OpenWorld);
    }

    let description = if annotations.is_empty() {
        None
    } else {
        Some(
            annotations
                .iter()
                .map(|a| a.label())
                .collect::<Vec<_>>()
                .join(", "),
        )
    };

    // Description color precedence: destructive wins (`Error`), else
    // read-only (`Success`), else neither.
    let description_color = if tool.is_destructive {
        Some(DescriptionColor::Error)
    } else if tool.is_read_only {
        Some(DescriptionColor::Success)
    } else {
        None
    };

    ToolRow {
        label: tool.display_name.clone(),
        value: index.to_string(),
        description,
        description_color,
    }
}

/// Build the full tool list options from a server's filtered tool list.
///
/// The caller is responsible for filtering first (via
/// [`filter_tools_by_server`] or its own source-of-truth).
pub fn build_tool_list_options(tools: &[ToolInfo]) -> Vec<ToolRow> {
    tools
        .iter()
        .enumerate()
        .map(|(i, t)| build_tool_row(t, i))
        .collect()
}

/// Format the dialog subtitle `{n} tool(s)` with English plural
/// rules: `1` → `"1 tool"`, otherwise `"{n} tools"`.
pub fn format_tool_subtitle(count: usize) -> String {
    if count == 1 {
        "1 tool".to_string()
    } else {
        format!("{count} tools")
    }
}

/// Format the dialog title `Tools for {server_name}`.
pub fn format_tool_title(server_name: &str) -> String {
    format!("Tools for {server_name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_tool(
        name: &str,
        display: &str,
        server: &str,
        ro: bool,
        dest: bool,
        open: bool,
    ) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            display_name: display.to_string(),
            is_read_only: ro,
            is_destructive: dest,
            is_open_world: open,
            server_name: server.to_string(),
        }
    }

    // --- annotation labels ---

    #[test]
    fn annotation_labels_are_expected() {
        assert_eq!(ToolAnnotationKind::ReadOnly.label(), "read-only");
        assert_eq!(ToolAnnotationKind::Destructive.label(), "destructive");
        assert_eq!(ToolAnnotationKind::OpenWorld.label(), "open-world");
    }

    // --- build_tool_row: annotation ordering ---

    #[test]
    fn row_no_annotations_has_no_description() {
        let t = mk_tool("mcp__s__x", "x", "s", false, false, false);
        let row = build_tool_row(&t, 0);
        assert_eq!(row.label, "x");
        assert_eq!(row.value, "0");
        assert_eq!(row.description, None);
        assert_eq!(row.description_color, None);
    }

    #[test]
    fn row_read_only_only() {
        let t = mk_tool("mcp__s__x", "x", "s", true, false, false);
        let row = build_tool_row(&t, 1);
        assert_eq!(row.description.as_deref(), Some("read-only"));
        assert_eq!(row.description_color, Some(DescriptionColor::Success));
    }

    #[test]
    fn row_destructive_only() {
        let t = mk_tool("mcp__s__x", "x", "s", false, true, false);
        let row = build_tool_row(&t, 2);
        assert_eq!(row.description.as_deref(), Some("destructive"));
        assert_eq!(row.description_color, Some(DescriptionColor::Error));
    }

    #[test]
    fn row_open_world_only() {
        let t = mk_tool("mcp__s__x", "x", "s", false, false, true);
        let row = build_tool_row(&t, 3);
        assert_eq!(row.description.as_deref(), Some("open-world"));
        // Open-world has NO color hint on its own.
        assert_eq!(row.description_color, None);
    }

    #[test]
    fn row_all_three_annotations_joined_in_push_order() {
        // Pin: read-only, destructive, open-world — NOT sorted.
        let t = mk_tool("mcp__s__x", "x", "s", true, true, true);
        let row = build_tool_row(&t, 4);
        assert_eq!(
            row.description.as_deref(),
            Some("read-only, destructive, open-world"),
        );
    }

    #[test]
    fn row_destructive_beats_read_only_for_color() {
        // Both flags set: Error wins over Success.
        let t = mk_tool("mcp__s__x", "x", "s", true, true, false);
        let row = build_tool_row(&t, 0);
        assert_eq!(row.description_color, Some(DescriptionColor::Error));
    }

    #[test]
    fn row_read_only_plus_open_world() {
        let t = mk_tool("mcp__s__x", "x", "s", true, false, true);
        let row = build_tool_row(&t, 5);
        assert_eq!(row.description.as_deref(), Some("read-only, open-world"));
        assert_eq!(row.description_color, Some(DescriptionColor::Success));
    }

    #[test]
    fn row_destructive_plus_open_world_is_error() {
        let t = mk_tool("mcp__s__x", "x", "s", false, true, true);
        let row = build_tool_row(&t, 0);
        assert_eq!(row.description.as_deref(), Some("destructive, open-world"));
        assert_eq!(row.description_color, Some(DescriptionColor::Error));
    }

    #[test]
    fn row_value_is_index_as_string() {
        let t = mk_tool("x", "x", "s", false, false, false);
        assert_eq!(build_tool_row(&t, 0).value, "0");
        assert_eq!(build_tool_row(&t, 17).value, "17");
        assert_eq!(build_tool_row(&t, 999).value, "999");
    }

    // --- build_tool_list_options ---

    #[test]
    fn build_options_empty_list() {
        let tools: Vec<ToolInfo> = vec![];
        assert!(build_tool_list_options(&tools).is_empty());
    }

    #[test]
    fn build_options_preserves_input_order() {
        let tools = vec![
            mk_tool("a", "aa", "s", false, false, false),
            mk_tool("b", "bb", "s", true, false, false),
            mk_tool("c", "cc", "s", false, true, false),
        ];
        let rows = build_tool_list_options(&tools);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].label, "aa");
        assert_eq!(rows[1].label, "bb");
        assert_eq!(rows[2].label, "cc");
        assert_eq!(rows[0].value, "0");
        assert_eq!(rows[1].value, "1");
        assert_eq!(rows[2].value, "2");
    }

    #[test]
    fn build_options_indices_are_consecutive() {
        let tools: Vec<ToolInfo> = (0..5)
            .map(|i| mk_tool("x", &format!("t{i}"), "s", false, false, false))
            .collect();
        let rows = build_tool_list_options(&tools);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.value, i.to_string());
        }
    }

    // --- filter_tools_by_server ---

    #[test]
    fn filter_by_server_exact_match() {
        let tools = vec![
            mk_tool("x", "x", "linear", false, false, false),
            mk_tool("y", "y", "github", false, false, false),
            mk_tool("z", "z", "linear", false, false, false),
        ];
        let filtered = filter_tools_by_server(&tools, "linear");
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].name, "x");
        assert_eq!(filtered[1].name, "z");
    }

    #[test]
    fn filter_by_server_empty_on_mismatch() {
        let tools = vec![mk_tool("x", "x", "linear", false, false, false)];
        assert!(filter_tools_by_server(&tools, "slack").is_empty());
    }

    #[test]
    fn filter_by_server_case_sensitive() {
        let tools = vec![mk_tool("x", "x", "linear", false, false, false)];
        assert!(filter_tools_by_server(&tools, "Linear").is_empty());
    }

    #[test]
    fn filter_by_server_empty_name_matches_nothing() {
        let tools = vec![mk_tool("x", "x", "linear", false, false, false)];
        assert!(filter_tools_by_server(&tools, "").is_empty());
    }

    // --- subtitle / title ---

    #[test]
    fn subtitle_plural_rules() {
        assert_eq!(format_tool_subtitle(0), "0 tools");
        assert_eq!(format_tool_subtitle(1), "1 tool");
        assert_eq!(format_tool_subtitle(2), "2 tools");
        assert_eq!(format_tool_subtitle(17), "17 tools");
    }

    #[test]
    fn title_formatting() {
        assert_eq!(format_tool_title("linear"), "Tools for linear");
        assert_eq!(format_tool_title(""), "Tools for ");
        assert_eq!(
            format_tool_title("my.server:8080"),
            "Tools for my.server:8080"
        );
    }
}
