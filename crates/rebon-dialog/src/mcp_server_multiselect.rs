//! Multi-select dialog for "N new MCP servers found in .mcp.json"
//! with all-on / all-off transitions.
//!
//! ## Behaviour
//!
//! * The templated title (`{N} new MCP servers found in .mcp.json`).
//! * The submit handler — partition into approved / rejected,
//!   produce `Approve` + `Reject` actions, dedupe against the
//!   existing settings arrays.
//! * The escape-rejects-all branch.

use crate::common::{DialogColor, SelectOption};

/// The dialog frame color (warning).
pub const DIALOG_COLOR: DialogColor = DialogColor::Warning;

/// Build the templated title.
pub fn build_title(server_count: usize) -> String {
    format!("{server_count} new MCP servers found in .mcp.json")
}

/// Build the option list (one per server name).
pub fn build_options(server_names: &[String]) -> Vec<SelectOption<String>> {
    server_names
        .iter()
        .map(|name| SelectOption::new(name.clone(), name.clone()))
        .collect()
}

/// Action emitted by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerMultiselectAction {
    /// Apply approve + reject lists, then close the dialog.
    Apply {
        /// Servers the user picked.
        approved: Vec<String>,
        /// Servers the user did not pick.
        rejected: Vec<String>,
    },
}

/// Reducer for the submit event.
pub fn handle_submit(server_names: &[String], selected: &[String]) -> McpServerMultiselectAction {
    let mut approved: Vec<String> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    for name in server_names {
        if selected.contains(name) {
            approved.push(name.clone());
        } else {
            rejected.push(name.clone());
        }
    }
    McpServerMultiselectAction::Apply { approved, rejected }
}

/// Cancel handler — same effect as submitting an empty selection
/// (rejects all).
pub fn handle_cancel(server_names: &[String]) -> McpServerMultiselectAction {
    handle_submit(server_names, &[])
}

/// Merge a new list into the existing settings array, deduping by
/// equality: existing entries keep their order and new ones are
/// appended.
pub fn merge_dedup(existing: &[String], to_add: &[String]) -> Vec<String> {
    let mut out: Vec<String> = existing.to_vec();
    for s in to_add {
        if !out.iter().any(|x| x == s) {
            out.push(s.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(ns: &[&str]) -> Vec<String> {
        ns.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn build_title_templates() {
        assert_eq!(build_title(3), "3 new MCP servers found in .mcp.json");
        assert_eq!(build_title(0), "0 new MCP servers found in .mcp.json");
    }

    #[test]
    fn build_options_one_per_name() {
        let opts = build_options(&names(&["a", "b"]));
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "a");
        assert_eq!(opts[0].value, "a");
        assert_eq!(opts[1].label, "b");
    }

    #[test]
    fn submit_partitions_correctly() {
        let action = handle_submit(&names(&["a", "b", "c"]), &names(&["a", "c"]));
        match action {
            McpServerMultiselectAction::Apply { approved, rejected } => {
                assert_eq!(approved, names(&["a", "c"]));
                assert_eq!(rejected, names(&["b"]));
            }
        }
    }

    #[test]
    fn submit_empty_selected_rejects_all() {
        let action = handle_submit(&names(&["a", "b"]), &[]);
        match action {
            McpServerMultiselectAction::Apply { approved, rejected } => {
                assert!(approved.is_empty());
                assert_eq!(rejected, names(&["a", "b"]));
            }
        }
    }

    #[test]
    fn submit_all_selected_approves_all() {
        let action = handle_submit(&names(&["a", "b"]), &names(&["a", "b"]));
        match action {
            McpServerMultiselectAction::Apply { approved, rejected } => {
                assert_eq!(approved, names(&["a", "b"]));
                assert!(rejected.is_empty());
            }
        }
    }

    #[test]
    fn cancel_rejects_all() {
        let action = handle_cancel(&names(&["a", "b"]));
        match action {
            McpServerMultiselectAction::Apply {
                approved, rejected, ..
            } => {
                assert!(approved.is_empty());
                assert_eq!(rejected, names(&["a", "b"]));
            }
        }
    }

    #[test]
    fn merge_dedup_preserves_existing_order() {
        let merged = merge_dedup(&names(&["a", "b"]), &names(&["c"]));
        assert_eq!(merged, names(&["a", "b", "c"]));
    }

    #[test]
    fn merge_dedup_skips_duplicates() {
        let merged = merge_dedup(&names(&["a", "b"]), &names(&["b", "c"]));
        assert_eq!(merged, names(&["a", "b", "c"]));
    }

    #[test]
    fn merge_dedup_empty_existing() {
        let merged = merge_dedup(&[], &names(&["a"]));
        assert_eq!(merged, names(&["a"]));
    }
}
