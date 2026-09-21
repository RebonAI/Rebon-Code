//! What a session was launched with, kept apart from what it is doing now.
//!
//! Everything here is read off the run's own arguments (or the background job
//! record that replays them) once, while the session is being built, and is
//! never rewritten by a turn. Keeping it in one place is what lets a function
//! that only needs the launch arguments take [`SessionStartupParams`] instead
//! of the whole session handle.
//!
//! The field names match [`crate::rebon_config::RuntimeOverride`]'s, because
//! that is where every one of them comes from and where `/hosted` hands them
//! back (`rebon_cli::session_shell::handover`).

use rebon_permissions::types::PermissionMode;

/// The launch arguments this session keeps for the rest of its life.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub struct SessionStartupParams {
    /// Startup effort override resolved from CLI.
    pub effort_level: Option<rebon_types::ReasoningEffort>,
    /// Startup permission mode override resolved from CLI.
    pub permission_mode: Option<PermissionMode>,
    /// Whether this session is the persistent supervisor of an Agent Queue.
    pub queue_session: bool,
    /// Runtime flags that should be inherited by detached or dispatched
    /// background sessions.
    pub channels: Vec<rebon_plugin_mcp::runtime::ChannelEntry>,
    pub development_channels: Vec<rebon_plugin_mcp::runtime::ChannelEntry>,
    pub settings: Vec<String>,
    /// Extra roots `--add-dir` put on this run. Read after startup too — every
    /// `PromptRequest` carries them as additional working directories — but
    /// never rewritten, so it stays a launch argument.
    pub add_dirs: Vec<String>,
    pub plugin_dirs: Vec<String>,
    pub mcp_configs: Vec<String>,
    pub strict_mcp_config: bool,
}
