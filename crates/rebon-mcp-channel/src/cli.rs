//! `rebon mcp …`: the clap half. The binary parses it and hands over what
//! only the binary knows — its store, its executable, its launch policy.

use std::path::PathBuf;

use anyhow::Context;
use rebon_session_host::BackgroundStore;

use crate::jobs::LaunchGate;
use crate::ledger::LedgerOwner;
use crate::server::{serve, ServeConfig};
use crate::watch::Cadence;

#[derive(Debug, clap::Subcommand, PartialEq, Eq)]
pub enum McpCommand {
    /// Serve Rebon's background jobs to an MCP client over stdio.
    ///
    /// Offers exec_start / job_status / job_result / job_cancel / job_reply /
    /// job_permit, and pushes a job's outcome back to the client over the MCP
    /// channel extension (`notifications/claude/channel`) when it finishes or
    /// stops to wait for an answer. The jobs belong to Rebon's background
    /// supervisor, not to this process: closing the client leaves them running.
    ///
    /// Configure it in a client as the command `rebon mcp serve`, started in
    /// the project directory; jobs may run only in that directory or below it.
    Serve(ServeArgs),
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct ServeArgs {
    /// Do not declare the channel capability or push anything; the client
    /// checks `job_status` instead. For clients that cannot take pushes.
    #[arg(long = "no-channel")]
    pub no_channel: bool,
    /// Also offer `channel_probe`, a tool that pushes one message at once —
    /// the quickest way to find out whether pushes reach a session at all.
    #[arg(long)]
    pub probe: bool,
}

/// Run a `rebon mcp` subcommand.
///
/// `store` and `rebon_exe` are the binary's own (its config home, its
/// executable for starting the supervisor). `launch_gate` is checked before
/// every job is launched; stdout is the MCP connection, so nothing else may
/// write to it.
pub async fn run(
    command: McpCommand,
    store: BackgroundStore,
    rebon_exe: PathBuf,
    launch_gate: LaunchGate,
) -> anyhow::Result<()> {
    match command {
        McpCommand::Serve(args) => {
            let root = std::env::current_dir()
                .context("rebon mcp serve needs a working directory to confine jobs to")?;
            // The connection gets its own ends of stdin and stdout before
            // `exec_start` can launch anything, so no process this server
            // starts can inherit, block on or write into them. See
            // `rebon_proto::process_stdio`.
            let stdio = rebon_proto::process_stdio::take_process_stdio()
                .context("failed to take stdio for the MCP connection")?;
            serve(
                stdio.input,
                stdio.output,
                ServeConfig {
                    store,
                    projects_root: rebon_session::default_projects_root(),
                    root,
                    rebon_exe,
                    launch_gate,
                    channel: !args.no_channel,
                    probe: args.probe,
                    owner: LedgerOwner::this_process(),
                    cadence: Cadence::default(),
                },
            )
            .await
        }
    }
}
