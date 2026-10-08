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
    /// Also offers sessions_list / session_read, which read — never change —
    /// the conversations other agents have had in the project: Rebon's
    /// sessions, and Claude Code's (under CLAUDE_CONFIG_DIR, else ~/.claude).
    ///
    /// Configure it in a client as the command `rebon mcp serve`, started in
    /// the project directory; jobs may run, and sessions be read, only in that
    /// directory or below it.
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
                    claude_config_dir: crate::sessions::claude_config_dir_from_env(),
                    groups_root: rebon_group::default_root(
                        &rebon_session::default_config_home_dir(),
                    ),
                    caller: rebon_group::identity::detect(),
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

/// `rebon group …`: agent groups (RFC-0009) from outside a session.
#[derive(Debug, clap::Subcommand, PartialEq, Eq)]
pub enum GroupCommand {
    /// Print what this session's agent group wrote to it, as hook context.
    ///
    /// Run by Claude Code or Codex as a command hook on UserPromptSubmit,
    /// SessionStart and PostToolUse, with the hook's JSON on stdin. It prints
    /// `{"hookSpecificOutput": {"additionalContext": …}}` when there is
    /// something new, and nothing otherwise; it never fails the hook.
    Hook(HookArgs),
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct HookArgs {
    /// The agent running the hook: claude-code or codex.
    #[arg(long)]
    pub agent: String,
}

/// `rebon permission …`: an agent CLI's permission requests, asked in the
/// Rebon app that runs it.
#[derive(Debug, clap::Subcommand, PartialEq, Eq)]
pub enum PermissionCommand {
    /// Hand a permission request to the Rebon app and print its answer.
    ///
    /// Run by Claude Code or Codex as their PermissionRequest hook, which the
    /// app adds when it starts one, with the request as JSON on stdin. It
    /// waits for the user's answer in the app and prints allow or deny; it
    /// prints nothing — and the CLI asks on its own screen — when the app is
    /// not there to ask, the user turns to the terminal, or anything fails.
    Hook(HookArgs),
}

/// Run a `rebon permission` subcommand.
pub fn run_permission(command: PermissionCommand) -> anyhow::Result<()> {
    match command {
        PermissionCommand::Hook(args) => {
            use std::io::Read;
            let mut input = String::new();
            if std::io::stdin().read_to_string(&mut input).is_err() {
                return Ok(());
            }
            let output = rebon_permission_relay::run_hook(
                &args.agent,
                &input,
                |name| std::env::var(name).ok(),
                // Unknown counts as there: the wait has a deadline anyway.
                |pid| rebon_session_host::process_is_running(pid) != Some(false),
            );
            if let Some(output) = output {
                println!("{output}");
            }
            Ok(())
        }
    }
}

/// Run a `rebon group` subcommand.
pub fn run_group(command: GroupCommand) -> anyhow::Result<()> {
    match command {
        GroupCommand::Hook(args) => {
            use std::io::Read;
            // A hook that errors shows up in the agent's session; one that
            // has nothing to say must say nothing, whatever went wrong.
            let mut input = String::new();
            if std::io::stdin().read_to_string(&mut input).is_err() {
                return Ok(());
            }
            let Ok(input) = serde_json::from_str::<serde_json::Value>(&input) else {
                return Ok(());
            };
            let store = rebon_group::GroupStore::new(rebon_group::default_root(
                &rebon_session::default_config_home_dir(),
            ));
            if let Some(output) = rebon_group::deliver::hook_output(&store, &args.agent, &input) {
                println!("{output}");
            }
            Ok(())
        }
    }
}
