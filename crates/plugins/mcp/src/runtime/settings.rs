//! MCP settings tab — projection from MCP state into a settings view.
//!
//! The settings tab has a row per server plus meta rows (enable MCP
//! globally, add server, import from Claude Desktop, etc.). The
//! projection is a pure [`McpSettingsView`] of [`McpSettingsRow`]s — the
//! consumer renders and handles events.

use crate::runtime::config::{ConfigScope, TransportKind};
use crate::runtime::status::McpServerStatus;

/// One row in the settings tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSettingsRow {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub kind: McpSettingsRowKind,
}

/// The kind of settings row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSettingsRowKind {
    /// A server in the list. Carries the state needed for the
    /// right-hand status badge.
    Server {
        name: String,
        status: McpServerStatus,
        scope: ConfigScope,
        transport: TransportKind,
        enabled: bool,
    },
    /// The "Add server…" action row.
    AddServer,
    /// The "Import from Claude Desktop" action row.
    ImportFromClaudeDesktop,
    /// The "Enable MCP globally" toggle row. The bool is the
    /// current value.
    EnableGlobal(bool),
    /// A separator / section header row. `title` is the text.
    Section { title: String },
}

/// The full settings tab view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McpSettingsView {
    pub rows: Vec<McpSettingsRow>,
}

/// The context handed in by the consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSettingsContext {
    /// Whether MCP is globally enabled.
    pub mcp_enabled: bool,
    /// Whether to show the "Import from Claude Desktop" row.
    pub show_claude_desktop_import: bool,
    /// Whether to show the "Add server" row.
    pub show_add_server: bool,
    /// The current server list (in display order).
    pub servers: Vec<McpSettingsServer>,
}

/// The per-server input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSettingsServer {
    pub name: String,
    pub status: McpServerStatus,
    pub scope: ConfigScope,
    pub transport: TransportKind,
    pub enabled: bool,
}

/// The id used for the global enable row.
pub const ROW_ID_ENABLE_GLOBAL: &str = "mcp.enable_global";
/// The id used for the add-server row.
pub const ROW_ID_ADD_SERVER: &str = "mcp.add_server";
/// The id used for the Claude Desktop import row.
pub const ROW_ID_IMPORT_CLAUDE_DESKTOP: &str = "mcp.import_claude_desktop";
/// The id prefix for per-server rows (`mcp.server.{name}`).
pub const ROW_ID_SERVER_PREFIX: &str = "mcp.server.";
/// The id for the "Servers" section header.
pub const ROW_ID_SECTION_SERVERS: &str = "mcp.section.servers";

/// Build the MCP settings view from a context.
pub fn build_mcp_settings_view(ctx: &McpSettingsContext) -> McpSettingsView {
    let mut rows = Vec::new();

    // 1. Global enable toggle — first row.
    rows.push(McpSettingsRow {
        id: ROW_ID_ENABLE_GLOBAL.to_string(),
        label: "Enable MCP".to_string(),
        description: Some("Allow Rebon to call tools on MCP servers.".to_string()),
        kind: McpSettingsRowKind::EnableGlobal(ctx.mcp_enabled),
    });

    // 2. Servers section header (only if there are servers).
    if !ctx.servers.is_empty() {
        rows.push(McpSettingsRow {
            id: ROW_ID_SECTION_SERVERS.to_string(),
            label: "Servers".to_string(),
            description: None,
            kind: McpSettingsRowKind::Section {
                title: "Servers".to_string(),
            },
        });
    }

    // 3. Per-server rows.
    for server in &ctx.servers {
        rows.push(McpSettingsRow {
            id: format!("{}{}", ROW_ID_SERVER_PREFIX, server.name),
            label: server.name.clone(),
            description: Some(format_server_description(server)),
            kind: McpSettingsRowKind::Server {
                name: server.name.clone(),
                status: server.status,
                scope: server.scope,
                transport: server.transport,
                enabled: server.enabled,
            },
        });
    }

    // 4. Add server row (after the server list).
    if ctx.show_add_server {
        rows.push(McpSettingsRow {
            id: ROW_ID_ADD_SERVER.to_string(),
            label: "Add server…".to_string(),
            description: None,
            kind: McpSettingsRowKind::AddServer,
        });
    }

    // 5. Import row (always last when shown).
    if ctx.show_claude_desktop_import {
        rows.push(McpSettingsRow {
            id: ROW_ID_IMPORT_CLAUDE_DESKTOP.to_string(),
            label: "Import from Claude Desktop".to_string(),
            description: None,
            kind: McpSettingsRowKind::ImportFromClaudeDesktop,
        });
    }

    McpSettingsView { rows }
}

/// Format the per-server description line:
/// `{status} · {scope} · {transport}`.
pub fn format_server_description(server: &McpSettingsServer) -> String {
    format!(
        "{} \u{00B7} {} \u{00B7} {}",
        status_label(server.status),
        server.scope.as_str(),
        server.transport.as_str()
    )
}

/// Human-readable status labels used in descriptions. Pinned.
pub fn status_label(s: McpServerStatus) -> &'static str {
    match s {
        McpServerStatus::Pending => "connecting",
        McpServerStatus::Connected => "connected",
        McpServerStatus::Failed => "failed",
        McpServerStatus::NeedsAuth => "needs auth",
        McpServerStatus::Disabled => "disabled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_server(name: &str, status: McpServerStatus) -> McpSettingsServer {
        McpSettingsServer {
            name: name.to_string(),
            status,
            scope: ConfigScope::User,
            transport: TransportKind::Stdio,
            enabled: true,
        }
    }

    fn mk_ctx(servers: Vec<McpSettingsServer>) -> McpSettingsContext {
        McpSettingsContext {
            mcp_enabled: true,
            show_claude_desktop_import: true,
            show_add_server: true,
            servers,
        }
    }

    // --- Row IDs pinned ---

    #[test]
    fn row_id_constants() {
        assert_eq!(ROW_ID_ENABLE_GLOBAL, "mcp.enable_global");
        assert_eq!(ROW_ID_ADD_SERVER, "mcp.add_server");
        assert_eq!(ROW_ID_IMPORT_CLAUDE_DESKTOP, "mcp.import_claude_desktop");
        assert_eq!(ROW_ID_SERVER_PREFIX, "mcp.server.");
    }

    // --- Global enable row always first ---

    #[test]
    fn first_row_is_enable_global() {
        let view = build_mcp_settings_view(&mk_ctx(vec![]));
        assert_eq!(view.rows[0].id, ROW_ID_ENABLE_GLOBAL);
        assert!(matches!(
            view.rows[0].kind,
            McpSettingsRowKind::EnableGlobal(true)
        ));
    }

    #[test]
    fn enable_global_reflects_ctx_flag() {
        let mut ctx = mk_ctx(vec![]);
        ctx.mcp_enabled = false;
        let view = build_mcp_settings_view(&ctx);
        assert!(matches!(
            view.rows[0].kind,
            McpSettingsRowKind::EnableGlobal(false)
        ));
    }

    // --- Servers section shown only when non-empty ---

    #[test]
    fn no_servers_section_when_empty() {
        let view = build_mcp_settings_view(&mk_ctx(vec![]));
        assert!(!view
            .rows
            .iter()
            .any(|r| matches!(r.kind, McpSettingsRowKind::Section { .. })));
    }

    #[test]
    fn servers_section_shown_when_present() {
        let view = build_mcp_settings_view(&mk_ctx(vec![mk_server(
            "linear",
            McpServerStatus::Connected,
        )]));
        assert!(view
            .rows
            .iter()
            .any(|r| matches!(r.kind, McpSettingsRowKind::Section { .. })));
    }

    // --- Server rows ---

    #[test]
    fn server_row_id_uses_prefix() {
        let view = build_mcp_settings_view(&mk_ctx(vec![mk_server(
            "linear",
            McpServerStatus::Connected,
        )]));
        let server_row = view
            .rows
            .iter()
            .find(|r| matches!(r.kind, McpSettingsRowKind::Server { .. }))
            .unwrap();
        assert_eq!(server_row.id, "mcp.server.linear");
    }

    #[test]
    fn server_rows_in_input_order() {
        let view = build_mcp_settings_view(&mk_ctx(vec![
            mk_server("b", McpServerStatus::Connected),
            mk_server("a", McpServerStatus::Connected),
            mk_server("c", McpServerStatus::Connected),
        ]));
        let server_ids: Vec<&String> = view
            .rows
            .iter()
            .filter(|r| matches!(r.kind, McpSettingsRowKind::Server { .. }))
            .map(|r| &r.id)
            .collect();
        assert_eq!(server_ids.len(), 3);
        assert_eq!(server_ids[0], "mcp.server.b");
        assert_eq!(server_ids[1], "mcp.server.a");
        assert_eq!(server_ids[2], "mcp.server.c");
    }

    // --- Feature-gated rows ---

    #[test]
    fn add_server_row_hidden_when_disabled() {
        let mut ctx = mk_ctx(vec![]);
        ctx.show_add_server = false;
        let view = build_mcp_settings_view(&ctx);
        assert!(!view
            .rows
            .iter()
            .any(|r| matches!(r.kind, McpSettingsRowKind::AddServer)));
    }

    #[test]
    fn import_row_hidden_when_disabled() {
        let mut ctx = mk_ctx(vec![]);
        ctx.show_claude_desktop_import = false;
        let view = build_mcp_settings_view(&ctx);
        assert!(!view
            .rows
            .iter()
            .any(|r| matches!(r.kind, McpSettingsRowKind::ImportFromClaudeDesktop)));
    }

    // --- Row ordering ---

    #[test]
    fn row_order_is_enable_servers_add_import() {
        let view = build_mcp_settings_view(&mk_ctx(vec![mk_server(
            "linear",
            McpServerStatus::Connected,
        )]));
        let ids: Vec<&String> = view.rows.iter().map(|r| &r.id).collect();
        assert_eq!(ids[0], ROW_ID_ENABLE_GLOBAL);
        assert_eq!(ids[1], ROW_ID_SECTION_SERVERS);
        assert_eq!(ids[2], "mcp.server.linear");
        assert_eq!(ids[3], ROW_ID_ADD_SERVER);
        assert_eq!(ids[4], ROW_ID_IMPORT_CLAUDE_DESKTOP);
    }

    #[test]
    fn no_servers_row_order_is_enable_add_import() {
        let view = build_mcp_settings_view(&mk_ctx(vec![]));
        let ids: Vec<&String> = view.rows.iter().map(|r| &r.id).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], ROW_ID_ENABLE_GLOBAL);
        assert_eq!(ids[1], ROW_ID_ADD_SERVER);
        assert_eq!(ids[2], ROW_ID_IMPORT_CLAUDE_DESKTOP);
    }

    // --- format_server_description ---

    #[test]
    fn server_description_uses_middle_dot_separator() {
        let s = mk_server("linear", McpServerStatus::Connected);
        let desc = format_server_description(&s);
        // The separator is U+00B7 MIDDLE DOT with spaces.
        assert_eq!(desc, "connected \u{00B7} user \u{00B7} stdio");
    }

    #[test]
    fn server_description_all_statuses() {
        let cases = [
            (McpServerStatus::Pending, "connecting"),
            (McpServerStatus::Connected, "connected"),
            (McpServerStatus::Failed, "failed"),
            (McpServerStatus::NeedsAuth, "needs auth"),
            (McpServerStatus::Disabled, "disabled"),
        ];
        for (status, expected) in cases {
            let s = mk_server("x", status);
            let desc = format_server_description(&s);
            assert!(desc.starts_with(expected));
        }
    }

    #[test]
    fn status_label_pinned() {
        assert_eq!(status_label(McpServerStatus::Pending), "connecting");
        assert_eq!(status_label(McpServerStatus::Connected), "connected");
        assert_eq!(status_label(McpServerStatus::Failed), "failed");
        assert_eq!(status_label(McpServerStatus::NeedsAuth), "needs auth");
        assert_eq!(status_label(McpServerStatus::Disabled), "disabled");
    }

    #[test]
    fn view_default_is_empty() {
        let view = McpSettingsView::default();
        assert!(view.rows.is_empty());
    }
}
