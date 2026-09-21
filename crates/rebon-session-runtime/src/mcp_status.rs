//! The owner's view of its MCP servers, as one value a client can be handed.
//!
//! Built here from a session that hosts the servers, carried on the owner's
//! `Status` snapshot, and drawn by `/mcp` — on the host from the session
//! itself, on a mirror from what the owner last said.

use std::path::Path;

use rebon_session_host::{McpServerSnapshot, McpStatusSnapshot, McpToolSnapshot};

use crate::mcp::{SessionMcp, TuiMcpLoadStatus};
use crate::EngineSession;

/// What this session's MCP servers look like, or `None` when this process
/// does not host them.
pub fn owner_mcp_snapshot(session: &EngineSession) -> Option<McpStatusSnapshot> {
    let mcp = session.engine_half.mcp.as_ref()?;
    let mut snapshot = stack_snapshot(
        mcp,
        Path::new(&session.cwd),
        &session.startup.mcp_configs,
        session.startup.strict_mcp_config,
    );
    // A tool the engine registered under the MCP prefix is one the proxy
    // brought up, whichever client it came through.
    for tool in session.engine_half.engine.eager_tool_snapshots() {
        if tool.name.starts_with("mcp__")
            && !snapshot.tools.iter().any(|known| known.name == tool.name)
        {
            snapshot.tools.push(McpToolSnapshot {
                name: tool.name,
                tokens: approximate_tokens(&tool.description)
                    + approximate_tokens(&tool.input_schema.to_string()),
            });
        }
    }
    snapshot
        .tools
        .sort_by(|left, right| left.name.cmp(&right.name));
    Some(snapshot)
}

/// What a held MCP stack looks like, with no session around it.
///
/// A worker keeps its stack between sessions, and a load can land in that
/// gap; this is what it publishes then, so a mirror asking in the gap sees
/// the servers rather than nothing.
pub(crate) fn stack_snapshot(
    mcp: &SessionMcp,
    cwd: &Path,
    mcp_configs: &[String],
    strict_mcp_config: bool,
) -> McpStatusSnapshot {
    let (servers, config_error) = configured_servers(cwd, mcp_configs, strict_mcp_config);
    let (mut warnings, error) = match &mcp.load_status {
        TuiMcpLoadStatus::Ready { warnings } => (warnings.clone(), None),
        TuiMcpLoadStatus::Failed { error } => (Vec::new(), Some(error.clone())),
        TuiMcpLoadStatus::Loading | TuiMcpLoadStatus::NotConfigured => (Vec::new(), None),
    };
    warnings.extend(config_error);

    let mut tools = Vec::new();
    for server in mcp.client.server_names() {
        for definition in mcp
            .client
            .cached_tool_definitions(&server)
            .unwrap_or_default()
        {
            tools.push(McpToolSnapshot {
                name: rebon_tool::build_mcp_tool_name(&server, &definition.name),
                tokens: approximate_tokens(&definition.description)
                    + approximate_tokens(&definition.input_schema.to_string()),
            });
        }
    }
    tools.sort_by(|left, right| left.name.cmp(&right.name));

    McpStatusSnapshot {
        loader: load_status_label(&mcp.load_status).to_string(),
        client: client_label(mcp).to_string(),
        servers,
        tools,
        warnings,
        error,
    }
}

/// What a process that hosts no servers, and has heard nothing from whoever
/// does, has to show.
pub fn not_hosted_here() -> McpStatusSnapshot {
    McpStatusSnapshot {
        loader: "held by the session's owner".into(),
        client: "not connected".into(),
        ..McpStatusSnapshot::default()
    }
}

/// The servers the configuration names, whether or not they came up, and
/// the one line to show when the configuration itself does not read.
fn configured_servers(
    cwd: &Path,
    mcp_configs: &[String],
    strict_mcp_config: bool,
) -> (Vec<McpServerSnapshot>, Option<String>) {
    match crate::mcp_config::collect_default_mcp_configs_with_overrides(
        cwd,
        mcp_configs,
        strict_mcp_config,
    ) {
        Ok(configs) => (
            configs
                .into_iter()
                .map(|item| McpServerSnapshot {
                    name: item.config.name().to_string(),
                    transport: transport_label(&item.config).to_string(),
                    source: item.source,
                })
                .collect(),
            None,
        ),
        Err(error) => (Vec::new(), Some(format!("config: {error}"))),
    }
}

fn transport_label(config: &crate::mcp_config::McpServerConfig) -> &'static str {
    match config {
        crate::mcp_config::McpServerConfig::Stdio(_) => "stdio",
        crate::mcp_config::McpServerConfig::Http(_) => "http",
        crate::mcp_config::McpServerConfig::Sse(_) => "sse",
    }
}

fn load_status_label(status: &TuiMcpLoadStatus) -> &'static str {
    match status {
        TuiMcpLoadStatus::Loading => "loading",
        TuiMcpLoadStatus::Ready { .. } => "ready",
        TuiMcpLoadStatus::Failed { .. } => "failed",
        TuiMcpLoadStatus::NotConfigured => "not configured",
    }
}

fn client_label(mcp: &SessionMcp) -> &'static str {
    match mcp.load_status {
        TuiMcpLoadStatus::Ready { .. } if mcp.delayed.is_ready() => "ready",
        TuiMcpLoadStatus::Loading => "pending",
        _ => "not connected",
    }
}

fn approximate_tokens(text: &str) -> u64 {
    (text.len() / 4) as u64
}
