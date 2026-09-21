//! # `remote` — running Rebon against a project on another machine
//!
//! The transport half of the `remote` plugin: everything about reaching
//! the far end, and nothing about what Rebon does once it is there.
//! The module tree below is that transport, unchanged.
//!
//! The shape is the one VS Code's Remote-SSH uses: a server build is
//! installed on the far end, the agent runs *there* — its shell, its
//! filesystem, its git checkout — and this machine keeps only the UI
//! and the transcript.
//!
//! ## Why there is no new protocol here
//!
//! Rebon already speaks both halves of this. `rebon --acp` is an ACP
//! JSON-RPC server on stdio, and `rebon-acp-client` already knows how
//! to start an agent CLI, hand it a session, stream its updates, and
//! route its permission prompts back into the local UI. A remote host
//! is therefore an agent CLI whose command happens to begin with
//! `ssh`. This crate's job is everything around that command:
//!
//! - [`target`] / [`host`] — what the user configured.
//! - [`store`] — where it is kept (`~/.rebon/remotes.json`).
//! - [`ssh`] — the argv, including the two defaults that matter:
//!   `BatchMode=yes` (an ssh prompt would corrupt the ACP stream) and
//!   Unix-only connection multiplexing.
//! - [`platform`] / [`script`] — what the far end is, and the shell
//!   scripts that probe and install it.
//! - [`exec`] — running those scripts.
//! - [`connect`] — the finished launch description the CLI turns into
//!   an `AgentCommand`.
//!
//! ## What this leg does not carry
//!
//! An ACP backend reports no token usage and does not route its writes
//! through the host's filesystem, so on a remote session `/rewind` has
//! nothing local to restore and `/context` has no usage to show. That
//! is not a gap to paper over: the remote Rebon keeps its own file
//! history and its own transcript, and the capability bits are
//! reported honestly so the UI can gate rather than offer a no-op.
//! The local session still gets every turn written into its own
//! transcript through the journal, so `--resume` and history search
//! see the conversation.
//!
//! ## Credentials
//!
//! By default the remote uses its own `~/.rebon/config.json`, exactly
//! as if someone had run `rebon` there over ssh. A host may instead
//! set `forwardCredentials`, which inlines the resolved provider key
//! into the remote process's environment for the life of the
//! connection — never touching the server's disk, but readable by
//! anyone who can read that process's environment. It is a per-host
//! opt-in, not a default.

pub mod connect;
pub mod exec;
pub mod host;
pub mod platform;
pub mod script;
pub mod shquote;
pub mod ssh;
pub mod store;
pub mod target;

pub use connect::{launch, remote_command, LaunchPlan, RemoteLaunch};
pub use exec::{
    control_dir, install, probe, run_script, uninstall, ExecError, InstallOptions, InstallReport,
    SshRun, DEFAULT_REGISTRY,
};
pub use host::{HostError, InstallStrategy, RemoteHost, Transport};
pub use platform::{PlatformError, RemotePlatform};
pub use script::{InstallOutcome, PackageSource, ProbeReport};
pub use shquote::{sh_join, sh_quote};
pub use ssh::{control_socket_path, ssh_argv, PortForward, SshCommand, SshOptions};
pub use store::{remotes_json_path, RemoteStore, StoreError, STORE_VERSION};
pub use target::{SshTarget, TargetError};

/// The agent id a remote host is exposed under.
///
/// Prefixed so a remote can never collide with an agent declared in
/// `acpAgents` or by a plugin — `/agent claude-code` and
/// `/agent remote:prod` are unambiguous, and the prefix is what tells
/// the session-agent layer to skip the local filesystem wiring.
pub const REMOTE_AGENT_PREFIX: &str = "remote:";

/// Build the agent id for a remote host name.
pub fn agent_id(name: &str) -> String {
    format!("{REMOTE_AGENT_PREFIX}{name}")
}

/// The host name behind an agent id, if it is a remote's.
pub fn host_name_from_agent_id(agent_id: &str) -> Option<&str> {
    agent_id.strip_prefix(REMOTE_AGENT_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_ids_round_trip() {
        assert_eq!(agent_id("prod"), "remote:prod");
        assert_eq!(host_name_from_agent_id("remote:prod"), Some("prod"));
        assert_eq!(host_name_from_agent_id("claude-code"), None);
    }

    #[test]
    fn a_remote_id_cannot_collide_with_a_declared_agent() {
        // Agent names reject `:` (see `host::validate_name`), so the
        // prefix is not forgeable from the other surface.
        assert!(RemoteHost::from_target("remote:prod", "host").is_err());
    }
}
