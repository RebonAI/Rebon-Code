//! `rebon-lsp-mcp` — the LSP-to-MCP bridge as its own process.
//!
//! Started by whatever configures it as an MCP server: a host MCP config that
//! names this executable. It is a separate executable rather than a subcommand
//! because nothing about it needs a kernel or a session — it is spawned before
//! either exists.
//!
//! **stdout is the MCP channel.** Nothing but protocol frames may be written
//! to it, which is why tracing is pinned to stderr below.

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "rebon-lsp-mcp",
    version,
    about = "Rebon's LSP-to-MCP bridge. Configured as an MCP server, not run by hand."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum Command {
    /// Bridge rust-analyzer for the current workspace.
    Rust {
        /// Run as the per-workspace shared rust-analyzer daemon instead
        /// of a stdio bridge. Spawned by bridges; exits if another
        /// daemon already owns the workspace.
        #[arg(long, hide = true)]
        daemon: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Rust { daemon } => {
            let options = rebon_plugin_lsp_mcp::RustServerOptions::for_current_dir()?;
            if daemon {
                rebon_plugin_lsp_mcp::run_rust_daemon(
                    options,
                    rebon_plugin_lsp_mcp::default_daemon_idle_ttl(),
                )
                .await
            } else {
                rebon_plugin_lsp_mcp::run_rust_server(options).await
            }
        }
    }
}

/// Logs go to stderr. stdout carries the MCP frames, and one stray line on it
/// breaks the protocol for the client that spawned this process.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
}
