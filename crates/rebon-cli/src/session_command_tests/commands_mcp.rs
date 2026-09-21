//! Terminal-level tests for `rebon_session_runtime::commands::mcp`.
//!
//! They live here rather than beside the code because each of them
//! builds an `AppState`, calls `crate::session_shell::session_command_inputs_from_app`, or
//! drives the TUI reducer — all three are the binary's, so a crate
//! that must not know what a terminal is cannot host them.

use crate::session::commands::mcp::*;
use crate::session::mcp::TuiMcpLoadStatus;

use crate::tui::runner::test_support::make_test_tui_session;

#[test]
fn mcp_command_uses_loader_snapshot_without_config_reload() {
    let mut session = make_test_tui_session();
    session
        .engine_half
        .mcp
        .as_mut()
        .expect("test session hosts MCP")
        .load_status = TuiMcpLoadStatus::Loading;
    let loading = execute_mcp_command(&session, McpCommand::Status);
    assert!(loading.contains("loader: loading"));
    assert!(loading.contains("client: pending"));
    assert!(!loading.contains("configured servers:"));

    let client = rebon_plugin_mcp::InMemoryMcpClient::new();
    client.register_tool_definition(
        "server",
        rebon_tool::McpToolDefinition {
            name: "ping".into(),
            description: "Ping".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: true,
            destructive: false,
            open_world: false,
            search_hint: None,
            always_load: true,
        },
        serde_json::json!({"ok": true}),
    );
    let mcp = session
        .engine_half
        .mcp
        .as_mut()
        .expect("test session hosts MCP");
    mcp.delayed.set_delegate(std::sync::Arc::new(client));
    mcp.load_status = TuiMcpLoadStatus::Ready {
        warnings: vec!["one server skipped".into()],
    };
    let ready = execute_mcp_command(&session, McpCommand::Status);
    assert!(ready.contains("loader: ready"));
    assert!(ready.contains("warnings: 1"));
    assert!(ready.contains("client: ready"));
    assert!(ready.contains("mcp__server__ping"));

    session
        .engine_half
        .mcp
        .as_mut()
        .expect("test session hosts MCP")
        .load_status = TuiMcpLoadStatus::Failed {
        error: "bad config".into(),
    };
    let failed = execute_mcp_command(&session, McpCommand::Status);
    assert!(failed.contains("loader: failed"));
    assert!(failed.contains("error: bad config"));
    assert!(failed.contains("client: not connected"));
}

/// The snapshot a worker publishes for its mirrors says what `/mcp`
/// would have said on the worker itself.
#[test]
fn owner_mcp_snapshot_reports_the_hosted_servers_and_tools() {
    let mut session = make_test_tui_session();
    let client = rebon_plugin_mcp::InMemoryMcpClient::new();
    client.register_tool_definition(
        "fixture",
        rebon_tool::McpToolDefinition {
            name: "ping".into(),
            description: "Ping the fixture".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: true,
            destructive: false,
            open_world: false,
            search_hint: None,
            always_load: true,
        },
        serde_json::json!({"ok": true}),
    );
    let mcp = session
        .engine_half
        .mcp
        .as_mut()
        .expect("test session hosts MCP");
    mcp.delayed.set_delegate(std::sync::Arc::new(client));
    mcp.load_status = TuiMcpLoadStatus::Ready {
        warnings: vec!["one server skipped".into()],
    };

    let snapshot = crate::mcp_status::owner_mcp_snapshot(&session).expect("a host has a snapshot");
    assert_eq!(snapshot.loader, "ready");
    assert_eq!(snapshot.client, "ready");
    assert_eq!(snapshot.warnings, vec!["one server skipped".to_string()]);
    assert!(snapshot.error.is_none());
    assert_eq!(snapshot.tools.len(), 1);
    assert_eq!(snapshot.tools[0].name, "mcp__fixture__ping");
    assert!(snapshot.tools[0].tokens > 0);

    session.engine_half.mcp = None;
    assert!(
        crate::mcp_status::owner_mcp_snapshot(&session).is_none(),
        "a process that hosts nothing has nothing to publish"
    );
    let text = execute_mcp_command(&session, McpCommand::Status);
    assert!(
        text.contains("loader: held by the session's owner"),
        "{text}"
    );
}

/// `/mcp` grew subcommands, and the parser is what keeps a mistyped one
/// from being sent to the model as a prompt.
#[test]
fn mcp_parses_its_subcommands_and_refuses_the_rest_without_falling_through() {
    assert_eq!(parse_mcp_command("/mcp"), Some(Ok(McpCommand::Status)));
    // Trailing space only: a leading one means the line is talking about
    // the command rather than running it.
    assert_eq!(parse_mcp_command("/mcp  "), Some(Ok(McpCommand::Status)));
    assert_eq!(parse_mcp_command("  /mcp"), None);
    assert_eq!(
        parse_mcp_command("/mcp reconnect fs"),
        Some(Ok(McpCommand::Reconnect("fs".to_string())))
    );
    assert_eq!(
        parse_mcp_command("/mcp disconnect fs"),
        Some(Ok(McpCommand::Disconnect("fs".to_string())))
    );

    // A subcommand that names no server, and one that is not a subcommand
    // at all: both are still `/mcp`, so both get usage rather than a
    // fall-through to the model.
    for text in [
        "/mcp reconnect",
        "/mcp disconnect",
        "/mcp wat",
        "/mcp reconnect a b",
    ] {
        let parsed = parse_mcp_command(text).unwrap_or_else(|| panic!("{text} is an /mcp"));
        assert!(parsed.is_err(), "{text} should be a usage error");
    }

    // Not this command.
    assert_eq!(parse_mcp_command("/mcpx"), None);
    assert_eq!(parse_mcp_command("/model"), None);
    assert_eq!(parse_mcp_command("tell me about /mcp"), None);
}
