//! `rebon` binary entrypoint.
//!
//! Thin clap dispatcher: inspect the parsed
//! flags, hand control to the matching run-mode module, and let the
//! library crates do all the actual work.
//!
//! Run modes:
//!
//! * `rebon --acp` → [`acp::run_acp_server`] — the ACP (Agent Client
//!   Protocol) JSON-RPC-over-stdio server, used when an editor or IDE
//!   client wants to drive rebon as an agent.
//! * `rebon` (default) → local interactive TUI via [`tui::run`].

mod acp;
mod background;
mod exec;
use background::RuntimeFieldsExt;
mod file_scanner;
mod goal;
mod kernel_cmd;
mod node_cmd;
mod oauth_drive;
// The session runtime is a crate of its own, so these aliases keep every
// `crate::session::…`, `crate::plugin::…`, `crate::ui_config::…` call site
// resolving as it always has — 380 of them, 44 files of which are under
// `tui/` — instead of spelling the dependency out at each one.
pub(crate) use rebon_session_runtime as session;
#[cfg(test)]
pub(crate) use rebon_session_runtime::test_env;
pub(crate) use rebon_session_runtime::{
    acp_subagent_pool, mcp_config, mcp_status, plugin, project_settings, rebon_config, ripgrep,
    task_notification_poller, ui_config,
};
mod serve;
mod session_agent_router;
#[cfg(test)]
mod session_command_tests;
mod session_shell;
mod sibling_command;
mod steering_poller;
mod tui;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use chrono::{Duration as ChronoDuration, Local};
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

use crate::ui_config::UiMode;
use rebon_permissions::PermissionMode;
use rebon_plugin_mcp::runtime::{parse_channel_entries, ChannelEntry};
use rebon_types::AgentCapabilityMode;
use rebon_types::ReasoningEffort;

#[derive(Debug, Parser)]
#[command(name = "rebon", version, about)]
struct Cli {
    /// Run as an ACP (Agent Client Protocol) server on stdio.
    ///
    /// Binds to real stdin/stdout, spawns an engine with every
    /// registered builtin tool, and drives the JSON-RPC dispatch
    /// loop until the client disconnects. `session/prompt` calls
    /// drive the full agentic loop (tool use + permission
    /// reverse-RPC + transcript persistence).
    #[arg(long)]
    acp: bool,

    /// Listen for ACP on a raw TCP port instead of stdio.
    ///
    /// Requires `--acp`. Passing `0` lets the OS allocate an available
    /// port; the actual bound address is reported on stderr so stdout
    /// remains reserved for ACP framing.
    #[arg(long = "acp-port", value_name = "PORT", requires = "acp")]
    acp_port: Option<u16>,

    /// Host/interface for `--acp-port` raw TCP ACP listening.
    ///
    /// Requires `--acp`; defaults to 127.0.0.1 when omitted.
    #[arg(long = "acp-host", value_name = "HOST", requires = "acp")]
    acp_host: Option<String>,

    /// Override the active custom provider from rebon config.
    ///
    /// When set, rebon looks up this name in
    /// `~/.rebon/config.json::customProviders[]` instead of
    /// using the stored `activeCustomProvider`. Lets you test a
    /// specific provider without running `rebon` to flip the
    /// active one on disk.
    ///
    /// Example: `rebon --provider openrouter`
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Override the default model for the resolved provider.
    ///
    /// When set, rebon passes this string to the engine as the
    /// default model instead of the provider's `model` field.
    /// Useful for sanity-checking the API surface against a
    /// specific model without touching the config file.
    ///
    /// Example: `rebon --model gpt-5.5`
    #[arg(long, global = true)]
    model: Option<String>,

    /// Resume a previous session by its session id.
    ///
    /// When set, rebon loads the on-disk transcript for the given
    /// session id and replays it into the TUI so you can continue
    /// where you left off. The session id is printed on exit.
    ///
    /// Example: `rebon --resume k7m2q-4xr9t-hb3wz-p8ncv`
    #[arg(long, conflicts_with = "continue_session")]
    resume: Option<String>,

    /// Continue the most recent stopped session in the current working directory.
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,

    /// Run this session in a background worker, instead of in this
    /// process. Opt-in: hosting was the default in 0.21–0.23 and is not
    /// one again until the mirror is finished (see below).
    ///
    /// The engine that owns the conversation — and every agent it
    /// spawns — runs in a process that outlives this terminal, so
    /// closing the terminal detaches instead of killing the work. The
    /// terminal is the session's mirror; `rebon --resume <id>` and
    /// `rebon attach <job>` mirror it again.
    ///
    /// What the mirror still gets wrong, and why it is not the default:
    /// a mid-turn round trip to the user does not reach the user
    /// (`AskUserQuestion` answered by an Esc, an appended message
    /// consumed a turn late, `EnterPlanMode` unshown and `ExitPlanMode`
    /// unconfirmed), and the terminal's incremental commit stalls (tool
    /// results invisible, rows never reaching scrollback).
    #[arg(long, conflicts_with = "local")]
    hosted: bool,

    /// Host this session in this process instead of a background worker.
    /// This is the default; the flag is kept so scripts written against
    /// it keep working, and so `--local` still says out loud which half
    /// of the choice it wants.
    ///
    /// The engine runs here, holds the session's lock, publishes no
    /// endpoint, and cannot be reached from another terminal or the
    /// desktop app — closing this terminal ends the session. `/hosted`
    /// still moves the session to a worker later.
    #[arg(long)]
    local: bool,

    /// Run this session on a configured remote host instead of locally.
    ///
    /// The agent — its shell, its filesystem, its git checkout — runs
    /// on the far end over ssh; this machine keeps the UI and the
    /// transcript. Configure hosts with `rebon remote add`.
    ///
    /// Example: `rebon --remote prod`
    #[arg(long, value_name = "NAME")]
    remote: Option<String>,

    /// Project directory on the remote, overriding the host's default.
    ///
    /// A path on the *remote* filesystem. Requires `--remote`.
    #[arg(long = "remote-path", value_name = "PATH", requires = "remote")]
    remote_path: Option<String>,

    /// Start a prompt in a persistent background worker and exit.
    #[arg(long = "bg", value_name = "PROMPT")]
    bg: Option<String>,

    /// Display name for a background job started with `--bg`.
    #[arg(long = "name", value_name = "NAME", requires = "bg")]
    name: Option<String>,

    /// Run a background prompt as the named custom/built-in agent.
    #[arg(long = "agent", value_name = "AGENT", requires = "bg")]
    agent: Option<String>,

    /// Working directory for the interactive session or `--bg` job.
    #[arg(long = "cwd", value_name = "DIR", global = true)]
    cwd: Option<PathBuf>,

    /// Select the interactive UI surface.
    ///
    /// `screen` keeps the existing full-screen alternate-screen TUI.
    /// `inline` uses shell scrollback with a bounded live viewport.
    #[arg(long = "ui-mode", value_parser = ui_mode_value_parser(), global = true)]
    ui_mode: Option<UiMode>,

    /// Startup effort/thinking level for this session or background job.
    #[arg(long = "effort", value_name = "LEVEL", value_parser = parse_effort_level, global = true)]
    effort: Option<ReasoningEffort>,

    /// Startup permission mode for this session or background job.
    #[arg(long = "permission-mode", alias = "mode", value_name = "MODE", value_parser = parse_permission_mode, global = true)]
    permission_mode: Option<PermissionMode>,

    /// Load settings override JSON or a settings file for this session.
    #[arg(long = "settings", value_name = "FILE_OR_JSON", global = true)]
    settings: Vec<String>,

    /// Grant file access to an additional directory for this session.
    #[arg(long = "add-dir", value_name = "DIR", global = true)]
    add_dirs: Vec<PathBuf>,

    /// Load a local plugin directory for this session.
    #[arg(long = "plugin-dir", value_name = "DIR", global = true)]
    plugin_dirs: Vec<PathBuf>,

    /// Load MCP servers from a config file or JSON string.
    #[arg(long = "mcp-config", value_name = "FILE_OR_JSON", global = true)]
    mcp_configs: Vec<String>,

    /// Enable an explicit language-server MCP bridge for this session.
    #[arg(long = "lsp", value_name = "LANG", value_enum, global = true)]
    lsp: Vec<LspKind>,

    /// Use only MCP servers supplied by --mcp-config.
    #[arg(long = "strict-mcp-config", global = true)]
    strict_mcp_config: bool,

    /// Enable approved MCP channel servers for this interactive session.
    ///
    /// Accepted values are `plugin:<name>@<marketplace>` and
    /// `server:<name>`. Entries still pass the runtime channel gate;
    /// this flag is only the per-session opt-in.
    #[arg(long = "channels", value_name = "CHANNEL", num_args = 1..)]
    channels: Vec<String>,

    /// Load development MCP channels after an interactive warning.
    ///
    /// Uses the same entry syntax as `--channels`, but accepted entries
    /// are marked `dev: true` individually after the TUI warning dialog.
    #[arg(long = "dangerously-load-development-channels", value_name = "CHANNEL", num_args = 1..)]
    development_channels: Vec<String>,

    /// Headless status-only update commands.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LspKind {
    Rust,
}

/// The binaries that ship beside `rebon` and now own three former subcommands.
///
/// Named here because the cli is what writes them down: two of the three are
/// spawned by materializers below, and all three are what the deprecated
/// shells in [`sibling_command`] hand over to. The binaries themselves are
/// declared by the plugins that own them.
const BROWSER_MCP_SIBLING: &str = "rebon-browser-mcp";

const COMPUTER_USE_SIBLING: &str = "rebon-computer-use";

// Both constants followed their writer into `rebon-session-runtime`
// (`plugin::builtin` materializes the config that names them). Re-exported so
// this crate's own `LSP_MCP_SIBLING` / `RUST_LSP_SERVER_NAME` keep resolving.
pub(crate) use rebon_session_runtime::{LSP_MCP_SIBLING, RUST_LSP_SERVER_NAME};

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum Command {
    /// Open the Agent View for background jobs and in-process agents.
    Agents {
        #[command(subcommand)]
        command: Option<AgentsCommand>,
    },
    /// Attach to a background job's persisted conversation.
    Attach {
        #[arg(value_name = "ID")]
        job_id: String,
    },
    /// Print recent output from a background job.
    Logs {
        #[arg(value_name = "ID")]
        job_id: String,
        #[arg(long = "lines", default_value_t = 80)]
        lines: usize,
    },
    /// Stop a background job.
    Stop {
        #[arg(value_name = "ID")]
        job_id: String,
    },
    /// Stop a background job.
    Kill {
        #[arg(value_name = "ID")]
        job_id: String,
    },
    /// Queue a fresh run using an existing job's prompt and runtime metadata.
    Respawn {
        #[arg(
            value_name = "ID",
            required_unless_present = "all",
            conflicts_with = "all"
        )]
        job_id: Option<String>,
        #[arg(long = "all")]
        all: bool,
    },
    /// Remove stopped or completed background job metadata.
    Rm {
        #[arg(value_name = "ID")]
        job_id: String,
    },
    /// Send a reply to a background job, using live IPC when the worker is running.
    Reply {
        #[arg(value_name = "ID")]
        job_id: String,
        #[arg(value_name = "MESSAGE", num_args = 1.., trailing_var_arg = true)]
        message: Vec<String>,
    },
    /// Answer the latest permission prompt for a background job by option number.
    Permit {
        #[arg(value_name = "ID")]
        job_id: String,
        #[arg(value_name = "OPTION")]
        option: usize,
    },
    /// Manage the per-user background supervisor service.
    Bg {
        #[command(subcommand)]
        command: AgentServiceCommand,
    },
    /// Hidden supervisor entrypoint for background jobs.
    #[command(name = "__background-supervisor", hide = true)]
    BackgroundSupervisor,
    /// Hidden worker entrypoint for `--bg`.
    #[command(name = "__background-worker", hide = true)]
    BackgroundWorker {
        #[arg(long = "job-id")]
        job_id: String,
    },
    /// Hidden stdio MCP bridge an ACP agent spawns to reach Rebon's
    /// injected write_file/edit_file tools. Configured via env, not
    /// arguments — see `rebon_acp_client::fs_mcp`.
    #[command(name = "__acp-fs-mcp", hide = true)]
    AcpFsMcp,
    /// Deprecated shell for `rebon-browser-mcp`. Removed in 0.26.
    #[command(name = "browser-mcp", hide = true)]
    BrowserMcp {
        /// Passed to `rebon-browser-mcp` unchanged.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0.., value_name = "ARGS")]
        args: Vec<OsString>,
    },
    /// Serve Rebon's background jobs to an MCP client (Claude Code and
    /// others): start a job, read its result, answer it, stop it — with the
    /// outcome pushed back over the MCP channel extension.
    Mcp {
        #[command(subcommand)]
        command: rebon_mcp_channel::cli::McpCommand,
    },
    /// Remote Control: serve this machine's Rebon sessions to an RC server.
    ///
    /// `rebon rc login` binds the machine, `rebon rc serve` registers it and
    /// runs the sessions a controller asks for, `rebon rc status` shows what
    /// is registered. What a served session does is visible to the server.
    Rc {
        #[command(subcommand)]
        command: rebon_rc_runner::cli::RcCommand,
    },
    /// Manage local Rebon plugins.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Manage remote hosts Rebon can run a session on over ssh.
    Remote {
        #[command(subcommand)]
        command: rebon_plugin_remote::cli::RemoteCommand,
    },
    /// Sign in with a subscription account (ChatGPT, GitHub Copilot).
    ///
    /// `rebon login` asks which account, `rebon login copilot` names one,
    /// and `rebon login --status` shows which are signed in. Works before
    /// any provider is configured, which is when it is needed.
    Login(rebon_plugin_onboarding::cli::LoginArgs),
    /// Sign out of a subscription account. The provider entry it created
    /// stays, so signing in again restores it.
    Logout(rebon_plugin_onboarding::cli::LogoutArgs),
    /// Deprecated shell for `rebon-computer-use`. Removed in 0.26.
    ///
    /// The runtime drives a desktop window the user picks; the `ComputerUse`
    /// tool connects to whichever one is published. It now ships as its own
    /// executable beside `rebon` — run `rebon-computer-use --help`.
    #[command(name = "computer-use")]
    ComputerUse {
        /// Passed to `rebon-computer-use` unchanged.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0.., value_name = "ARGS")]
        args: Vec<OsString>,
    },
    /// Manage the embedded kernel's loop vendors.
    Kernel {
        #[command(subcommand)]
        command: KernelCommand,
    },
    /// Inspect or install the Node runtime the plugin plane runs on.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// Deprecated shell for `rebon-lsp-mcp`. Removed in 0.26.
    #[command(name = "lsp-mcp", hide = true)]
    LspMcp {
        /// Passed to `rebon-lsp-mcp` unchanged.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0.., value_name = "ARGS")]
        args: Vec<OsString>,
    },
    /// Status-only update information. Does not check the network or install anything.
    Update {
        #[command(subcommand)]
        command: rebon_plugin_updater::cli::UpdateCliCommand,
    },
    /// Run a single prompt headlessly and stream the turn as JSONL events.
    ///
    /// Non-interactive one-shot: builds an in-process engine session, runs the
    /// agentic loop to completion, and prints one JSON object per line to
    /// stdout (tool calls, results, assistant messages, usage). Tracing goes to
    /// stderr so stdout stays a clean event stream. Auto-approves tool
    /// permissions (unattended). Designed for eval harnesses like NiceEval.
    ///
    /// Example: `rebon exec --json --model gpt-5.4 "what is the weather in Beijing?"`
    Exec {
        /// The prompt text to send (all trailing words are joined).
        #[arg(value_name = "PROMPT", num_args = 1.., trailing_var_arg = true)]
        prompt: Vec<String>,
        /// Emit machine-readable JSONL events on stdout (one object per line).
        /// Without it, a compact human-readable trace is printed instead.
        #[arg(long = "json", default_value_t = false)]
        json: bool,
        /// Resume a previous (stopped) session by id to continue the
        /// conversation. The session id is printed on the first output line.
        #[arg(long = "resume", value_name = "ID")]
        resume: Option<String>,
        /// Stop the agentic loop after this many model iterations.
        #[arg(long = "max-iterations", value_name = "N")]
        max_iterations: Option<std::num::NonZeroUsize>,
        /// Agent capability mode: `normal` (default) or `minimal`. Mirrors the
        /// desktop app's new-chat picker; on a provider that opts into Anchored
        /// Minimal, `minimal` also selects the anchored bootstrap request.
        #[arg(long = "capability", value_name = "MODE", value_parser = parse_capability_mode, default_value = "normal")]
        capability: AgentCapabilityMode,
        /// Re-open the turn this many times after the model says it is done,
        /// asking it to verify its own work first. `0` (default) accepts the
        /// first "done".
        ///
        /// For unattended runs. A model with hours of budget routinely stops
        /// after minutes and hands in work it has not checked against anything;
        /// each round resumes the same session, so nothing is re-read, and a
        /// round that edits no file and runs no command ends the gate at once.
        #[arg(long = "verify-rounds", value_name = "N", default_value_t = 0)]
        verify_rounds: u32,
        /// Stop opening new `--verify-rounds` once the turn has run this long,
        /// in seconds. Unset means only the round count bounds it.
        #[arg(long = "verify-budget", value_name = "SECONDS")]
        verify_budget: Option<u64>,
        /// Cancel the run once it has taken this long, in seconds.
        ///
        /// A hard ceiling, unlike `--verify-budget`: that one declines to open
        /// another audit round, this one stops the work in progress. The turn
        /// is cancelled the way Ctrl-C would cancel it — in-flight tools are
        /// interrupted and the transcript is written, so `--resume` still
        /// works — and the final result line reports `max_duration`.
        ///
        /// Unset means the run is bounded only by `--max-iterations`, which is
        /// a count and not a duration: an unattended turn can and does spend
        /// hours inside 600 iterations.
        ///
        /// Bounds the agent loop, not a command already running: a tool in
        /// flight is interrupted, but its process runs on to its own timeout
        /// (at most 600 s), so the real ceiling is this plus one tool timeout.
        #[arg(long = "max-duration", value_name = "SECONDS")]
        max_duration: Option<u64>,
    },
    /// Serve Rebon to a browser on this machine.
    ///
    /// Runs the same ACP server `--acp` gives an editor, behind a local web
    /// page: sessions, streaming turns, tool calls, permission prompts, and
    /// the session agent (`local`, a kernel loop such as `kernel:dsh`, or a
    /// configured agent CLI) are all driven from the page. `kernelPlugins`
    /// boots exactly as it does for the TUI, so composed dsh plugins are
    /// available to every session served here.
    ///
    /// Binds loopback by default and prints a URL carrying a per-run token;
    /// the page and its WebSocket need that token, so another page open in
    /// the same browser cannot drive the agent.
    Serve {
        /// Address to listen on. Anything but loopback exposes the agent to
        /// that network, guarded by the token alone.
        #[arg(long, default_value = serve::DEFAULT_HOST, value_name = "HOST")]
        host: String,
        /// Port to listen on. `0` picks a free one.
        #[arg(long, default_value_t = serve::DEFAULT_PORT, value_name = "PORT")]
        port: u16,
        /// Require this token instead of generating one.
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// Open the page in the default browser once listening.
        #[arg(long)]
        open: bool,
        /// Serve the page from this directory instead of the build compiled
        /// into the binary (a `assets/web-ui/dist` you just built, or a
        /// customised page). Also read from `REBON_WEB_UI_DIR`.
        #[arg(long = "web-ui", value_name = "DIR")]
        web_ui: Option<std::path::PathBuf>,
        /// Also accept ACP connections on a local IPC endpoint — a Unix
        /// socket, or a named pipe on Windows — for native clients that do
        /// not need the web page. Defaults to a per-port endpoint under the
        /// Rebon config home; pass a path to choose one. Clients present the
        /// same token, as their first line.
        #[arg(long = "ipc", value_name = "PATH", num_args = 0..=1)]
        ipc: Option<Option<std::path::PathBuf>>,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum PluginCommand {
    /// Install a local plugin path, a package archive, or a built-in alias.
    Install {
        #[arg(value_name = "PATH_OR_ARCHIVE_OR_NAME")]
        source: String,
        #[arg(long = "scope", value_parser = parse_plugin_scope, default_value = "user")]
        scope: plugin::PluginScope,
        /// Expected SHA-256 of a package archive. Checked before the archive is
        /// opened; a mismatch refuses the install.
        #[arg(long, value_name = "HEX")]
        sha256: Option<String>,
    },
    /// Re-hash installed packages and report any that changed on disk since
    /// they were installed.
    Verify {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(long = "scope", value_parser = parse_plugin_scope)]
        scope: Option<plugin::PluginScope>,
    },
    /// List installed plugins.
    List {
        #[arg(long = "scope", value_parser = parse_plugin_scope)]
        scope: Option<plugin::PluginScope>,
    },
    /// Show installed plugin status.
    Status {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(long = "scope", value_parser = parse_plugin_scope)]
        scope: Option<plugin::PluginScope>,
    },
    /// Enable an installed plugin.
    Enable {
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long = "scope", value_parser = parse_plugin_scope, default_value = "user")]
        scope: plugin::PluginScope,
    },
    /// Disable an installed plugin.
    Disable {
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long = "scope", value_parser = parse_plugin_scope, default_value = "user")]
        scope: plugin::PluginScope,
    },
    /// Uninstall a plugin from the selected scope.
    Uninstall {
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long = "scope", value_parser = parse_plugin_scope, default_value = "user")]
        scope: plugin::PluginScope,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum KernelCommand {
    /// Show loop vendors and where they run.
    Status,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum NodeCommand {
    /// Show which Node runtime the plugin plane would use, what is installed
    /// under the config home, and whether downloading is allowed.
    Status,
    /// Install a Node runtime under the config home.
    ///
    /// With no flags this downloads the build whose SHA-256 is compiled into
    /// this binary. `--from-path` installs from a local archive instead, which
    /// is also the offline and air-gapped path.
    Install {
        /// Install from a local `.tar.gz` / `.zip` instead of downloading.
        #[arg(long, value_name = "ARCHIVE")]
        from_path: Option<std::path::PathBuf>,
        /// SHA-256 of a local archive Rebon does not pin. Requires
        /// `--version` and `--from-path`.
        #[arg(long, value_name = "HEX")]
        sha256: Option<String>,
        /// Node version a local archive contains. Requires `--sha256`.
        #[arg(long, value_name = "X.Y.Z")]
        version: Option<String>,
        /// Mirror serving nodejs.org's `dist` layout. The pinned digest still
        /// decides whether the bytes are accepted.
        #[arg(long, value_name = "URL")]
        dist_base: Option<String>,
        /// Reinstall even when this version is already installed.
        #[arg(long)]
        force: bool,
    },
    /// Remove a managed Node runtime.
    Uninstall {
        #[arg(long, value_name = "X.Y.Z")]
        version: Option<String>,
        /// Remove every managed runtime.
        #[arg(long)]
        all: bool,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum AgentsCommand {
    /// Manage the per-user background supervisor service.
    Service {
        #[command(subcommand)]
        command: AgentServiceCommand,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum AgentServiceCommand {
    /// Print local supervisor service registration state.
    Status,
    /// Register a per-user scheduler for the background supervisor.
    Install,
    /// Remove the per-user background supervisor scheduler registration.
    Uninstall,
}

/// In ACP mode logs go to stderr (no TUI to interfere with).
/// In TUI mode logs go to a file so they never corrupt the
/// ratatui alternate-screen surface — stderr is not covered by
/// crossterm's `EnterAlternateScreen`.
fn init_tracing(tui_mode: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // The `ignore` + `globset` crates (used by rebon-tool's glob/grep) emit
    // a handful of DEBUG lines on every invocation: "opened gitignore file
    // ...", "built glob set; ...". Under `RUST_LOG=debug` this completely
    // drowns rebon's own diagnostics. Pin those targets to WARN unless the
    // user explicitly asked for their logs via `RUST_LOG=...,globset=...`
    // or `...,ignore=...`.
    let user_env = std::env::var("RUST_LOG").unwrap_or_default();
    let filter = if user_env.contains("globset=") {
        filter
    } else {
        filter.add_directive("globset=warn".parse().expect("static directive"))
    };
    let filter = if user_env.contains("ignore=") {
        filter
    } else {
        filter.add_directive("ignore=warn".parse().expect("static directive"))
    };
    // The `stream_dbg` target prints per-frame INFO lines from
    // `render_streaming_overlay` and friends — useful when chasing a
    // streaming-render bug, ruinous as default. A 14h session
    // accumulates ~1.5M log lines / ~200MB on disk from this target
    // alone, drowning every other diagnostic and bloating the file.
    // Clamp to WARN unless the user explicitly opts back in via
    // `RUST_LOG=...,stream_dbg=info` (or =debug/=trace).
    let filter = if user_env.contains("stream_dbg=") {
        filter
    } else {
        filter.add_directive("stream_dbg=warn".parse().expect("static directive"))
    };
    if tui_mode {
        let log_dir = rebon_log_dir();
        let (file, log_path, rotation_warning) =
            prepare_tui_log_file(&log_dir).expect("failed to open rebon log file");
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_writer(Mutex::new(file))
            .with_ansi(false)
            .try_init()
            .ok();
        tracing::info!(
            log_path = %log_path.display(),
            retained_history = 5,
            "rebon startup: log file ready"
        );
        if let Some(warning) = rotation_warning {
            tracing::warn!(warning = %warning, "rebon startup: log rotation degraded");
        }
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_writer(std::io::stderr)
            .try_init()
            .ok();
    }
}

fn prepare_tui_log_file(log_dir: &Path) -> io::Result<(fs::File, PathBuf, Option<String>)> {
    fs::create_dir_all(log_dir)?;
    let log_path = log_dir.join("rebon.log");
    let mut warning = None;
    let append_existing = match rotate_current_tui_log(log_dir, &log_path) {
        Ok(()) => false,
        Err(err) => {
            warning = Some(format!("failed to rotate {}: {err}", log_path.display()));
            true
        }
    };
    if let Err(err) = prune_old_tui_logs(log_dir) {
        warning = Some(match warning {
            Some(existing) => format!("{existing}; failed to prune old logs: {err}"),
            None => format!("failed to prune old logs: {err}"),
        });
    }

    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    if append_existing {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let file = options.open(&log_path)?;
    Ok((file, log_path, warning))
}

fn rotate_current_tui_log(log_dir: &Path, log_path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(log_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if !metadata.file_type().is_file() {
        return Ok(());
    }
    if metadata.len() == 0 {
        fs::remove_file(log_path)?;
        return Ok(());
    }
    fs::rename(log_path, next_history_log_path(log_dir)?)
}

fn next_history_log_path(log_dir: &Path) -> io::Result<PathBuf> {
    let now = Local::now();
    for offset in 0..3600 {
        let timestamp = (now + ChronoDuration::seconds(offset))
            .format("%Y-%m-%dT%H-%M-%S")
            .to_string();
        let path = log_dir.join(format!("rebon-{timestamp}.log"));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no available rebon log history filename",
    ))
}

fn prune_old_tui_logs(log_dir: &Path) -> io::Result<()> {
    let mut logs = Vec::new();
    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_rotated_rebon_log_name(&name) && entry.file_type()?.is_file() {
            logs.push((name.into_owned(), entry.path()));
        }
    }
    logs.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in logs.into_iter().skip(5) {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn is_rotated_rebon_log_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() != "rebon-YYYY-MM-DDTHH-mm-ss.log".len()
        || !name.starts_with("rebon-")
        || !name.ends_with(".log")
    {
        return false;
    }
    let timestamp = &bytes["rebon-".len()..bytes.len() - ".log".len()];
    timestamp.iter().enumerate().all(|(idx, byte)| match idx {
        4 | 7 | 13 | 16 => *byte == b'-',
        10 => *byte == b'T',
        _ => byte.is_ascii_digit(),
    })
}

fn rebon_log_dir() -> PathBuf {
    // Prefer $REBON_LOG_DIR if set, otherwise fall back to temp dir.
    if let Ok(dir) = std::env::var("REBON_LOG_DIR") {
        return PathBuf::from(dir);
    }
    std::env::temp_dir().join("rebon").join("logs")
}

/// `--ui-mode`'s parser, built from [`UiMode::ALL`] so `--help` lists exactly
/// the spellings the flag accepts.
///
/// `UiMode` lives in `rebon-config` with the other persisted settings, which
/// does not depend on clap; a `PossibleValuesParser` over the type's own list
/// gives the same help text and the same `InvalidValue` error a derived
/// `ValueEnum` did, without pushing an argument parser down a config crate.
fn ui_mode_value_parser() -> impl clap::builder::TypedValueParser<Value = UiMode> {
    use clap::builder::TypedValueParser as _;
    clap::builder::PossibleValuesParser::new(UiMode::ALL.map(UiMode::as_str)).map(|chosen| {
        chosen
            .parse::<UiMode>()
            .expect("the possible values are UiMode's own spellings")
    })
}

fn parse_effort_level(raw: &str) -> Result<ReasoningEffort, String> {
    let normalized = raw.trim().to_ascii_lowercase().replace('-', "_");
    // `--effort` has accepted these three spellings since before the level
    // had a single parser, and they are the flag's promise rather than the
    // type's: `ReasoningEffort::from_str` takes no aliases on purpose, so
    // they are folded to canonical here instead of widening it.
    let canonical = match normalized.as_str() {
        "med" => "medium",
        "x_high" | "extra_high" => "xhigh",
        other => other,
    };
    canonical.parse::<ReasoningEffort>().map_err(|_| {
        format!("invalid effort level `{normalized}`; expected low, medium, high, xhigh, or max")
    })
}

fn parse_permission_mode(raw: &str) -> Result<PermissionMode, String> {
    match raw.trim().to_ascii_lowercase().replace(['-', '_'], "").as_str() {
        "default" => Ok(PermissionMode::Default),
        "plan" | "planmode" => Ok(PermissionMode::Plan),
        "acceptedits" => Ok(PermissionMode::AcceptEdits),
        "bypasspermissions" | "bypass" => Ok(PermissionMode::BypassPermissions),
        "dontask" => Ok(PermissionMode::DontAsk),
        "auto" => Ok(PermissionMode::Auto),
        other => Err(format!(
            "invalid permission mode `{other}`; expected default, plan, acceptEdits, bypassPermissions, dontAsk, or auto"
        )),
    }
}

fn parse_capability_mode(raw: &str) -> Result<AgentCapabilityMode, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "normal" | "full" => Ok(AgentCapabilityMode::Normal),
        "minimal" | "min" => Ok(AgentCapabilityMode::Minimal),
        other => Err(format!(
            "invalid capability mode `{other}`; expected normal or minimal"
        )),
    }
}

fn parse_plugin_scope(raw: &str) -> Result<plugin::PluginScope, String> {
    plugin::PluginScope::parse(raw).map_err(|err| err.to_string())
}

fn string_paths(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect()
}

#[cfg(test)]
fn latest_stopped_session_id(projects_root: &Path, cwd: &str) -> Option<String> {
    let sessions = rebon_acp::ServerState::default()
        .list_sessions(projects_root, Some(cwd), Some(cwd))
        .ok()?;
    sessions
        .into_iter()
        .filter(|session| {
            !rebon_session::is_session_active(projects_root, &session.cwd, &session.id)
        })
        .max_by_key(|session| session.created_at)
        .map(|session| session.id)
}

/// `--channels` and `--dangerously-load-development-channels`, parsed. Every
/// route that takes them parses both, at the point it used to, so a bad entry
/// still fails the route before anything else it does.
fn cli_channel_entries(cli: &Cli) -> anyhow::Result<(Vec<ChannelEntry>, Vec<ChannelEntry>)> {
    let parse = |entries: &[String]| {
        parse_channel_entries(entries).map_err(|err| anyhow::anyhow!(err.to_string()))
    };
    Ok((parse(&cli.channels)?, parse(&cli.development_channels)?))
}

fn runtime_overrides_from_cli(
    cli: &Cli,
    channels: Vec<ChannelEntry>,
    development_channels: Vec<ChannelEntry>,
    executable: Option<&Path>,
) -> anyhow::Result<rebon_config::RuntimeOverride> {
    let mut mcp_configs = cli.mcp_configs.clone();
    mcp_configs.extend(lsp_mcp_configs_from_cli(&cli.lsp, executable)?);
    Ok(rebon_config::RuntimeOverride {
        provider: cli.provider.clone(),
        model: cli.model.clone(),
        fast_mode: None,
        resume: cli.resume.clone(),
        cwd: None,
        channels,
        development_channels,
        settings: cli.settings.clone(),
        add_dirs: string_paths(&cli.add_dirs),
        plugin_dirs: string_paths(&cli.plugin_dirs),
        mcp_configs,
        strict_mcp_config: cli.strict_mcp_config,
        ui_mode: cli.ui_mode,
        effort_level: cli.effort,
        permission_mode: cli.permission_mode,
        queue_session: false,
        startup_agent_view: false,
        startup_hosted: cli.hosted,
        // Hosting is opt-in again. `startup_local` is the one flag every
        // downstream host decision already reads (RFC-0004 §15.1: the
        // startup fast path, the deferred handoff, and the resume router
        // each ask it), so resolving the default here flips all three at
        // once and leaves the hosted paths reachable through `--hosted`
        // for the work that finishes the mirror.
        startup_local: cli.local || !cli.hosted,
        startup_agent_view_cwd_scope: None,
        startup_notices: Vec::new(),
        attached_background_job_id: None,
        remote: cli.remote.clone(),
        remote_path: cli.remote_path.clone(),
    })
}

/// `executable` is the running `rebon`; `None` asks the OS for it.
///
/// It is a parameter because a test binary lives in `target/debug/deps/`,
/// where the bridge is not — the product locations are relative to the
/// *shipped* executable, so a test has to name one.
fn lsp_mcp_configs_from_cli(
    lsp: &[LspKind],
    executable: Option<&Path>,
) -> anyhow::Result<Vec<String>> {
    if !lsp.iter().any(|kind| matches!(kind, LspKind::Rust)) {
        return Ok(Vec::new());
    }
    // The bridge is its own executable beside `rebon`; writing a config that
    // pointed at `rebon lsp-mcp rust` would go through the deprecated shell,
    // and that shell is gone in 0.26.
    let bridge = match executable {
        Some(executable) => {
            rebon_types::sibling_binary::resolve_from_executable(LSP_MCP_SIBLING, executable)
        }
        None => rebon_types::sibling_binary::resolve(LSP_MCP_SIBLING),
    }
    .context("failed to locate the LSP bridge for --lsp rust")?;
    let cwd = std::env::current_dir().context("failed to read current directory for --lsp rust")?;
    Ok(vec![serde_json::json!({
        "mcpServers": {
            RUST_LSP_SERVER_NAME: {
                "command": bridge.to_string_lossy(),
                "args": ["rust"],
                "cwd": cwd.to_string_lossy(),
            }
        }
    })
    .to_string()])
}

fn agent_view_enabled() -> anyhow::Result<()> {
    if rebon_config::agent_view_is_disabled() {
        anyhow::bail!("agent view is disabled by config or REBON_CODE_DISABLE_AGENT_VIEW");
    }
    Ok(())
}

/// What `rebon mcp serve` checks before it launches a job: the same two
/// things `--bg` checks — background jobs are enabled, and the job's
/// permission mode is one the user has accepted for unattended runs.
fn background_launch_gate(runtime: &background::BackgroundRuntimeFields) -> anyhow::Result<()> {
    agent_view_enabled()?;
    crate::session::host::runtime_permissions::ensure_background_runtime_permission_mode_allowed(
        runtime,
    )
}

fn ensure_background_permission_mode_allowed(mode: Option<PermissionMode>) -> anyhow::Result<()> {
    if let Some(mode) = mode {
        rebon_config::ensure_background_permission_mode_allowed(mode)?;
    }
    Ok(())
}

fn record_interactive_background_permission_mode_acceptance(mode: Option<PermissionMode>) {
    let Some(mode) = mode else {
        return;
    };
    if !rebon_config::background_permission_mode_requires_interactive_acceptance(mode) {
        return;
    }
    if let Err(err) = rebon_config::mark_background_permission_mode_accepted(mode) {
        tracing::debug!(
            mode = mode.as_wire(),
            error = %err,
            "failed to persist interactive background permission-mode acceptance"
        );
    }
}

fn ensure_stopped_job_tree_complete(
    job_id: &str,
    tree: &background::StoppedJobTree,
) -> anyhow::Result<()> {
    if tree.failed_children.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "could not stop all work started by {job_id}: {} failure(s)",
        tree.failed_children.len()
    )
}

async fn run_headless_command(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Agents { command: None } => {
            unreachable!("plain agents is routed before headless command dispatch")
        }
        Command::Agents {
            command:
                Some(AgentsCommand::Service {
                    command: service_command,
                }),
        } => run_agent_service_command(service_command).await,
        Command::Bg { command } => run_agent_service_command(command).await,
        Command::Attach { job_id } => {
            agent_view_enabled()?;
            let target = background::attach_background_job(&job_id)?;
            tui::run(target.overrides).await
        }
        Command::Logs { job_id, lines } => background::print_background_logs(&job_id, lines),
        Command::Stop { job_id } | Command::Kill { job_id } => {
            let tree = background::stop_background_job(&job_id)?;
            println!("stopped · {job_id}");
            for child in &tree.stopped_children {
                println!("stopped · {child} (started by {job_id})");
            }
            for (child, error) in &tree.failed_children {
                eprintln!("could not stop · {child} (started by {job_id}): {error}");
            }
            ensure_stopped_job_tree_complete(&job_id, &tree)
        }
        Command::Respawn { job_id, all } => {
            if all {
                let jobs = background::respawn_all_background_jobs()?;
                println!("respawned · {} job(s)", jobs.len());
                for job in jobs {
                    println!("  {}  {}", job.identity.job_id, job.identity.name);
                }
                return Ok(());
            }
            let job_id = job_id.expect("clap requires an id unless --all is present");
            let job = background::respawn_background_job(&job_id)?;
            println!("respawned · {}", job.identity.job_id);
            println!("  source: {job_id}");
            println!(
                "  rebon attach {}       open in this terminal",
                job.identity.job_id
            );
            println!(
                "  rebon logs {}         show recent output",
                job.identity.job_id
            );
            Ok(())
        }
        Command::Rm { job_id } => {
            background::remove_background_job(&job_id)?;
            println!("removed · {job_id}");
            Ok(())
        }
        Command::Reply { job_id, message } => {
            background::reply_to_background_job(&job_id, message.join(" "))?;
            println!("replied · {job_id}");
            Ok(())
        }
        Command::Permit { job_id, option } => {
            if option == 0 {
                anyhow::bail!("permission option numbers start at 1");
            }
            let store = background::cli_default_store();
            store.answer_latest_permission_option(&job_id, option - 1)?;
            println!("permission answered · {job_id} option {option}");
            Ok(())
        }
        Command::BackgroundSupervisor => background::run_background_supervisor(),
        Command::BackgroundWorker { job_id } => background::run_background_worker(job_id).await,
        // stdout is the MCP channel; anything this process logs goes
        // to stderr via the non-TUI tracing default.
        Command::AcpFsMcp => rebon_acp_client::fs_mcp::run_bridge().await,
        // stdout is the MCP connection here too; this route logs to stderr.
        // The jobs it starts are the supervisor's, gated exactly as `--bg`.
        Command::Mcp { command } => {
            rebon_mcp_channel::cli::run(
                command,
                background::cli_default_store(),
                background::rebon_exe(),
                std::sync::Arc::new(background_launch_gate),
            )
            .await
        }
        Command::BrowserMcp { args } => {
            sibling_command::forward_to_sibling("browser-mcp", BROWSER_MCP_SIBLING, args)
        }
        Command::Plugin { command } => run_plugin_command(command).await,
        Command::Remote { command } => rebon_plugin_remote::cli::run_remote_command(command),
        Command::Login(args) => rebon_plugin_onboarding::cli::run_login(args).await,
        Command::Logout(args) => rebon_plugin_onboarding::cli::run_logout(args),
        Command::ComputerUse { args } => {
            sibling_command::forward_to_sibling("computer-use", COMPUTER_USE_SIBLING, args)
        }
        Command::Kernel { command } => kernel_cmd::run(command).await,
        Command::Node { command } => node_cmd::run(command).await,
        Command::LspMcp { args } => {
            sibling_command::forward_to_sibling("lsp-mcp", LSP_MCP_SIBLING, args)
        }
        Command::Update { command } => {
            rebon_plugin_updater::cli::run_update_cli_command(command).await
        }
        // `exec` is intercepted in `main` (it needs the global provider/model
        // flags and stdout-clean tracing), so it never reaches here.
        Command::Exec { .. } => {
            unreachable!("Command::Exec is dispatched in main() before run_headless_command")
        }
        Command::Serve { .. } => {
            unreachable!("Command::Serve is dispatched in main() before run_headless_command")
        }
        Command::Rc { .. } => {
            unreachable!("Command::Rc is dispatched in main() before run_headless_command")
        }
    }
}

/// What `rebon rc` is handed: this binary's store, executable, runtime
/// flags, launch policy, and the readers of files this binary owns.
fn rc_host(runtime: background::BackgroundRuntimeFields) -> rebon_rc_runner::cli::RcHost {
    rebon_rc_runner::cli::RcHost {
        config_home: rebon_config::config_home_dir(),
        store: background::cli_default_store(),
        projects_root: rebon_session::default_projects_root(),
        // The running executable, never a bare `rebon` looked up on PATH.
        rebon_exe: background::rebon_exe(),
        runtime,
        launch_gate: std::sync::Arc::new(background_launch_gate),
        permission_projection: std::sync::Arc::new(|query| {
            let session_id = query.session_id.clone().unwrap_or_default();
            crate::session::host::permission_params(&session_id, query)
        }),
        configured_projects: std::sync::Arc::new(|| {
            Ok(rebon_config::load_rc_projects()?
                .into_iter()
                .map(|project| rebon_rc_runner::projects::ConfiguredProject {
                    path: project.path,
                    label: project.label,
                })
                .collect())
        }),
    }
}

async fn run_plugin_command(command: PluginCommand) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("failed to read the current directory")?;
    let store = plugin::PluginStore::new(rebon_config::config_home_dir(), cwd.clone());
    let installer = plugin::PluginInstaller::new(store, cwd, Vec::new());
    match command {
        PluginCommand::Install {
            source,
            scope,
            sha256,
        } => {
            let record = installer.install(&source, scope, sha256.as_deref())?;
            println!(
                "{}",
                plugin::format_plugin_result("installed", scope, &record)
            );
        }
        PluginCommand::Verify { name, scope } => {
            let report = installer.verify(name.as_deref(), scope)?;
            if report.is_empty() {
                println!("no plugins installed");
            }
            let problems = report.iter().filter(|entry| entry.is_problem()).count();
            for entry in &report {
                println!("{}", entry.describe());
            }
            if problems > 0 {
                anyhow::bail!(
                    "{problems} of {} installed plugins no longer match what was installed",
                    report.len()
                );
            }
        }
        PluginCommand::Enable { name, scope } => {
            let record = installer.enable(&name, scope)?;
            println!(
                "{}",
                plugin::format_plugin_result("enabled", scope, &record)
            );
        }
        PluginCommand::Disable { name, scope } => {
            let record = installer.disable(&name, scope)?;
            println!(
                "{}",
                plugin::format_plugin_result("disabled", scope, &record)
            );
        }
        PluginCommand::Uninstall { name, scope } => {
            if let Some(record) = installer.uninstall(&name, scope)? {
                println!(
                    "{}",
                    plugin::format_plugin_result("uninstalled", scope, &record)
                );
            } else {
                println!(
                    "plugin `{name}` is not installed in {} scope",
                    scope.as_str()
                );
            }
        }
        PluginCommand::List { scope } => {
            print_plugin_records(installer.list(scope)?);
        }
        PluginCommand::Status { name, scope } => {
            print_plugin_records(installer.status(name.as_deref(), scope)?);
        }
    }
    Ok(())
}

fn print_plugin_records(records: Vec<(plugin::PluginScope, plugin::InstalledPluginRecord)>) {
    if records.is_empty() {
        println!("No plugins installed.");
        return;
    }
    for (scope, record) in records {
        let state = if record.enabled {
            "enabled"
        } else {
            "disabled"
        };
        let source = record
            .source
            .as_deref()
            .unwrap_or(match record.source_kind {
                crate::plugin::store::PluginSourceKind::Local => "local",
                crate::plugin::store::PluginSourceKind::Builtin => "builtin",
            });
        println!(
            "{} {}  {}  {}  {}",
            record.name,
            record.version,
            scope.as_str(),
            state,
            source
        );
    }
}

async fn run_agent_service_command(command: AgentServiceCommand) -> anyhow::Result<()> {
    let store = background::cli_default_store();
    match command {
        AgentServiceCommand::Status => {
            println!("supervisorService: per-user scheduler");
            println!("daemonPath: {}", store.daemon_dir().display());
            println!("rosterPath: {}", store.roster_path().display());
            println!("daemonLogPath: {}", store.daemon_log_path().display());
            match store.read_roster() {
                Ok(roster) => {
                    println!("supervisorPid: {}", roster.supervisor_pid);
                    println!("rosterUpdatedAtMs: {}", roster.updated_at_ms);
                    println!("trackedJobs: {}", roster.jobs.len());
                }
                Err(_) => println!("rosterState: (none)"),
            }
            print_service_lines(rebon_plugin_updater::cli::supervisor_service_status_lines());
            Ok(())
        }
        AgentServiceCommand::Install => {
            print_service_lines(rebon_plugin_updater::cli::install_supervisor_service()?);
            Ok(())
        }
        AgentServiceCommand::Uninstall => {
            print_service_lines(rebon_plugin_updater::cli::uninstall_supervisor_service()?);
            Ok(())
        }
    }
}

/// What the scheduler registration reported, one line per line. The plugin
/// composes the sentences; printing them is this binary's half.
fn print_service_lines(lines: Vec<String>) {
    for line in lines {
        println!("{line}");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StartupRoute {
    AgentView,
    HeadlessCommand { tui_tracing: bool },
    RuntimeSession,
}

fn classify_cli_startup(cli: &Cli) -> anyhow::Result<StartupRoute> {
    validate_cli_combinations(cli)?;
    if matches!(
        cli.command.as_ref(),
        Some(Command::Agents { command: None })
    ) {
        return Ok(StartupRoute::AgentView);
    }
    if let Some(command) = cli.command.as_ref() {
        return Ok(StartupRoute::HeadlessCommand {
            tui_tracing: command_uses_tui_tracing(command),
        });
    }
    Ok(StartupRoute::RuntimeSession)
}

fn validate_cli_combinations(cli: &Cli) -> anyhow::Result<()> {
    if cli.bg.is_some() && cli.command.is_some() {
        anyhow::bail!("--bg cannot be combined with a subcommand");
    }
    if cli.continue_session && cli.bg.is_some() {
        anyhow::bail!("--continue cannot be combined with --bg");
    }
    if cli.continue_session && cli.acp {
        anyhow::bail!("--continue cannot be combined with --acp");
    }
    if cli.continue_session && cli.command.is_some() {
        anyhow::bail!("--continue cannot be combined with a subcommand");
    }
    Ok(())
}

fn command_uses_tui_tracing(command: &Command) -> bool {
    matches!(command, Command::Attach { .. })
}

/// The engine's composed futures are enormous in unoptimized builds, and the
/// process main thread gets only 1 MiB on Windows — a debug background worker
/// overflowed it before its first turn. Every route therefore runs on a
/// thread with an explicit, roomy stack, and the Tokio workers get the same.
const MAIN_STACK_BYTES: usize = 16 * 1024 * 1024;

fn main() -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("rebon-main".into())
        .stack_size(MAIN_STACK_BYTES)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(MAIN_STACK_BYTES)
                .build()
                .expect("failed to build the rebon Tokio runtime")
                .block_on(async_main())
        })
        .expect("failed to spawn the rebon main thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// One route, and then the plane it may have started.
///
/// Every route funnels through here, so this is the one place that covers all
/// of them. It has to be *before* the runtime is dropped, and the runtime is
/// dropped the moment `block_on` in `main` returns: on Windows a read of the
/// host's stdout is backed by the blocking pool, `Runtime::drop` waits for
/// blocking tasks rather than aborting them, and a read parked on a live child
/// never finishes. The wait also lands before `Termination` prints `main`'s
/// error -- which is how a rebon that could not resolve a provider used to
/// park forever with the explanation already in hand.
async fn async_main() -> anyhow::Result<()> {
    let outcome = route_main().await;
    rebon_harness::shutdown_process_plugin_plane().await;
    outcome
}

async fn route_main() -> anyhow::Result<()> {
    let startup_started = std::time::Instant::now();
    let mut cli = Cli::parse();
    // Runs before any route can touch `config.json`: the seeding rule keys off
    // "config exists but has no flag", so a first run must still look like a
    // first run here.
    rebon_config::migrate_claude_codex_fallback_default();
    // Provider definitions move to `~/.rebon/providers/` here, before any
    // route resolves one. Reversible and non-fatal: a failure leaves
    // `config.json` authoritative.
    rebon_config::migrate_providers_to_store();
    let route = classify_cli_startup(&cli)?;
    let cwd_scope = match cli.cwd.as_ref() {
        Some(cwd) => {
            let canonical = cwd
                .canonicalize()
                .with_context(|| format!("failed to resolve --cwd {}", cwd.display()))?;
            std::env::set_current_dir(&canonical)
                .with_context(|| format!("failed to enter --cwd {}", canonical.display()))?;
            Some(canonical)
        }
        None => None,
    };
    match route {
        StartupRoute::AgentView => {
            init_tracing(true);
            tracing::info!(
                elapsed_ms = startup_started.elapsed().as_millis() as u64,
                route = "agent_view",
                "rebon startup: cli route selected"
            );
            agent_view_enabled()?;
            ensure_background_permission_mode_allowed(cli.permission_mode)?;
            let (channels, development_channels) = cli_channel_entries(&cli)?;
            background::keep_supervisor_alive_for_agent_view();
            let mut overrides =
                runtime_overrides_from_cli(&cli, channels, development_channels, None)?;
            overrides.startup_agent_view = true;
            overrides.startup_agent_view_cwd_scope = cwd_scope
                .as_ref()
                .map(|path| path.to_string_lossy().to_string());
            if let Some(message) = rebon_plugin_updater::cli::supervisor_service_install_hint() {
                overrides.startup_notices.push(message);
            }
            return tui::run(overrides).await;
        }
        StartupRoute::HeadlessCommand { tui_tracing } => {
            let command = cli
                .command
                .take()
                .expect("headless command route requires a parsed subcommand");
            init_tracing(tui_tracing);
            tracing::info!(
                elapsed_ms = startup_started.elapsed().as_millis() as u64,
                route = "headless_command",
                tui_tracing,
                "rebon startup: cli route selected"
            );
            match command {
                Command::Exec {
                    prompt,
                    json,
                    resume,
                    max_iterations,
                    capability,
                    verify_rounds,
                    verify_budget,
                    max_duration,
                } => {
                    return exec::run(exec::ExecArgs {
                        prompt: prompt.join(" "),
                        json,
                        resume,
                        verify_rounds,
                        verify_budget_sec: verify_budget,
                        max_duration_sec: max_duration,
                        max_iterations: max_iterations.map(std::num::NonZeroUsize::get),
                        provider: cli.provider.clone(),
                        model: cli.model.clone(),
                        effort: cli.effort,
                        permission_mode: cli.permission_mode,
                        capability_mode: capability,
                        plugin_dirs: cli.plugin_dirs.clone(),
                    })
                    .await;
                }
                Command::Serve {
                    host,
                    port,
                    token,
                    open,
                    web_ui,
                    ipc,
                } => {
                    let (channels, development_channels) = cli_channel_entries(&cli)?;
                    let overrides =
                        runtime_overrides_from_cli(&cli, channels, development_channels, None)?;
                    return serve::run(
                        overrides,
                        serve::ServeArgs {
                            host,
                            port,
                            token,
                            open,
                            web_ui,
                            ipc,
                        },
                    )
                    .await;
                }
                // Like `serve`: the global runtime flags apply to every
                // session the runner opens, so they are read here.
                Command::Rc { command } => {
                    let (channels, development_channels) = cli_channel_entries(&cli)?;
                    let overrides =
                        runtime_overrides_from_cli(&cli, channels, development_channels, None)?;
                    let runtime = {
                        use crate::background::RuntimeFieldsExt;
                        background::BackgroundRuntimeFields::from_runtime_override(&overrides)
                    };
                    return rebon_rc_runner::cli::run(command, rc_host(runtime)).await;
                }
                other => return run_headless_command(other).await,
            }
        }
        StartupRoute::RuntimeSession => {}
    }
    init_tracing(!cli.acp);
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        route = if cli.acp {
            "acp"
        } else if cli.bg.is_some() {
            "background"
        } else {
            "tui"
        },
        "rebon startup: cli route selected"
    );

    let (channels, development_channels) = cli_channel_entries(&cli)?;

    let startup_resume = if !cli.acp && cli.bg.is_none() {
        cli.resume
            .clone()
            .map(tui::StartupResumeIntent::Exact)
            .or_else(|| {
                cli.continue_session
                    .then_some(tui::StartupResumeIntent::ContinueLatest)
            })
    } else {
        None
    };
    let overrides = runtime_overrides_from_cli(&cli, channels, development_channels, None)?;

    if let Some(prompt) = cli.bg {
        agent_view_enabled()?;
        let store = background::cli_default_store();
        let job = background::launch_background_prompt(
            &store,
            background::BackgroundLaunchOptions {
                prompt,
                images: Vec::new(),
                cwd: std::env::current_dir()
                    .context("failed to read the current directory for --bg")?,
                isolate_in_worktree: true,
                require_worktree: false,
                preserve_worktree_on_success: false,
                queue_session: false,
                runtime: background::BackgroundRuntimeFields::from_runtime_override(&overrides),
                name: cli.name,
                agent_type: cli.agent,
                parent_job_id: None,
            },
        )?;
        if let Some(message) = rebon_plugin_updater::cli::supervisor_service_install_hint() {
            eprintln!("{message}");
        }
        println!("backgrounded · {}", job.identity.job_id);
        println!("  rebon agents          list sessions");
        println!(
            "  rebon attach {}       open in this terminal",
            job.identity.job_id
        );
        println!(
            "  rebon logs {}         show recent output",
            job.identity.job_id
        );
        println!(
            "  rebon stop {}         stop this session",
            job.identity.job_id
        );
        return Ok(());
    }

    if cli.acp {
        let transport = match cli.acp_port {
            Some(port) => acp::AcpTransport::Tcp {
                host: cli.acp_host.unwrap_or_else(|| "127.0.0.1".to_string()),
                port,
            },
            None => acp::AcpTransport::Stdio,
        };
        acp::run_acp_server(overrides, transport).await
    } else {
        record_interactive_background_permission_mode_acceptance(cli.permission_mode);
        tui::run_with_startup_resume(overrides, startup_resume).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use rebon_plugin_updater::cli::{UpdateCliCommand, UpdateServiceCommand};

    #[test]
    fn rotated_rebon_log_name_matches_timestamp_shape_only() {
        assert!(is_rotated_rebon_log_name("rebon-2026-07-08T09-10-11.log"));
        assert!(!is_rotated_rebon_log_name("rebon.log"));
        assert!(!is_rotated_rebon_log_name("rebon-2026-07-08T09:10:11.log"));
        assert!(!is_rotated_rebon_log_name("other-2026-07-08T09-10-11.log"));
    }

    #[test]
    fn prune_old_tui_logs_keeps_latest_five_history_files() {
        let tmp = tempfile::tempdir().unwrap();
        for day in 1..=7 {
            std::fs::write(
                tmp.path()
                    .join(format!("rebon-2026-07-{day:02}T00-00-00.log")),
                format!("log {day}"),
            )
            .unwrap();
        }
        std::fs::write(tmp.path().join("rebon.log"), "current").unwrap();
        std::fs::write(tmp.path().join("unrelated.log"), "keep").unwrap();

        prune_old_tui_logs(tmp.path()).unwrap();

        assert!(!tmp.path().join("rebon-2026-07-01T00-00-00.log").exists());
        assert!(!tmp.path().join("rebon-2026-07-02T00-00-00.log").exists());
        for day in 3..=7 {
            assert!(tmp
                .path()
                .join(format!("rebon-2026-07-{day:02}T00-00-00.log"))
                .exists());
        }
        assert!(tmp.path().join("rebon.log").exists());
        assert!(tmp.path().join("unrelated.log").exists());
    }

    #[test]
    fn continue_flag_parses_and_conflicts_with_resume() {
        let cli = Cli::parse_from(["rebon", "--continue"]);
        assert!(cli.continue_session);
        assert_eq!(cli.resume, None);

        let short = Cli::parse_from(["rebon", "-c"]);
        assert!(short.continue_session);

        let err = Cli::try_parse_from(["rebon", "--continue", "--resume", "sess-1"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn latest_stopped_session_id_selects_newest_inactive_session() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let cwd = "/tmp/rebon-continue";
        rebon_session::write_transcript_entries(
            &root,
            cwd,
            "sess-old",
            vec![rebon_session::TranscriptWriteEntry::new(
                "user",
                serde_json::json!({"message": {"role": "user", "content": "old"}}),
            )
            .with_uuid("u-old")
            .with_timestamp("2024-01-01T00:00:00.000Z")],
        )
        .unwrap();
        rebon_session::write_transcript_entries(
            &root,
            cwd,
            "sess-new",
            vec![rebon_session::TranscriptWriteEntry::new(
                "user",
                serde_json::json!({"message": {"role": "user", "content": "new"}}),
            )
            .with_uuid("u-new")
            .with_timestamp("2024-01-02T00:00:00.000Z")],
        )
        .unwrap();
        let _lock = rebon_session::try_acquire_session_active_lock(&root, cwd, "sess-new")
            .unwrap()
            .unwrap();

        assert_eq!(
            latest_stopped_session_id(&root, cwd).as_deref(),
            Some("sess-old")
        );
    }

    #[test]
    fn latest_stopped_session_id_returns_none_when_only_active_sessions_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let cwd = "/tmp/rebon-continue-active";
        rebon_session::write_transcript_entries(
            &root,
            cwd,
            "sess-active",
            vec![rebon_session::TranscriptWriteEntry::new(
                "user",
                serde_json::json!({"message": {"role": "user", "content": "active"}}),
            )
            .with_uuid("u-active")
            .with_timestamp("2024-01-01T00:00:00.000Z")],
        )
        .unwrap();
        let _lock = rebon_session::try_acquire_session_active_lock(&root, cwd, "sess-active")
            .unwrap()
            .unwrap();

        assert_eq!(latest_stopped_session_id(&root, cwd), None);
    }

    #[test]
    fn acp_port_requires_acp() {
        let err = Cli::try_parse_from(["rebon", "--acp-port", "0"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn acp_host_requires_acp() {
        let err = Cli::try_parse_from(["rebon", "--acp-host", "127.0.0.1"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn acp_without_port_keeps_stdio_transport_selection() {
        let cli = Cli::parse_from(["rebon", "--acp"]);
        assert!(cli.acp);
        assert_eq!(cli.acp_port, None);
        assert_eq!(cli.acp_host, None);
    }

    /// A directory laid out the way an install is: `rebon` with the bridge
    /// binary beside it. Returned so the caller keeps the temp dir alive.
    fn install_with_lsp_bridge() -> (tempfile::TempDir, PathBuf) {
        let install = tempfile::tempdir().unwrap();
        let executable = install
            .path()
            .join(rebon_types::sibling_binary::file_name("rebon"));
        std::fs::write(
            install
                .path()
                .join(rebon_types::sibling_binary::file_name(LSP_MCP_SIBLING)),
            b"binary",
        )
        .unwrap();
        (install, executable)
    }

    #[test]
    fn lsp_rust_appends_inline_mcp_config() {
        let cli = Cli::parse_from(["rebon", "--lsp", "rust"]);
        assert_eq!(cli.lsp, vec![LspKind::Rust]);

        let (_install, executable) = install_with_lsp_bridge();
        let overrides =
            runtime_overrides_from_cli(&cli, Vec::new(), Vec::new(), Some(&executable)).unwrap();
        assert_eq!(overrides.mcp_configs.len(), 1);

        let temp = tempfile::tempdir().unwrap();
        let configs = crate::mcp_config::collect_default_mcp_configs_with_overrides(
            temp.path(),
            &overrides.mcp_configs,
            true,
        )
        .unwrap();
        assert_eq!(configs.len(), 1);
        let crate::mcp_config::McpServerConfig::Stdio(config) = &configs[0].config else {
            panic!("expected stdio MCP config");
        };
        assert_eq!(config.name, RUST_LSP_SERVER_NAME);
        // The resolved sibling binary, not `rebon` with a subcommand: the
        // shell that would accept the old form is gone in 0.26.
        assert_eq!(
            config.command,
            _install
                .path()
                .join(rebon_types::sibling_binary::file_name(LSP_MCP_SIBLING))
                .to_string_lossy()
        );
        assert_eq!(config.args, vec!["rust"]);
        assert_eq!(
            config.cwd.as_deref(),
            Some(std::env::current_dir().unwrap().to_string_lossy().as_ref())
        );
    }

    #[test]
    fn strict_mcp_config_preserves_lsp_rust_config() {
        let cli = Cli::parse_from(["rebon", "--strict-mcp-config", "--lsp", "rust"]);
        let (_install, executable) = install_with_lsp_bridge();
        let overrides =
            runtime_overrides_from_cli(&cli, Vec::new(), Vec::new(), Some(&executable)).unwrap();

        assert!(overrides.strict_mcp_config);
        let temp = tempfile::tempdir().unwrap();
        let configs = crate::mcp_config::collect_default_mcp_configs_with_overrides(
            temp.path(),
            &overrides.mcp_configs,
            overrides.strict_mcp_config,
        )
        .unwrap();

        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].config.name(), "rust_lsp");
    }

    #[test]
    fn serve_parses_with_defaults_and_global_flags() {
        let cli = Cli::parse_from(["rebon", "serve"]);
        assert_eq!(
            cli.command,
            Some(Command::Serve {
                host: serve::DEFAULT_HOST.to_string(),
                port: serve::DEFAULT_PORT,
                token: None,
                open: false,
                web_ui: None,
                ipc: None,
            })
        );
        assert!(matches!(
            classify_cli_startup(&cli).unwrap(),
            StartupRoute::HeadlessCommand { tui_tracing: false }
        ));

        let cli = Cli::parse_from([
            "rebon",
            "serve",
            "--host",
            "0.0.0.0",
            "--port",
            "0",
            "--token",
            "t0k3n",
            "--open",
            "--web-ui",
            "assets/web-ui/dist",
            "--model",
            "gpt-5.6",
        ]);
        assert_eq!(
            cli.command,
            Some(Command::Serve {
                host: "0.0.0.0".to_string(),
                port: 0,
                token: Some("t0k3n".to_string()),
                open: true,
                web_ui: Some(std::path::PathBuf::from("assets/web-ui/dist")),
                ipc: None,
            })
        );
        assert_eq!(cli.model.as_deref(), Some("gpt-5.6"));
    }

    #[test]
    fn exec_configuration_flags_parse_after_subcommand() {
        let cli = Cli::parse_from([
            "rebon",
            "exec",
            "--model",
            "gpt-5.6",
            "--effort",
            "max",
            "--permission-mode",
            "bypass",
            "inspect",
            "the project",
        ]);
        assert_eq!(
            cli.command,
            Some(Command::Exec {
                prompt: vec!["inspect".to_string(), "the project".to_string()],
                json: false,
                resume: None,
                max_iterations: None,
                capability: AgentCapabilityMode::Normal,
                verify_rounds: 0,
                verify_budget: None,
                max_duration: None,
            })
        );
        assert_eq!(cli.model.as_deref(), Some("gpt-5.6"));
        assert_eq!(cli.effort, Some(ReasoningEffort::Max));
        assert_eq!(cli.permission_mode, Some(PermissionMode::BypassPermissions));
    }

    #[test]
    fn effort_flag_keeps_its_three_aliases_and_its_error_text() {
        // These spellings are the flag's promise, not the type's:
        // `ReasoningEffort::from_str` accepts no aliases, so nothing else
        // pins them. Written when the flag stopped carrying its own copy
        // of the five-level table.
        for raw in ["med", "MED", " med "] {
            assert_eq!(parse_effort_level(raw), Ok(ReasoningEffort::Medium));
        }
        for raw in ["x_high", "x-high", "extra_high", "EXTRA-HIGH"] {
            assert_eq!(parse_effort_level(raw), Ok(ReasoningEffort::XHigh));
        }
        for raw in ["low", "Medium", "HIGH", "xhigh", "max"] {
            assert!(parse_effort_level(raw).is_ok(), "{raw} must parse");
        }
        // The message names the normalised input, as it always has.
        assert_eq!(
            parse_effort_level("Highest"),
            Err("invalid effort level `highest`; expected low, medium, high, xhigh, or max".into())
        );
        assert!(parse_effort_level("auto").is_err());
    }

    #[test]
    fn exec_max_iterations_parses_and_rejects_zero() {
        let cli = Cli::parse_from(["rebon", "exec", "--max-iterations", "128", "inspect"]);
        assert_eq!(
            cli.command,
            Some(Command::Exec {
                prompt: vec!["inspect".to_string()],
                json: false,
                resume: None,
                max_iterations: std::num::NonZeroUsize::new(128),
                capability: AgentCapabilityMode::Normal,
                verify_rounds: 0,
                verify_budget: None,
                max_duration: None,
            })
        );

        assert!(
            Cli::try_parse_from(["rebon", "exec", "--max-iterations", "0", "inspect",]).is_err()
        );
    }

    /// The two ceilings are separate flags because they stop different things:
    /// `--verify-budget` declines to open another audit round, `--max-duration`
    /// cancels the work in progress. Passing one must not set the other.
    #[test]
    fn exec_max_duration_is_its_own_ceiling() {
        let cli = Cli::parse_from(["rebon", "exec", "--max-duration", "7200", "inspect"]);
        assert!(matches!(
            cli.command,
            Some(Command::Exec {
                max_duration: Some(7200),
                verify_budget: None,
                ..
            })
        ));

        let default = Cli::parse_from(["rebon", "exec", "inspect"]);
        assert!(matches!(
            default.command,
            Some(Command::Exec {
                max_duration: None,
                ..
            })
        ));
    }

    #[test]
    fn exec_capability_defaults_to_normal_and_accepts_minimal() {
        let default = Cli::parse_from(["rebon", "exec", "inspect"]);
        assert!(matches!(
            default.command,
            Some(Command::Exec {
                capability: AgentCapabilityMode::Normal,
                ..
            })
        ));

        let minimal = Cli::parse_from(["rebon", "exec", "--capability", "Minimal", "inspect"]);
        assert!(matches!(
            minimal.command,
            Some(Command::Exec {
                capability: AgentCapabilityMode::Minimal,
                ..
            })
        ));

        assert!(Cli::try_parse_from(["rebon", "exec", "--capability", "tiny", "inspect"]).is_err());
    }

    #[test]
    fn agent_view_configuration_flags_parse_after_subcommand() {
        let cli = Cli::parse_from([
            "rebon",
            "agents",
            "--model",
            "gpt-5.5",
            "--effort",
            "xhigh",
            "--permission-mode",
            "acceptEdits",
            "--add-dir",
            "../shared",
            "--mcp-config",
            "{}",
            "--strict-mcp-config",
        ]);
        assert_eq!(cli.command, Some(Command::Agents { command: None }));
        assert_eq!(cli.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(cli.effort, Some(ReasoningEffort::XHigh));
        assert_eq!(cli.permission_mode, Some(PermissionMode::AcceptEdits));
        assert_eq!(cli.add_dirs, vec![PathBuf::from("../shared")]);
        assert_eq!(cli.mcp_configs, vec!["{}".to_string()]);
        assert!(cli.strict_mcp_config);
    }

    #[test]
    fn permission_mode_alias_parses_legacy_modes() {
        let bypass = Cli::parse_from(["rebon", "--mode", "bypassPermissions"]);
        assert_eq!(
            bypass.permission_mode,
            Some(PermissionMode::BypassPermissions)
        );

        let accept = Cli::parse_from(["rebon", "--mode", "acceptEdits"]);
        assert_eq!(accept.permission_mode, Some(PermissionMode::AcceptEdits));

        // Every mode with runtime semantics must be reachable from the flag.
        let dont_ask = Cli::parse_from(["rebon", "--mode", "dontAsk"]);
        assert_eq!(dont_ask.permission_mode, Some(PermissionMode::DontAsk));
    }

    #[test]
    fn ui_mode_accepts_screen_and_inline() {
        let screen = Cli::parse_from(["rebon", "--ui-mode", "screen"]);
        assert_eq!(screen.ui_mode, Some(UiMode::Screen));
        let inline = Cli::parse_from(["rebon", "--ui-mode", "inline"]);
        assert_eq!(inline.ui_mode, Some(UiMode::Inline));
    }

    #[test]
    fn ui_mode_rejects_invalid_value() {
        let err = Cli::try_parse_from(["rebon", "--ui-mode", "float"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn ui_mode_does_not_change_acp_flag_parsing() {
        let cli = Cli::parse_from(["rebon", "--acp", "--ui-mode", "inline"]);
        assert!(cli.acp);
        assert_eq!(cli.ui_mode, Some(UiMode::Inline));
    }

    #[test]
    fn update_status_subcommand_parses_without_acp_or_tui_flags() {
        let cli = Cli::parse_from(["rebon", "update", "status"]);
        assert_eq!(
            cli.command,
            Some(Command::Update {
                command: UpdateCliCommand::Status
            })
        );
        assert!(!cli.acp);
    }

    #[test]
    fn update_requires_status_subcommand() {
        let err = Cli::try_parse_from(["rebon", "update"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn update_service_subcommands_parse_without_tui_startup() {
        let cases = [
            ("status", UpdateServiceCommand::Status),
            ("run-once", UpdateServiceCommand::RunOnce),
            ("install", UpdateServiceCommand::Install),
            ("uninstall", UpdateServiceCommand::Uninstall),
        ];
        for (raw, parsed) in cases {
            let cli = Cli::parse_from(["rebon", "update", "service", raw]);
            assert_eq!(
                cli.command,
                Some(Command::Update {
                    command: UpdateCliCommand::Service { command: parsed }
                })
            );
            assert!(!cli.acp);
        }
    }

    #[test]
    fn agents_subcommand_parses() {
        let cli = Cli::parse_from(["rebon", "agents"]);
        assert_eq!(cli.command, Some(Command::Agents { command: None }));
    }

    #[test]
    fn agents_service_subcommands_parse_without_tui_startup() {
        let cases = [
            ("status", AgentServiceCommand::Status),
            ("install", AgentServiceCommand::Install),
            ("uninstall", AgentServiceCommand::Uninstall),
        ];
        for (raw, parsed) in cases {
            let cli = Cli::parse_from(["rebon", "agents", "service", raw]);
            assert_eq!(
                cli.command,
                Some(Command::Agents {
                    command: Some(AgentsCommand::Service { command: parsed })
                })
            );
            assert!(!cli.acp);
        }
    }

    #[test]
    fn bg_service_alias_subcommands_parse_without_tui_startup() {
        let cases = [
            ("status", AgentServiceCommand::Status),
            ("install", AgentServiceCommand::Install),
            ("uninstall", AgentServiceCommand::Uninstall),
        ];
        for (raw, parsed) in cases {
            let cli = Cli::parse_from(["rebon", "bg", raw]);
            assert_eq!(cli.command, Some(Command::Bg { command: parsed }));
            assert!(!cli.acp);
            assert_eq!(
                classify_cli_startup(&cli).unwrap(),
                StartupRoute::HeadlessCommand { tui_tracing: false }
            );
        }
    }

    #[test]
    fn bg_service_alias_accepts_global_flags_without_runtime_session_startup() {
        let cli = Cli::parse_from([
            "rebon",
            "bg",
            "status",
            "--provider",
            "openrouter",
            "--model",
            "gpt-5.5",
            "--effort",
            "high",
            "--permission-mode",
            "acceptEdits",
            "--add-dir",
            "../shared",
            "--mcp-config",
            "{}",
            "--strict-mcp-config",
        ]);
        assert_eq!(
            cli.command,
            Some(Command::Bg {
                command: AgentServiceCommand::Status
            })
        );
        assert_eq!(cli.provider.as_deref(), Some("openrouter"));
        assert_eq!(cli.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(cli.effort, Some(ReasoningEffort::High));
        assert_eq!(cli.permission_mode, Some(PermissionMode::AcceptEdits));
        assert_eq!(cli.add_dirs, vec![PathBuf::from("../shared")]);
        assert_eq!(cli.mcp_configs, vec!["{}".to_string()]);
        assert!(cli.strict_mcp_config);
        assert_eq!(
            classify_cli_startup(&cli).unwrap(),
            StartupRoute::HeadlessCommand { tui_tracing: false }
        );
    }

    #[test]
    fn bg_service_alias_requires_a_service_subcommand() {
        let err = Cli::try_parse_from(["rebon", "bg"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn bg_service_alias_rejects_unknown_service_subcommands() {
        let err = Cli::try_parse_from(["rebon", "bg", "run-once"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn bg_service_alias_rejects_background_prompt_mode_combination_before_runtime_startup() {
        let cli = Cli::parse_from(["rebon", "--bg", "do work", "bg", "status"]);
        let err = classify_cli_startup(&cli).unwrap_err();
        assert!(err
            .to_string()
            .contains("--bg cannot be combined with a subcommand"));
    }

    #[test]
    fn bg_service_alias_rejects_continue_combination_before_runtime_startup() {
        let cli = Cli::parse_from(["rebon", "--continue", "bg", "status"]);
        let err = classify_cli_startup(&cli).unwrap_err();
        assert!(err
            .to_string()
            .contains("--continue cannot be combined with a subcommand"));
    }

    #[test]
    fn bg_service_alias_help_lists_only_lightweight_service_commands() {
        let mut command = Cli::command();
        let bg = command
            .find_subcommand_mut("bg")
            .expect("bg subcommand should be registered");
        let mut help = Vec::new();
        bg.write_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        assert!(help.contains("status"));
        assert!(help.contains("install"));
        assert!(help.contains("uninstall"));
        assert!(!help.contains("__background-worker"));
        assert!(!help.contains("__background-supervisor"));
        assert!(!help.contains("attach"));
        assert!(!help.contains("logs"));
        assert!(!help.contains("respawn"));
    }

    #[test]
    fn bg_service_alias_route_is_distinct_from_agent_view_and_attach_tui_routes() {
        let bg = Cli::parse_from(["rebon", "bg", "status"]);
        assert_eq!(
            classify_cli_startup(&bg).unwrap(),
            StartupRoute::HeadlessCommand { tui_tracing: false }
        );

        let agents = Cli::parse_from(["rebon", "agents"]);
        assert_eq!(
            classify_cli_startup(&agents).unwrap(),
            StartupRoute::AgentView
        );

        let attach = Cli::parse_from(["rebon", "attach", "bg-1"]);
        assert_eq!(
            classify_cli_startup(&attach).unwrap(),
            StartupRoute::HeadlessCommand { tui_tracing: true }
        );
    }

    #[test]
    fn bg_service_alias_parse_loop_stays_allocation_local_and_side_effect_free() {
        for _ in 0..512 {
            let cli = Cli::parse_from(["rebon", "bg", "status"]);
            assert_eq!(
                classify_cli_startup(&cli).unwrap(),
                StartupRoute::HeadlessCommand { tui_tracing: false }
            );
        }
    }

    #[test]
    fn cwd_flag_parses_before_and_after_subcommands() {
        let before = Cli::parse_from(["rebon", "--cwd", ".", "agents"]);
        assert_eq!(before.cwd, Some(PathBuf::from(".")));
        assert_eq!(before.command, Some(Command::Agents { command: None }));

        let after = Cli::parse_from(["rebon", "agents", "--cwd", "."]);
        assert_eq!(after.cwd, Some(PathBuf::from(".")));
        assert_eq!(after.command, Some(Command::Agents { command: None }));
    }

    #[test]
    fn bg_flag_parses_prompt() {
        let cli = Cli::parse_from([
            "rebon",
            "--bg",
            "do work",
            "--name",
            "worker",
            "--agent",
            "code-reviewer",
            "--effort",
            "xhigh",
            "--permission-mode",
            "acceptEdits",
        ]);
        assert_eq!(cli.bg.as_deref(), Some("do work"));
        assert_eq!(cli.name.as_deref(), Some("worker"));
        assert_eq!(cli.agent.as_deref(), Some("code-reviewer"));
        assert_eq!(cli.effort, Some(ReasoningEffort::XHigh));
        assert_eq!(cli.permission_mode, Some(PermissionMode::AcceptEdits));
        assert_eq!(cli.command, None);
    }

    #[test]
    fn attach_and_logs_parse() {
        let attach = Cli::parse_from(["rebon", "attach", "bg-1"]);
        assert_eq!(
            attach.command,
            Some(Command::Attach {
                job_id: "bg-1".to_string()
            })
        );
        let logs = Cli::parse_from(["rebon", "logs", "bg-1", "--lines", "7"]);
        assert_eq!(
            logs.command,
            Some(Command::Logs {
                job_id: "bg-1".to_string(),
                lines: 7
            })
        );
    }

    #[test]
    fn stop_command_succeeds_when_the_complete_tree_stops() {
        let tree = background::StoppedJobTree {
            stopped_children: vec!["bg-child".to_string()],
            failed_children: Vec::new(),
        };

        ensure_stopped_job_tree_complete("bg-root", &tree).unwrap();
    }

    #[test]
    fn stop_command_fails_for_a_reported_child_stop_failure() {
        let tree = background::StoppedJobTree {
            stopped_children: Vec::new(),
            failed_children: vec![("bg-child".to_string(), "permission denied".to_string())],
        };

        let error = ensure_stopped_job_tree_complete("bg-root", &tree)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "could not stop all work started by bg-root: 1 failure(s)"
        );
    }

    #[test]
    fn stop_command_fails_for_the_sweep_cap_report() {
        let tree = background::StoppedJobTree {
            stopped_children: Vec::new(),
            failed_children: vec![(
                "bg-root".to_string(),
                "children were still appearing after 8 sweeps; some may still be running"
                    .to_string(),
            )],
        };

        assert!(ensure_stopped_job_tree_complete("bg-root", &tree).is_err());
    }

    #[test]
    fn stop_command_fails_for_a_child_state_read_report() {
        let tree = background::StoppedJobTree {
            stopped_children: Vec::new(),
            failed_children: vec![(
                "bg-child".to_string(),
                "failed to read background state".to_string(),
            )],
        };

        assert!(ensure_stopped_job_tree_complete("bg-root", &tree).is_err());
    }

    #[test]
    fn stop_and_kill_parse() {
        let stop = Cli::parse_from(["rebon", "stop", "bg-1"]);
        assert_eq!(
            stop.command,
            Some(Command::Stop {
                job_id: "bg-1".to_string()
            })
        );
        let kill = Cli::parse_from(["rebon", "kill", "bg-1"]);
        assert_eq!(
            kill.command,
            Some(Command::Kill {
                job_id: "bg-1".to_string()
            })
        );
    }

    #[test]
    fn respawn_and_rm_parse() {
        let respawn = Cli::parse_from(["rebon", "respawn", "bg-1"]);
        assert_eq!(
            respawn.command,
            Some(Command::Respawn {
                job_id: Some("bg-1".to_string()),
                all: false
            })
        );
        let respawn_all = Cli::parse_from(["rebon", "respawn", "--all"]);
        assert_eq!(
            respawn_all.command,
            Some(Command::Respawn {
                job_id: None,
                all: true
            })
        );
        let rm = Cli::parse_from(["rebon", "rm", "bg-1"]);
        assert_eq!(
            rm.command,
            Some(Command::Rm {
                job_id: "bg-1".to_string()
            })
        );
        let reply = Cli::parse_from(["rebon", "reply", "bg-1", "continue", "work"]);
        assert_eq!(
            reply.command,
            Some(Command::Reply {
                job_id: "bg-1".to_string(),
                message: vec!["continue".to_string(), "work".to_string()]
            })
        );
        let permit = Cli::parse_from(["rebon", "permit", "bg-1", "2"]);
        assert_eq!(
            permit.command,
            Some(Command::Permit {
                job_id: "bg-1".to_string(),
                option: 2
            })
        );
    }

    #[test]
    fn remote_subcommands_parse() {
        let add = Cli::parse_from([
            "rebon",
            "remote",
            "add",
            "prod",
            "deploy@build.example",
            "--path",
            "/srv/app",
            "--install",
            "push",
        ]);
        assert_eq!(
            add.command,
            Some(Command::Remote {
                command: rebon_plugin_remote::cli::RemoteCommand::Add {
                    name: "prod".to_string(),
                    target: "deploy@build.example".to_string(),
                    path: Some("/srv/app".to_string()),
                    install: Some(rebon_plugin_remote::InstallStrategy::Push),
                    transport: None,
                    identity_file: None,
                    ssh_config: None,
                    jump_host: None,
                    server_dir: None,
                    forward_credentials: false,
                    force: false,
                    no_setup: false,
                }
            })
        );

        let install = Cli::parse_from(["rebon", "remote", "install", "prod", "--force"]);
        assert_eq!(
            install.command,
            Some(Command::Remote {
                command: rebon_plugin_remote::cli::RemoteCommand::Install {
                    name: "prod".to_string(),
                    force: true,
                    package: None,
                    registry: None,
                }
            })
        );

        let status = Cli::parse_from(["rebon", "remote", "status", "prod"]);
        assert_eq!(
            status.command,
            Some(Command::Remote {
                command: rebon_plugin_remote::cli::RemoteCommand::Status {
                    name: "prod".to_string()
                }
            })
        );
    }

    #[test]
    fn login_and_logout_parse_as_subcommands() {
        assert_eq!(
            Cli::parse_from(["rebon", "login"]).command,
            Some(Command::Login(rebon_plugin_onboarding::cli::LoginArgs {
                account: None,
                status: false,
            }))
        );
        assert_eq!(
            Cli::parse_from(["rebon", "login", "copilot"]).command,
            Some(Command::Login(rebon_plugin_onboarding::cli::LoginArgs {
                account: Some("copilot".to_string()),
                status: false,
            }))
        );
        assert_eq!(
            Cli::parse_from(["rebon", "login", "--status"]).command,
            Some(Command::Login(rebon_plugin_onboarding::cli::LoginArgs {
                account: None,
                status: true,
            }))
        );
        assert!(Cli::try_parse_from(["rebon", "login", "copilot", "--status"]).is_err());
        assert_eq!(
            Cli::parse_from(["rebon", "logout", "openai"]).command,
            Some(Command::Logout(rebon_plugin_onboarding::cli::LogoutArgs {
                account: Some("openai".to_string()),
            }))
        );
    }

    #[test]
    fn an_unknown_install_strategy_is_rejected_at_parse_time() {
        // Better here than three ssh round trips later.
        let err = Cli::try_parse_from(["rebon", "remote", "add", "p", "h", "--install", "rsync"])
            .unwrap_err();
        assert!(
            err.to_string().contains("fetch, push, npm, system"),
            "{err}"
        );
    }

    #[test]
    fn remote_flags_parse_and_remote_path_requires_a_host() {
        let cli = Cli::parse_from(["rebon", "--remote", "prod", "--remote-path", "/srv/app"]);
        assert_eq!(cli.remote.as_deref(), Some("prod"));
        assert_eq!(cli.remote_path.as_deref(), Some("/srv/app"));

        // A remote path with no remote names a directory on a machine
        // nobody chose.
        assert!(Cli::try_parse_from(["rebon", "--remote-path", "/srv/app"]).is_err());
    }

    /// Hosting is opt-in. `startup_local` is what the startup fast path,
    /// the deferred handoff and the resume router each read to decide
    /// where the engine runs, so a bare `rebon` has to resolve it true —
    /// if this ever reads false again, every session is a mirror and the
    /// mirror's unfinished halves are back (an appended message consumed
    /// a turn late, an Esc answering `AskUserQuestion`, `ExitPlanMode`
    /// unconfirmed, tool results and rows never committing).
    #[test]
    fn a_session_is_hosted_only_when_asked_for() {
        let overrides = |args: &[&str]| {
            runtime_overrides_from_cli(&Cli::parse_from(args), Vec::new(), Vec::new(), None)
                .unwrap()
        };

        let bare = overrides(&["rebon"]);
        assert!(bare.startup_local, "a bare `rebon` runs in this process");
        assert!(!bare.startup_hosted);

        let local = overrides(&["rebon", "--local"]);
        assert!(local.startup_local, "`--local` still says so");
        assert!(!local.startup_hosted);

        let hosted = overrides(&["rebon", "--hosted"]);
        assert!(
            !hosted.startup_local,
            "`--hosted` is the way to a worker, and still reaches one"
        );
        assert!(hosted.startup_hosted);

        // The two halves of the choice cannot both be asked for.
        assert!(Cli::try_parse_from(["rebon", "--hosted", "--local"]).is_err());
    }

    #[test]
    fn hidden_background_worker_parses() {
        let cli = Cli::parse_from(["rebon", "__background-worker", "--job-id", "bg-1"]);
        assert_eq!(
            cli.command,
            Some(Command::BackgroundWorker {
                job_id: "bg-1".to_string()
            })
        );
    }

    #[test]
    fn hidden_acp_fs_bridge_parses_and_stays_hidden() {
        let cli = Cli::parse_from(["rebon", "__acp-fs-mcp"]);
        assert_eq!(cli.command, Some(Command::AcpFsMcp));
        // The argv the host injects must match what clap accepts.
        assert_eq!(rebon_acp_client::FS_BRIDGE_SUBCOMMAND, "__acp-fs-mcp");
    }

    #[test]
    fn mcp_serve_parses_its_flags_and_logs_off_stdout() {
        use rebon_mcp_channel::cli::{McpCommand, ServeArgs};
        let serve = |args: &[&str]| {
            let mut argv = vec!["rebon", "mcp", "serve"];
            argv.extend_from_slice(args);
            Cli::parse_from(argv).command.expect("a subcommand")
        };
        assert_eq!(
            serve(&[]),
            Command::Mcp {
                command: McpCommand::Serve(ServeArgs {
                    no_channel: false,
                    probe: false,
                })
            }
        );
        assert_eq!(
            serve(&["--no-channel", "--probe"]),
            Command::Mcp {
                command: McpCommand::Serve(ServeArgs {
                    no_channel: true,
                    probe: true,
                })
            }
        );
        // stdout is the MCP connection: this route must trace to stderr.
        assert!(!command_uses_tui_tracing(&serve(&[])));
        assert!(
            Cli::try_parse_from(["rebon", "mcp"]).is_err(),
            "serve is spelled out"
        );
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("mcp"), "a public command: {help}");
    }

    #[test]
    fn rc_parses_with_the_global_runtime_flags_and_logs_off_the_tui() {
        use rebon_rc_runner::cli::{RcCommand, ServeArgs};
        let cli = Cli::parse_from([
            "rebon",
            "--permission-mode",
            "plan",
            "rc",
            "serve",
            "--project",
            "app",
        ]);
        assert_eq!(cli.permission_mode, Some(PermissionMode::Plan));
        let command = cli.command.expect("a subcommand");
        assert_eq!(
            command,
            Command::Rc {
                command: RcCommand::Serve(ServeArgs {
                    projects: vec!["app".into()],
                    max_sessions: 8,
                    machine_name: None,
                })
            }
        );
        assert!(!command_uses_tui_tracing(&command));
        assert!(Cli::try_parse_from(["rebon", "rc"]).is_err());
        // `rebon remote` is the ssh command; `rc` does not take its place.
        assert!(Cli::try_parse_from(["rebon", "remote", "list"]).is_ok());
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("rc"), "a public command: {help}");
    }

    #[test]
    fn a_bad_channel_entry_fails_the_route_that_takes_it() {
        let cli = Cli::parse_from(["rebon", "--channels", "server:ok", "plugin:missing-market"]);
        let error = cli_channel_entries(&cli).expect_err("the second entry is malformed");
        assert!(
            error.to_string().contains("plugin:missing-market"),
            "{error}"
        );
        let cli = Cli::parse_from([
            "rebon",
            "--channels",
            "server:a",
            "--dangerously-load-development-channels",
            "plugin:b@market",
        ]);
        let (channels, development) = cli_channel_entries(&cli).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(development.len(), 1);
    }

    #[test]
    fn hidden_browser_mcp_forwards_its_arguments_verbatim() {
        // The shell does not interpret them: `rebon-browser-mcp` owns the flag
        // definitions now, so anything typed here has to arrive unchanged.
        let cli = Cli::parse_from([
            "rebon",
            "browser-mcp",
            "--extension-dir",
            "C:/plugins/rebon-browser/extension",
            "--port",
            "19001",
        ]);
        assert_eq!(
            cli.command,
            Some(Command::BrowserMcp {
                args: vec![
                    OsString::from("--extension-dir"),
                    OsString::from("C:/plugins/rebon-browser/extension"),
                    OsString::from("--port"),
                    OsString::from("19001"),
                ],
            })
        );
        assert_eq!(
            classify_cli_startup(&cli).unwrap(),
            StartupRoute::HeadlessCommand { tui_tracing: false }
        );
    }

    #[test]
    fn hidden_background_supervisor_parses() {
        let cli = Cli::parse_from(["rebon", "__background-supervisor"]);
        assert_eq!(cli.command, Some(Command::BackgroundSupervisor));
    }

    #[test]
    fn plugin_subcommands_parse_with_scope() {
        let cli = Cli::parse_from(["rebon", "plugin", "install", "rust-lsp"]);
        assert_eq!(
            cli.command,
            Some(Command::Plugin {
                command: PluginCommand::Install {
                    source: "rust-lsp".to_string(),
                    scope: plugin::PluginScope::User,
                    sha256: None,
                }
            })
        );

        let cli = Cli::parse_from([
            "rebon",
            "plugin",
            "install",
            "demo-1.0.0.tgz",
            "--sha256",
            "ab".repeat(32).as_str(),
        ]);
        assert_eq!(
            cli.command,
            Some(Command::Plugin {
                command: PluginCommand::Install {
                    source: "demo-1.0.0.tgz".to_string(),
                    scope: plugin::PluginScope::User,
                    sha256: Some("ab".repeat(32)),
                }
            })
        );

        let cli = Cli::parse_from(["rebon", "plugin", "verify", "--scope", "project"]);
        assert_eq!(
            cli.command,
            Some(Command::Plugin {
                command: PluginCommand::Verify {
                    name: None,
                    scope: Some(plugin::PluginScope::Project),
                }
            })
        );

        let cli = Cli::parse_from([
            "rebon", "plugin", "disable", "rust-lsp", "--scope", "project",
        ]);
        assert_eq!(
            cli.command,
            Some(Command::Plugin {
                command: PluginCommand::Disable {
                    name: "rust-lsp".to_string(),
                    scope: plugin::PluginScope::Project,
                }
            })
        );
    }

    #[test]
    fn hidden_lsp_mcp_forwards_its_arguments_verbatim() {
        let cli = Cli::parse_from(["rebon", "lsp-mcp", "rust"]);
        assert_eq!(
            cli.command,
            Some(Command::LspMcp {
                args: vec![OsString::from("rust")],
            })
        );
        assert_eq!(
            classify_cli_startup(&cli).unwrap(),
            StartupRoute::HeadlessCommand { tui_tracing: false }
        );

        let daemon = Cli::parse_from(["rebon", "lsp-mcp", "rust", "--daemon"]);
        assert_eq!(
            daemon.command,
            Some(Command::LspMcp {
                args: vec![OsString::from("rust"), OsString::from("--daemon")],
            })
        );
    }

    #[test]
    fn computer_use_forwards_its_arguments_verbatim() {
        let cli = Cli::parse_from(["rebon", "computer-use", "observe", "--at", "120,340"]);
        assert_eq!(
            cli.command,
            Some(Command::ComputerUse {
                args: vec![
                    OsString::from("observe"),
                    OsString::from("--at"),
                    OsString::from("120,340"),
                ],
            })
        );
        assert_eq!(
            classify_cli_startup(&cli).unwrap(),
            StartupRoute::HeadlessCommand { tui_tracing: false }
        );
    }

    #[test]
    fn acp_port_accepts_default_host_when_acp_is_set() {
        let cli = Cli::parse_from(["rebon", "--acp", "--acp-port", "0"]);
        assert!(cli.acp);
        assert_eq!(cli.acp_port, Some(0));
        assert_eq!(cli.acp_host, None);
    }
}
