//! CLI/TUI-only runtime overrides. Kept in the binary because it references
//! `crate::ui_config::UiMode` (binary-internal); the rest of the former
//! `rebon_config` module moved to the `rebon-config` library crate.

/// Command-line-driven overrides applied on top of the on-disk
/// `.rebon/config.json`.
///
/// Populated from the `--provider` / `--model` flags in
/// `crate::main`. Both fields are optional:
///
/// * `provider = Some("openrouter")` — ignore the stored
///   `activeCustomProvider` and resolve the entry named `openrouter`
///   from `customProviders[]` instead. Errors out if no matching
///   entry exists, same as when `activeCustomProvider` itself is
///   stale.
/// * `model = Some("gpt-4o-mini")` — after the provider is
///   resolved, replace its `model` field with this value. Lets
///   the user sanity-check a single turn against a specific model
///   without touching disk.
#[derive(Debug, Clone, Default)]
pub struct RuntimeOverride {
    /// Name of a `customProviders[]` entry to use instead of the
    /// stored `activeCustomProvider`.
    pub provider: Option<String>,
    /// Model name to pass to the executor instead of the
    /// provider's `model` field.
    pub model: Option<String>,
    /// Per-session override for OpenAI priority service tier. `None` falls
    /// back to the saved global fast-mode preference.
    pub fast_mode: Option<bool>,
    /// Session id to resume. When set, the TUI loads the on-disk
    /// transcript for this session instead of creating a fresh one.
    pub resume: Option<String>,
    /// Explicit working directory for session construction. This is used by
    /// background attach/resume paths to avoid process-wide cwd mutation.
    pub cwd: Option<String>,
    /// Normal per-session MCP channel entries from `--channels`.
    /// These never carry the dev allowlist bypass.
    pub channels: Vec<rebon_plugin_mcp::runtime::ChannelEntry>,
    /// Development channel entries from
    /// `--dangerously-load-development-channels`. TUI startup must
    /// confirm these before appending them with `dev: true`; ACP must
    /// not silently enable them.
    pub development_channels: Vec<rebon_plugin_mcp::runtime::ChannelEntry>,
    /// Settings override payloads or file paths supplied with `--settings`.
    pub settings: Vec<String>,
    /// Additional directories granted to the session with `--add-dir`.
    pub add_dirs: Vec<String>,
    /// Local plugin directories supplied with `--plugin-dir`.
    pub plugin_dirs: Vec<String>,
    /// MCP config payloads or file paths supplied with `--mcp-config`.
    pub mcp_configs: Vec<String>,
    /// When true, only `--mcp-config` MCP servers are loaded.
    pub strict_mcp_config: bool,
    /// Startup-only interactive UI mode override from `--ui-mode`.
    pub ui_mode: Option<crate::ui_config::UiMode>,
    /// Startup effort/thinking override from `--effort`.
    pub effort_level: Option<rebon_types::ReasoningEffort>,
    /// Startup permission mode override from `--permission-mode`.
    pub permission_mode: Option<rebon_permissions::PermissionMode>,
    /// Whether this runtime owns the persistent supervisor session of an Agent
    /// Queue. Internal background-worker context; not a CLI flag.
    pub queue_session: bool,
    /// Open the Agent View instead of starting on the prompt surface.
    pub startup_agent_view: bool,
    /// Hand this session to a background worker as soon as it is open,
    /// from `--hosted`. Never inherited by a worker: a worker that tried
    /// to host itself would hand its session to a second worker.
    pub startup_hosted: bool,
    /// Host the session in this process, from `--local`: take the session
    /// lock here, publish no endpoint, start no worker. The escape hatch
    /// from the hosted default. Never inherited by a
    /// worker, which is by definition not local.
    pub startup_local: bool,
    /// Optional cwd scope for startup Agent View job listing.
    pub startup_agent_view_cwd_scope: Option<String>,
    /// Non-persistent startup notices injected into the TUI transcript.
    pub startup_notices: Vec<String>,
    /// Background job id whose session is being opened in the foreground.
    /// Used only by TUI attach paths so a later `/background` can return
    /// to the same Agent View row instead of adopting the session as a new job.
    pub attached_background_job_id: Option<String>,
    /// Remote host this session runs on, from `--remote`.
    ///
    /// Unlike the configured default agent, this is a demand: a
    /// session that cannot reach the named remote fails to open
    /// rather than quietly starting on the local engine, because
    /// "local" is the one thing the user ruled out by asking.
    pub remote: Option<String>,
    /// Project directory on that remote, from `--remote-path`. A path
    /// on the remote's filesystem, never this machine's.
    pub remote_path: Option<String>,
}
