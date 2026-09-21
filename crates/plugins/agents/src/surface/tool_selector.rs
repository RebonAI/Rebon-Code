//! Tool-selector reducer ([`ToolSelectorState`]).
//!
//! 1. Filtering the available tools down to what an agent may use is
//!    the consumer's responsibility — this module operates on the
//!    pre-filtered list.
//! 2. Buckets each tool into one of `read_only` / `edit` / `execution`
//!    / `mcp` / `other`. The bucket name lookup is a static set
//!    pinned by tool name.
//! 3. Tracks which tools are currently selected, with bulk
//!    "select-all-in-bucket" + "select-all-tools" actions.
//! 4. On confirm, if EVERY tool is selected, the result is `None`
//!    (behavioral for "all tools allowed"). Otherwise, the explicit
//!    selected list is returned.
//!
//! The Rust implementation models all of this as pure functions + a small
//! reducer. The tool registry / MCP detection lives upstream.

use std::collections::BTreeSet;

/// One tool the user can select. This module doesn't care about
/// `description` or `inputSchema` — those are upstream concerns. The
/// `is_mcp` flag drives the MCP bucket; the `name` is matched against
/// the bucket sets.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolEntry {
    /// Tool name (e.g. `BashTool`, `FileReadTool`, `mcp__server__name`).
    pub name: String,
    /// True if this is an MCP tool (forces it into the MCP bucket).
    pub is_mcp: bool,
}

impl ToolEntry {
    /// Convenience constructor.
    pub fn new(name: impl Into<String>, is_mcp: bool) -> Self {
        ToolEntry {
            name: name.into(),
            is_mcp,
        }
    }
}

/// The five buckets a tool can land in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolBucket {
    /// Read-only tools (Glob, Grep, FileRead, …).
    ReadOnly,
    /// Tools that mutate files (FileEdit, FileWrite, NotebookEdit).
    Edit,
    /// Tools that execute code (BashTool, …).
    Execution,
    /// MCP tools (dynamically grouped).
    Mcp,
    /// Anything else (catch-all).
    Other,
}

impl ToolBucket {
    /// Display name shown for this bucket.
    pub fn display_name(self) -> &'static str {
        match self {
            ToolBucket::ReadOnly => "Read-only tools",
            ToolBucket::Edit => "Edit tools",
            ToolBucket::Execution => "Execution tools",
            ToolBucket::Mcp => "MCP tools",
            ToolBucket::Other => "Other tools",
        }
    }

    /// Stable identifier used by the navigable-items list
    /// (`bucket-readonly`, `bucket-edit`, …).
    pub fn nav_id(self) -> &'static str {
        match self {
            ToolBucket::ReadOnly => "bucket-readonly",
            ToolBucket::Edit => "bucket-edit",
            ToolBucket::Execution => "bucket-execution",
            ToolBucket::Mcp => "bucket-mcp",
            ToolBucket::Other => "bucket-other",
        }
    }
}

/// Which bucket one tool lands in.
///
/// The name-to-bucket question is answered by `classify`, which the caller
/// supplies: this crate takes no Cargo dependency on a tool registry, and the
/// list of builtin tool names is a fact that lives in one place
/// (`rebon-tools-core`) rather than in a copy here. It had a copy, and the
/// copy had already drifted by one alias.
///
/// `None` from the classifier means "leave this tool out of the picker
/// entirely" -- the Agent tool is the one that answers that way.
///
/// Whether a tool is an MCP tool stays here: that is a fact about this run,
/// not about the tool, so no table can answer it.
pub fn classify_tool(
    tool: &ToolEntry,
    classify: &dyn Fn(&str) -> Option<ToolBucket>,
) -> Option<ToolBucket> {
    if tool.is_mcp {
        return Some(ToolBucket::Mcp);
    }
    classify(&tool.name)
}

/// Bucketed projection of a tool list, one field per [`ToolBucket`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BucketedTools {
    /// Read-only bucket.
    pub read_only: Vec<ToolEntry>,
    /// Edit bucket.
    pub edit: Vec<ToolEntry>,
    /// Execution bucket.
    pub execution: Vec<ToolEntry>,
    /// MCP bucket.
    pub mcp: Vec<ToolEntry>,
    /// Other bucket.
    pub other: Vec<ToolEntry>,
}

impl BucketedTools {
    /// Build the bucketed projection. `classify` is the caller's name-to-bucket
    /// answer -- see [`classify_tool`].
    pub fn from_tools(tools: &[ToolEntry], classify: &dyn Fn(&str) -> Option<ToolBucket>) -> Self {
        let mut out = BucketedTools::default();
        for tool in tools {
            match classify_tool(tool, classify) {
                Some(ToolBucket::ReadOnly) => out.read_only.push(tool.clone()),
                Some(ToolBucket::Edit) => out.edit.push(tool.clone()),
                Some(ToolBucket::Execution) => out.execution.push(tool.clone()),
                Some(ToolBucket::Mcp) => out.mcp.push(tool.clone()),
                Some(ToolBucket::Other) => out.other.push(tool.clone()),
                None => {} // the classifier keeps this one out
            }
        }
        out
    }

    /// Get the tools in a given bucket.
    pub fn get(&self, bucket: ToolBucket) -> &[ToolEntry] {
        match bucket {
            ToolBucket::ReadOnly => &self.read_only,
            ToolBucket::Edit => &self.edit,
            ToolBucket::Execution => &self.execution,
            ToolBucket::Mcp => &self.mcp,
            ToolBucket::Other => &self.other,
        }
    }
}

/// Reducer state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSelectorState {
    /// All tools the user can pick from (the consumer-pre-filtered
    /// list).
    pub tools: Vec<ToolEntry>,
    /// Currently-selected tool names.
    pub selected: BTreeSet<String>,
    /// Bucketed projection (computed once at construction).
    pub buckets: BucketedTools,
}

impl ToolSelectorState {
    /// Build a fresh state.
    ///
    /// `initial` picks the starting selection:
    /// - `None` or `Some(["*"])` → all tools selected.
    /// - `Some(list)` → exactly those tools selected.
    pub fn new(
        tools: Vec<ToolEntry>,
        initial: Option<&[String]>,
        classify: &dyn Fn(&str) -> Option<ToolBucket>,
    ) -> Self {
        let buckets = BucketedTools::from_tools(&tools, classify);
        let selected: BTreeSet<String> = match initial {
            None => tools.iter().map(|t| t.name.clone()).collect(),
            Some(list) if list.iter().any(|s| s == "*") => {
                tools.iter().map(|t| t.name.clone()).collect()
            }
            Some(list) => list.iter().cloned().collect(),
        };
        ToolSelectorState {
            tools,
            selected,
            buckets,
        }
    }

    /// Toggle a single tool.
    pub fn toggle_tool(mut self, name: &str) -> Self {
        if self.selected.contains(name) {
            self.selected.remove(name);
        } else {
            self.selected.insert(name.to_string());
        }
        self
    }

    /// Bulk-toggle a set of tools.
    pub fn toggle_tools(mut self, names: &[String], select: bool) -> Self {
        if select {
            for n in names {
                self.selected.insert(n.clone());
            }
        } else {
            for n in names {
                self.selected.remove(n);
            }
        }
        self
    }

    /// Toggle every tool in a bucket.
    /// If any tool in the bucket is unselected, this selects them ALL;
    /// otherwise it deselects them all.
    pub fn toggle_bucket(self, bucket: ToolBucket) -> Self {
        let bucket_tools = self.buckets.get(bucket).to_vec();
        let names: Vec<String> = bucket_tools.iter().map(|t| t.name.clone()).collect();
        let needs_selection = bucket_tools
            .iter()
            .any(|t| !self.selected.contains(&t.name));
        self.toggle_tools(&names, needs_selection)
    }

    /// Toggle the "All tools" pseudo-action. If all tools are
    /// currently selected, deselect everything; otherwise select
    /// everything.
    pub fn toggle_all(mut self) -> Self {
        if self.is_all_selected() {
            self.selected.clear();
        } else {
            for t in &self.tools {
                self.selected.insert(t.name.clone());
            }
        }
        self
    }

    /// True iff every available tool is selected and there's at
    /// least one tool.
    pub fn is_all_selected(&self) -> bool {
        !self.tools.is_empty() && self.tools.iter().all(|t| self.selected.contains(&t.name))
    }

    /// Filter the selected list to tools that still exist.
    pub fn valid_selected(&self) -> Vec<String> {
        let names: BTreeSet<&str> = self.tools.iter().map(|t| t.name.as_str()).collect();
        let mut out: Vec<String> = self
            .selected
            .iter()
            .filter(|n| names.contains(n.as_str()))
            .cloned()
            .collect();
        out.sort();
        out
    }

    /// Compute the confirm payload.
    ///
    /// If every tool is selected, the result is `None` ("all tools
    /// allowed"). Otherwise, the explicit list is returned.
    pub fn confirm(&self) -> Option<Vec<String>> {
        let valid = self.valid_selected();
        if valid.len() == self.tools.len() && !self.tools.is_empty() {
            None
        } else {
            Some(valid)
        }
    }

    /// Count of selected tools in a bucket.
    pub fn bucket_selected_count(&self, bucket: ToolBucket) -> usize {
        self.buckets
            .get(bucket)
            .iter()
            .filter(|t| self.selected.contains(&t.name))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str) -> ToolEntry {
        ToolEntry::new(name, false)
    }

    /// A stand-in for the caller's classifier. Deliberately tiny and local:
    /// the real one reads `rebon-tools-core`, and this crate must not need it
    /// to exercise its own reducer.
    fn classify(name: &str) -> Option<ToolBucket> {
        match name {
            "FileReadTool" | "GrepTool" => Some(ToolBucket::ReadOnly),
            "FileWriteTool" => Some(ToolBucket::Edit),
            "BashTool" => Some(ToolBucket::Execution),
            "Agent" => None,
            _ => Some(ToolBucket::Other),
        }
    }

    fn state(tools: Vec<ToolEntry>, initial: Option<&[String]>) -> ToolSelectorState {
        ToolSelectorState::new(tools, initial, &classify)
    }

    fn mcp(name: &str) -> ToolEntry {
        ToolEntry::new(name, true)
    }

    #[test]
    fn classify_read_only() {
        assert_eq!(
            classify_tool(&t("FileReadTool"), &classify),
            Some(ToolBucket::ReadOnly)
        );
    }

    #[test]
    fn classify_edit() {
        assert_eq!(
            classify_tool(&t("FileWriteTool"), &classify),
            Some(ToolBucket::Edit)
        );
    }

    #[test]
    fn classify_execution() {
        assert_eq!(
            classify_tool(&t("BashTool"), &classify),
            Some(ToolBucket::Execution)
        );
    }

    #[test]
    fn classify_mcp_overrides_name() {
        let m = ToolEntry::new("FileReadTool", true);
        assert_eq!(classify_tool(&m, &classify), Some(ToolBucket::Mcp));
    }

    #[test]
    fn classify_other_catches_unknown() {
        assert_eq!(
            classify_tool(&t("MysteryTool"), &classify),
            Some(ToolBucket::Other)
        );
    }

    #[test]
    fn classify_skips_agent_tool_name() {
        assert_eq!(classify_tool(&t("Agent"), &classify), None);
    }

    #[test]
    fn stale_task_tool_name_is_other() {
        assert_eq!(
            classify_tool(&t("TaskTool"), &classify),
            Some(ToolBucket::Other)
        );
    }

    #[test]
    fn buckets_distribute_correctly() {
        let tools = vec![
            t("FileReadTool"),
            t("FileWriteTool"),
            t("BashTool"),
            mcp("mcp__x"),
            t("UnknownTool"),
            t("Agent"),
        ];
        let b = BucketedTools::from_tools(&tools, &classify);
        assert_eq!(b.read_only.len(), 1);
        assert_eq!(b.edit.len(), 1);
        assert_eq!(b.execution.len(), 1);
        assert_eq!(b.mcp.len(), 1);
        assert_eq!(b.other.len(), 1);
    }

    #[test]
    fn new_with_none_selects_all() {
        let tools = vec![t("FileReadTool"), t("BashTool")];
        let s = state(tools, None);
        assert_eq!(s.selected.len(), 2);
        assert!(s.is_all_selected());
    }

    #[test]
    fn new_with_star_selects_all() {
        let tools = vec![t("FileReadTool"), t("BashTool")];
        let s = state(tools, Some(&["*".to_string()]));
        assert!(s.is_all_selected());
    }

    #[test]
    fn new_with_explicit_list_selects_those() {
        let tools = vec![t("FileReadTool"), t("BashTool")];
        let s = state(tools, Some(&["FileReadTool".to_string()]));
        assert_eq!(s.selected.len(), 1);
        assert!(s.selected.contains("FileReadTool"));
    }

    #[test]
    fn toggle_tool_adds_then_removes() {
        let tools = vec![t("FileReadTool"), t("BashTool")];
        let s = state(tools, Some(&[]));
        let s = s.toggle_tool("FileReadTool");
        assert!(s.selected.contains("FileReadTool"));
        let s = s.toggle_tool("FileReadTool");
        assert!(!s.selected.contains("FileReadTool"));
    }

    #[test]
    fn toggle_bucket_selects_when_partial() {
        let tools = vec![t("FileReadTool"), t("GrepTool")];
        let s = state(tools, Some(&["FileReadTool".to_string()]));
        let s = s.toggle_bucket(ToolBucket::ReadOnly);
        assert!(s.selected.contains("FileReadTool"));
        assert!(s.selected.contains("GrepTool"));
    }

    #[test]
    fn toggle_bucket_deselects_when_full() {
        let tools = vec![t("FileReadTool"), t("GrepTool")];
        let s = state(tools, None);
        let s = s.toggle_bucket(ToolBucket::ReadOnly);
        assert!(!s.selected.contains("FileReadTool"));
        assert!(!s.selected.contains("GrepTool"));
    }

    #[test]
    fn toggle_all_when_partial_selects_all() {
        let tools = vec![t("FileReadTool"), t("BashTool")];
        let s = state(tools, Some(&["FileReadTool".to_string()]));
        let s = s.toggle_all();
        assert!(s.is_all_selected());
    }

    #[test]
    fn toggle_all_when_full_clears() {
        let tools = vec![t("FileReadTool"), t("BashTool")];
        let s = state(tools, None);
        let s = s.toggle_all();
        assert!(s.selected.is_empty());
    }

    #[test]
    fn confirm_all_returns_none() {
        let tools = vec![t("FileReadTool"), t("BashTool")];
        let s = state(tools, None);
        assert_eq!(s.confirm(), None);
    }

    #[test]
    fn confirm_partial_returns_explicit() {
        let tools = vec![t("FileReadTool"), t("BashTool")];
        let s = state(tools, Some(&["FileReadTool".to_string()]));
        assert_eq!(s.confirm(), Some(vec!["FileReadTool".to_string()]));
    }

    #[test]
    fn confirm_empty_tools_returns_empty_list() {
        let s = state(vec![], None);
        // Empty tools list — confirm returns Some(vec![]).
        assert_eq!(s.confirm(), Some(vec![]));
    }

    #[test]
    fn valid_selected_filters_unknown_names() {
        let tools = vec![t("FileReadTool")];
        let s = state(
            tools,
            Some(&["FileReadTool".to_string(), "MissingTool".to_string()]),
        );
        let valid = s.valid_selected();
        assert_eq!(valid, vec!["FileReadTool".to_string()]);
    }

    #[test]
    fn bucket_selected_count() {
        let tools = vec![t("FileReadTool"), t("GrepTool"), t("BashTool")];
        let s = state(tools, Some(&["FileReadTool".to_string()]));
        assert_eq!(s.bucket_selected_count(ToolBucket::ReadOnly), 1);
        assert_eq!(s.bucket_selected_count(ToolBucket::Execution), 0);
    }

    #[test]
    fn bucket_display_names_pinned() {
        assert_eq!(ToolBucket::ReadOnly.display_name(), "Read-only tools");
        assert_eq!(ToolBucket::Edit.display_name(), "Edit tools");
        assert_eq!(ToolBucket::Execution.display_name(), "Execution tools");
        assert_eq!(ToolBucket::Mcp.display_name(), "MCP tools");
        assert_eq!(ToolBucket::Other.display_name(), "Other tools");
    }

    #[test]
    fn bucket_nav_ids_pinned() {
        assert_eq!(ToolBucket::ReadOnly.nav_id(), "bucket-readonly");
        assert_eq!(ToolBucket::Edit.nav_id(), "bucket-edit");
        assert_eq!(ToolBucket::Mcp.nav_id(), "bucket-mcp");
    }
}
