//! Approve/approve-all/reject for a newly-discovered MCP server in
//! `.mcp.json`.
//!
//! ## Behaviour
//!
//! * The server-name-templated title.
//! * The three-option list (`yes_all`, `yes`, `no`), in that order.
//! * The settings write-through behaviour for each branch
//!   (modeled as a [`McpServerApprovalAction`] variant).
//! * The cancel branch (defaults to `no`).
//!
//! ## Outbound seam
//!
//! The dialog reads the existing `enabled_mcpjson_servers` /
//! `disabled_mcpjson_servers` arrays from settings and only writes when
//! the server name isn't already present. This module exposes
//! [`apply_settings_update`] so the consumer can pass the existing
//! arrays in and get the new arrays back.

use crate::common::{DialogColor, SelectOption};

/// The dialog frame color.
pub const DIALOG_COLOR: DialogColor = DialogColor::Warning;

/// Build the templated title.
pub fn build_title(server_name: &str) -> String {
    format!("New MCP server found in .mcp.json: {server_name}")
}

/// Option values: `yes`, `yes_all` or `no`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerApprovalValue {
    /// "Use this and all future MCP servers in this project".
    YesAll,
    /// "Use this MCP server".
    Yes,
    /// "Continue without using this MCP server".
    No,
}

/// Action emitted by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerApprovalAction {
    /// Approve the server (and optionally also enable-all-future).
    Approve {
        /// The server name to add to the enabled list.
        server_name: String,
        /// True if the user picked `yes_all` (also writes
        /// `enable_all_project_mcp_servers = true`).
        enable_all: bool,
    },
    /// Reject the server.
    Reject {
        /// The server name to add to the disabled list.
        server_name: String,
    },
}

/// Build the option list (`yes_all` first, `yes` second,
/// `no` third).
pub fn build_options() -> Vec<SelectOption<McpServerApprovalValue>> {
    vec![
        SelectOption::new(
            "Use this and all future MCP servers in this project",
            McpServerApprovalValue::YesAll,
        ),
        SelectOption::new("Use this MCP server", McpServerApprovalValue::Yes),
        SelectOption::new(
            "Continue without using this MCP server",
            McpServerApprovalValue::No,
        ),
    ]
}

/// Reducer.
pub fn handle_event(value: McpServerApprovalValue, server_name: &str) -> McpServerApprovalAction {
    match value {
        McpServerApprovalValue::Yes => McpServerApprovalAction::Approve {
            server_name: server_name.to_string(),
            enable_all: false,
        },
        McpServerApprovalValue::YesAll => McpServerApprovalAction::Approve {
            server_name: server_name.to_string(),
            enable_all: true,
        },
        McpServerApprovalValue::No => McpServerApprovalAction::Reject {
            server_name: server_name.to_string(),
        },
    }
}

/// Cancel handler — defaults to `no`.
pub fn handle_cancel(server_name: &str) -> McpServerApprovalAction {
    handle_event(McpServerApprovalValue::No, server_name)
}

/// Apply the approval to a settings snapshot. Returns `Some(new_list)`
/// when the server should be added (i.e. it's not already present),
/// otherwise `None`.
pub fn apply_settings_update(existing: &[String], server_name: &str) -> Option<Vec<String>> {
    if existing.iter().any(|s| s == server_name) {
        return None;
    }
    let mut updated: Vec<String> = existing.to_vec();
    updated.push(server_name.to_string());
    Some(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_color_is_warning() {
        assert_eq!(DIALOG_COLOR, DialogColor::Warning);
    }

    #[test]
    fn build_title_templates_server_name() {
        assert_eq!(
            build_title("my-server"),
            "New MCP server found in .mcp.json: my-server"
        );
    }

    #[test]
    fn build_options_order_yesall_yes_no() {
        let opts = build_options();
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].value, McpServerApprovalValue::YesAll);
        assert_eq!(opts[1].value, McpServerApprovalValue::Yes);
        assert_eq!(opts[2].value, McpServerApprovalValue::No);
    }

    #[test]
    fn handle_event_yes() {
        let action = handle_event(McpServerApprovalValue::Yes, "foo");
        assert_eq!(
            action,
            McpServerApprovalAction::Approve {
                server_name: "foo".into(),
                enable_all: false,
            }
        );
    }

    #[test]
    fn handle_event_yes_all_sets_enable_all() {
        let action = handle_event(McpServerApprovalValue::YesAll, "foo");
        assert_eq!(
            action,
            McpServerApprovalAction::Approve {
                server_name: "foo".into(),
                enable_all: true,
            }
        );
    }

    #[test]
    fn handle_event_no() {
        let action = handle_event(McpServerApprovalValue::No, "foo");
        assert_eq!(
            action,
            McpServerApprovalAction::Reject {
                server_name: "foo".into()
            }
        );
    }

    #[test]
    fn handle_cancel_defaults_to_reject() {
        assert_eq!(
            handle_cancel("bar"),
            McpServerApprovalAction::Reject {
                server_name: "bar".into()
            }
        );
    }

    #[test]
    fn apply_settings_update_adds_when_missing() {
        let existing = vec!["a".to_string(), "b".to_string()];
        let updated = apply_settings_update(&existing, "c");
        assert_eq!(updated, Some(vec!["a".into(), "b".into(), "c".into()]));
    }

    #[test]
    fn apply_settings_update_skips_when_present() {
        let existing = vec!["a".to_string(), "b".to_string()];
        let updated = apply_settings_update(&existing, "a");
        assert_eq!(updated, None);
    }

    #[test]
    fn apply_settings_update_empty() {
        let updated = apply_settings_update(&[], "x");
        assert_eq!(updated, Some(vec!["x".into()]));
    }
}
