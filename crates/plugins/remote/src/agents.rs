//! The wiring that turns a configured host into a session backend.
//!
//! The interesting decision is what a remote *is* to the rest of
//! Rebon: an entry in the same [`DeclaredAgent`] list that `acpAgents`
//! and `runtime: acp` definitions feed. That is what makes `/agent`,
//! the agent list, session sidecars, and the transcript journal work
//! on a remote without any of them learning what ssh is. Two fields
//! separate it from a local agent, and both are consequences of the
//! filesystem being somewhere else:
//!
//! - `workspace_cwd` carries the *remote* project path, because the
//!   session's own cwd names a directory on this machine.
//! - `inject_fs_tools` is off. The injection spawns `rebon
//!   __acp-fs-mcp` as a child of the agent — which is on the far side
//!   of the ssh connection, where it could not reach this host's fs
//!   service. The remote keeps its own file history instead.
//!
//! [`RemoteAgents`] is how the list leaves this crate: a front end asks
//! `rebon_agent_core::declared_source` and never names this plugin, so
//! turning the plugin off is the whole of "no remotes in this session".

use anyhow::Context;

use crate::remote::{RemoteHost, RemotePlatform, RemoteStore};

use rebon_agent_core::declared_source::DeclaredAgentSource;
use rebon_agent_core::routing::{AgentOrigin, DeclaredAgent};

/// The server build a remote should run: this build's own version.
///
/// Pinned rather than "whatever is newest on the far end" so the two
/// sides of an ACP conversation are always the same build. A version
/// skew here shows up as a capability the client uses and the server
/// does not have.
///
/// This crate and `rebon-cli` both take `version.workspace = true`, so
/// "this plugin's version" and "the binary's version" are one number.
/// That is an assumption made here and checkable only there, so
/// `rebon-cli`'s
/// `tui::wiring::tests::the_remote_server_version_is_this_binarys_own`
/// is what fails if the two ever come apart.
pub fn server_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub(crate) fn config_dir() -> std::path::PathBuf {
    rebon_config::config_home_dir()
}

/// Read the configured remotes.
pub fn load_store() -> anyhow::Result<RemoteStore> {
    RemoteStore::load(&config_dir()).context("reading remotes.json")
}

/// Look one up, or fail with the list of what exists.
pub fn resolve(name: &str) -> anyhow::Result<RemoteHost> {
    Ok(load_store()?.require(name)?.clone())
}

/// Provider API keys `forwardCredentials` carries to the remote.
///
/// A fixed list rather than "every variable that looks like a key":
/// the remote process's environment is readable by anyone who can read
/// `/proc/<pid>/environ` there, so what leaves this machine is
/// enumerated, not inferred. Mirrors the environment providers
/// `rebon-config` already recognises.
const FORWARDED_CREDENTIAL_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "DEEPSEEK_API_KEY",
    "ZHIPUAI_API_KEY",
    "MOONSHOT_API_KEY",
    "MINIMAX_API_KEY",
];

/// The credentials to inline into the remote command, if the host asked
/// for it.
///
/// Only variables actually set here are sent; an unset one is skipped
/// rather than forwarded empty, because an empty key on the far end
/// fails as "unauthorized" instead of "no credential configured".
///
/// `lookup` is a parameter rather than a direct `std::env::var` so the
/// rules above can be tested without a test mutating the process
/// environment that every other test in the binary shares.
fn forwarded_credentials_with(
    host: &RemoteHost,
    lookup: impl Fn(&str) -> Option<String>,
) -> std::collections::BTreeMap<String, String> {
    if !host.forward_credentials {
        return std::collections::BTreeMap::new();
    }
    FORWARDED_CREDENTIAL_VARS
        .iter()
        .filter_map(|name| {
            let value = lookup(name)?;
            (!value.trim().is_empty()).then(|| ((*name).to_string(), value))
        })
        .collect()
}

fn forwarded_credentials(host: &RemoteHost) -> std::collections::BTreeMap<String, String> {
    forwarded_credentials_with(host, |name| std::env::var(name).ok())
}

/// Build the agent declaration for a remote host.
///
/// `project_path` overrides the host's configured directory, which is
/// what `--remote-path` does.
pub fn declared_agent(host: &RemoteHost, project_path: Option<&str>) -> DeclaredAgent {
    let version = server_version();
    let platform = host
        .platform
        .as_deref()
        .and_then(parse_platform)
        .unwrap_or(RemotePlatform::LinuxX64);
    let mut plan = crate::remote::LaunchPlan::new(version, platform.binary_name());
    plan.project_path = project_path;
    plan.control_dir = crate::remote::control_dir(&config_dir());
    plan.remote_env = forwarded_credentials(host);
    let launch = crate::remote::launch(host, &plan);

    DeclaredAgent {
        id: crate::remote::agent_id(&host.name),
        label: format!("{} (remote)", host.name),
        command: launch.program,
        args: launch.args,
        env: launch.env.into_iter().collect(),
        // The ssh process itself runs from the local session's
        // directory; only the far end's cwd matters, and that travels
        // in `workspace_cwd`.
        cwd: None,
        origin: AgentOrigin::Remote,
        inject_fs_tools: false,
        session_meta: None,
        install_hint: launch.install_hint,
        // An empty string is meaningful, not a missing value: the ACP
        // server resolves it to the remote process's own directory,
        // which is where the ssh command landed. `None` here would
        // instead send this machine's cwd.
        workspace_cwd: Some(
            project_path
                .or(host.path.as_deref())
                .unwrap_or_default()
                .to_string(),
        ),
    }
}

/// Declarations for every configured remote.
///
/// `selected` / `path` carry `--remote` / `--remote-path`: the path
/// override applies to the host the user actually opened and to no
/// other, so `/agent remote:staging` later in the same session still
/// lands in staging's own directory.
///
/// A store that cannot be read is a warning, not a failed session:
/// remotes are additive, and refusing to open a local session because
/// `remotes.json` has a typo in it would be a poor trade. `--remote`
/// itself still fails loudly — the switch it asks for cannot find an
/// agent that was never declared.
pub fn declared_agents_for(selected: Option<&str>, path: Option<&str>) -> Vec<DeclaredAgent> {
    let store = match load_store() {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!(error = %err, "rebon: ignoring an unusable remotes.json");
            return Vec::new();
        }
    };
    let selected = selected.map(|name| name.trim().to_ascii_lowercase());
    store
        .hosts
        .iter()
        .map(|host| {
            let is_selected = selected
                .as_deref()
                .is_some_and(|name| host.name.to_ascii_lowercase() == name);
            declared_agent(host, if is_selected { path } else { None })
        })
        .collect()
}

fn parse_platform(raw: &str) -> Option<RemotePlatform> {
    let (os, cpu) = raw.split_once('-')?;
    match (os, cpu) {
        ("linux", "x64") => Some(RemotePlatform::LinuxX64),
        ("linux", "arm64") => Some(RemotePlatform::LinuxArm64),
        ("darwin", "x64") => Some(RemotePlatform::DarwinX64),
        ("darwin", "arm64") => Some(RemotePlatform::DarwinArm64),
        ("win32", "x64") => Some(RemotePlatform::Win32X64),
        _ => None,
    }
}

/// This plugin's answer on the `declared-agent-source` seam.
///
/// A unit struct rather than a handle to anything: the store is read on
/// every call, because a `rebon remote add` in another terminal should
/// show up in the next session without this one being restarted.
pub struct RemoteAgents;

impl DeclaredAgentSource for RemoteAgents {
    fn declared_agents(
        &self,
        demanded: Option<&str>,
        workspace_path: Option<&str>,
    ) -> Vec<DeclaredAgent> {
        declared_agents_for(demanded, workspace_path)
    }

    fn agent_id_for(&self, demanded: &str) -> String {
        crate::remote::agent_id(demanded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> RemoteHost {
        RemoteHost::from_target("prod", "deploy@build.example").unwrap()
    }

    #[test]
    fn a_remote_declares_itself_as_an_ssh_command() {
        let agent = declared_agent(&host(), None);
        assert_eq!(agent.command, "ssh");
        assert_eq!(agent.id, "remote:prod");
        assert_eq!(agent.origin, AgentOrigin::Remote);
        assert!(
            agent.args.last().unwrap().contains("--acp"),
            "{:?}",
            agent.args
        );
    }

    #[test]
    fn a_remote_never_gets_the_injected_fs_tools() {
        // They would be spawned on the far side of the ssh connection,
        // where they cannot reach this host's fs service.
        assert!(!declared_agent(&host(), None).inject_fs_tools);
    }

    #[test]
    fn the_remote_project_path_travels_as_the_workspace_cwd() {
        let mut host = host();
        host.path = Some("/srv/app".into());
        let agent = declared_agent(&host, None);
        assert_eq!(agent.workspace_cwd.as_deref(), Some("/srv/app"));
        // And the ssh process is left on the local session's cwd.
        assert_eq!(agent.cwd, None);
    }

    #[test]
    fn an_explicit_path_overrides_the_hosts_default() {
        let mut host = host();
        host.path = Some("/srv/app".into());
        let agent = declared_agent(&host, Some("/srv/other"));
        assert_eq!(agent.workspace_cwd.as_deref(), Some("/srv/other"));
        assert!(agent.args.last().unwrap().contains("/srv/other"));
    }

    #[test]
    fn no_configured_path_sends_an_empty_cwd_not_the_local_one() {
        // Empty means "the directory the remote process is already in"
        // to the ACP server. `None` would make the backend fall back
        // to this machine's cwd, which is a path the remote does not
        // have.
        let agent = declared_agent(&host(), None);
        assert_eq!(agent.workspace_cwd.as_deref(), Some(""));
    }

    #[test]
    fn a_recorded_platform_picks_the_right_executable_name() {
        let mut host = host();
        host.platform = Some("win32-x64".into());
        let agent = declared_agent(&host, None);
        assert!(
            agent.args.last().unwrap().contains("rebon.exe"),
            "{:?}",
            agent.args
        );
    }

    #[test]
    fn an_unprobed_or_unreadable_platform_falls_back_to_the_common_one() {
        let mut host = host();
        host.platform = Some("plan9-mips".into());
        let agent = declared_agent(&host, None);
        assert!(agent.args.last().unwrap().contains("/rebon --acp"));
        assert_eq!(parse_platform("plan9-mips"), None);
        assert_eq!(
            parse_platform("linux-arm64"),
            Some(RemotePlatform::LinuxArm64)
        );
    }

    #[test]
    fn the_spawn_hint_points_at_the_command_that_fixes_it() {
        let agent = declared_agent(&host(), None);
        let hint = agent.install_hint.expect("hint");
        assert!(hint.contains("rebon remote install prod"), "{hint}");
    }

    /// Every variable set, so a test only has to say what it expects
    /// to be filtered out.
    fn every_var(name: &str) -> Option<String> {
        Some(format!("value-of-{name}"))
    }

    #[test]
    fn credentials_stay_home_unless_the_host_opted_in() {
        let mut host = host();
        // Even with keys available, an opted-out host sends nothing.
        assert!(forwarded_credentials_with(&host, every_var).is_empty());

        host.forward_credentials = true;
        let forwarded = forwarded_credentials_with(&host, every_var);
        assert_eq!(
            forwarded.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("value-of-ANTHROPIC_API_KEY")
        );
    }

    #[test]
    fn an_unset_or_blank_credential_is_skipped_rather_than_sent_empty() {
        // An empty key on the far end fails as "unauthorized", which
        // reads as a wrong key rather than a missing one.
        let mut host = host();
        host.forward_credentials = true;
        let forwarded = forwarded_credentials_with(&host, |name| match name {
            "MINIMAX_API_KEY" => Some("   ".to_string()),
            "MOONSHOT_API_KEY" => None,
            other => Some(format!("value-of-{other}")),
        });
        assert!(!forwarded.contains_key("MINIMAX_API_KEY"), "{forwarded:?}");
        assert!(!forwarded.contains_key("MOONSHOT_API_KEY"), "{forwarded:?}");
        assert!(forwarded.contains_key("OPENAI_API_KEY"), "{forwarded:?}");
    }

    #[test]
    fn the_forwarded_set_is_enumerated_not_inferred() {
        // Whatever leaves this machine is a fixed list; a variable
        // that merely looks like a key is not swept up.
        let mut host = host();
        host.forward_credentials = true;
        let forwarded = forwarded_credentials_with(&host, every_var);
        assert!(
            !forwarded.contains_key("SOME_OTHER_API_KEY"),
            "{forwarded:?}"
        );
        assert_eq!(forwarded.len(), FORWARDED_CREDENTIAL_VARS.len());
    }

    #[test]
    fn forwarded_credentials_reach_the_remote_command_inline() {
        // Inline `NAME=value exec …` rather than ssh's SendEnv, which
        // needs a matching AcceptEnv the server almost never has.
        let mut host = host();
        host.forward_credentials = true;
        let plan_env = forwarded_credentials_with(&host, |name| {
            (name == "OPENAI_API_KEY").then(|| "sk-forwarded".to_string())
        });
        let mut plan = crate::remote::LaunchPlan::new(server_version(), "rebon");
        plan.remote_env = plan_env;
        let line = crate::remote::remote_command(&host, &plan);
        assert!(line.contains("OPENAI_API_KEY=sk-forwarded exec "), "{line}");
    }

    #[test]
    fn the_pinned_server_version_is_this_builds_own() {
        assert_eq!(server_version(), env!("CARGO_PKG_VERSION"));
        let agent = declared_agent(&host(), None);
        assert!(
            agent.args.last().unwrap().contains(server_version()),
            "{:?}",
            agent.args
        );
    }

    /// The seam hands out the same ids the rest of the plugin spells, so
    /// `--remote prod` and a later `/agent remote:prod` name one agent.
    #[test]
    fn the_seam_names_a_host_the_way_the_prefix_helper_does() {
        assert_eq!(
            RemoteAgents.agent_id_for("prod"),
            crate::remote::agent_id("prod")
        );
        assert_eq!(RemoteAgents.agent_id_for("prod"), "remote:prod");
    }

    /// Naming is not existence: a host nobody configured still resolves to
    /// an id, and the switch that follows is what reports it missing. If
    /// this answered `None` for unknown hosts, `--remote typo` would be
    /// indistinguishable from `--remote` with the plugin turned off.
    #[test]
    fn the_seam_names_a_host_that_was_never_configured() {
        assert_eq!(
            RemoteAgents.agent_id_for("never-configured"),
            "remote:never-configured"
        );
    }

    /// The declarations the seam hands over are the ones in the store, with
    /// the path override applied to the demanded host and to no other.
    #[test]
    fn the_seam_declares_the_configured_hosts() {
        let home = crate::test_home::TestConfigHome::new();
        let mut store = RemoteStore::default();
        store
            .insert(
                RemoteHost::from_target("prod", "deploy@build").unwrap(),
                false,
            )
            .unwrap();
        let mut staging = RemoteHost::from_target("staging", "deploy@stage").unwrap();
        staging.path = Some("/srv/staging".into());
        store.insert(staging, false).unwrap();
        store.save(home.path()).unwrap();

        let declared = RemoteAgents.declared_agents(Some("prod"), Some("/srv/opened"));
        let ids: Vec<&str> = declared.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["remote:prod", "remote:staging"]);
        assert_eq!(declared[0].workspace_cwd.as_deref(), Some("/srv/opened"));
        assert_eq!(declared[1].workspace_cwd.as_deref(), Some("/srv/staging"));
    }

    /// A config home with no `remotes.json` declares nothing rather than
    /// failing: not having configured a remote is not an error.
    #[test]
    fn a_config_home_without_a_store_declares_nothing() {
        let _home = crate::test_home::TestConfigHome::new();
        assert!(RemoteAgents.declared_agents(None, None).is_empty());
    }
}
