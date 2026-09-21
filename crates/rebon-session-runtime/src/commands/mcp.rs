//! `/mcp`: which MCP servers this session has, and reconnecting one.

use super::fmt_tokens;
use crate::EngineSession;
use rebon_slash_commands::strip_command_prefix;

/// What `/mcp` was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpCommand {
    /// Bare `/mcp` — what is configured, what loaded, which tools came back.
    Status,
    /// `/mcp reconnect <name>` — drop the connection and dial it again with the
    /// configuration it was started with.
    Reconnect(String),
    /// `/mcp disconnect <name>` — drop it and leave it dropped.
    Disconnect(String),
}

/// Recognize `/mcp`, with or without a subcommand.
///
/// Returns `None` for anything that is not `/mcp` at all, so the caller can go
/// on matching other commands; a malformed subcommand is still an `/mcp`, and
/// is reported as a usage error rather than silently falling through to the
/// model as a prompt.
pub fn parse_mcp_command(text: &str) -> Option<Result<McpCommand, String>> {
    let rest = strip_command_prefix(text.trim_end(), "mcp")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        // `/mcpsomething` is not this command.
        return None;
    }
    let mut words = rest.split_whitespace();
    Some(match (words.next(), words.next(), words.next()) {
        (None, _, _) => Ok(McpCommand::Status),
        (Some("reconnect"), Some(name), None) => Ok(McpCommand::Reconnect(name.to_string())),
        (Some("disconnect"), Some(name), None) => Ok(McpCommand::Disconnect(name.to_string())),
        (Some("reconnect" | "disconnect"), None, _) => {
            Err("usage: /mcp reconnect <server> | /mcp disconnect <server>".to_string())
        }
        _ => Err(format!(
            "unknown /mcp subcommand — usage: /mcp | /mcp reconnect <server> | /mcp disconnect <server>{}",
            String::new()
        )),
    })
}

pub fn execute_mcp_command(session: &EngineSession, command: McpCommand) -> String {
    match command {
        McpCommand::Status => execute_mcp_status(session),
        McpCommand::Reconnect(name) => execute_mcp_lifecycle(session, &name, true),
        McpCommand::Disconnect(name) => execute_mcp_lifecycle(session, &name, false),
    }
}

/// Reconnects or disconnects one server, on the session's own runtime.
///
/// Blocking is deliberate and safe here: the event loop is a `spawn_blocking`
/// worker, not a runtime thread, and this is the same handle `/agent` already
/// blocks on to swap a session runtime. A reconnect dials a server, so it can
/// take a moment — a user who typed it is waiting for exactly that.
fn execute_mcp_lifecycle(session: &EngineSession, name: &str, reconnect: bool) -> String {
    let Some(client) = session
        .engine_half
        .mcp
        .as_ref()
        .map(|mcp| mcp.client.clone())
    else {
        return "!no MCP runtime is attached to this session".to_string();
    };
    let Some(handle) = session.runtime_handle() else {
        return "!this session has no runtime to reconnect on".to_string();
    };

    if reconnect {
        match handle.block_on(client.reconnect_server(name)) {
            Ok(true) => format!("MCP server `{name}` reconnected."),
            Ok(false) => format!("!no MCP server named `{name}` is connected"),
            // The old connection is already gone by now — say so, because the
            // server is *not* simply as it was.
            Err(error) => {
                format!("!reconnecting `{name}` failed and it is now disconnected: {error}")
            }
        }
    } else if handle.block_on(client.disconnect_server(name)) {
        format!("MCP server `{name}` disconnected. `/mcp reconnect {name}` brings it back.")
    } else {
        format!("!no MCP server named `{name}` is connected")
    }
}

fn execute_mcp_status(session: &EngineSession) -> String {
    let snapshot = crate::mcp_status::owner_mcp_snapshot(session)
        .unwrap_or_else(crate::mcp_status::not_hosted_here);
    format_mcp_status(snapshot)
}

/// The `/mcp` text for a snapshot — the host's own, or the one a mirrored
/// session's owner sent.
pub fn format_mcp_status(snapshot: rebon_session_host::McpStatusSnapshot) -> String {
    rebon_slash_commands::formatters::format_mcp_command(
        rebon_slash_commands::formatters::McpCommandDto {
            loader: snapshot.loader,
            warnings: snapshot.warnings,
            error: snapshot.error,
            client: snapshot.client,
            tools: snapshot
                .tools
                .into_iter()
                .map(|tool| rebon_slash_commands::formatters::McpToolDto {
                    name: tool.name,
                    tokens: fmt_tokens(tool.tokens.min(u64::from(u32::MAX)) as u32),
                })
                .collect(),
        },
    )
}

pub(crate) fn mcp_config_transport_label(
    config: &crate::mcp_config::McpServerConfig,
) -> &'static str {
    match config {
        crate::mcp_config::McpServerConfig::Stdio(_) => "stdio",
        crate::mcp_config::McpServerConfig::Http(_) => "http",
        crate::mcp_config::McpServerConfig::Sse(_) => "sse",
    }
}
