//! `rebon-browser-mcp` — the Chrome/Edge bridge as its own process.
//!
//! A separate executable rather than a subcommand, because it is spawned as an
//! MCP server before a kernel or a session exists. The browser extension does
//! not spawn anything; it connects to the WebSocket this process listens on.
//!
//! **stdout is the MCP channel.** Nothing but protocol frames may be written
//! to it, which is why tracing is pinned to stderr below.

use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "rebon-browser-mcp",
    version,
    about = "Rebon's Chrome/Edge bridge. Spawned as an MCP server, not run by hand."
)]
struct Cli {
    /// The installed plugin's `extension` directory, whose manifest and
    /// pairing state this server reads.
    #[arg(long = "extension-dir", value_name = "DIR")]
    extension_dir: PathBuf,
    /// Port the extension connects back on.
    #[arg(long, default_value_t = rebon_plugin_browser::DEFAULT_BROWSER_PORT)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let mut options = rebon_plugin_browser::BrowserServerOptions::new(
        rebon_session::config_home::default_config_home_dir(),
        cli.extension_dir,
    );
    options.port = cli.port;
    rebon_plugin_browser::run_server(options).await
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
