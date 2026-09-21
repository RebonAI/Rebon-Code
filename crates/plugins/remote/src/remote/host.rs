//! A remote Rebon host, as the user configured it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::remote::ssh::SshOptions;
use crate::remote::target::{SshTarget, TargetError};

/// Where the remote server binary comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InstallStrategy {
    /// The remote downloads its own platform package from the npm
    /// registry. The default: it needs nothing from this machine, and
    /// it works when local and remote are different architectures.
    #[default]
    Fetch,
    /// This machine streams a package to the remote over the ssh
    /// connection. For servers with no outbound internet.
    Push,
    /// `npm install -g @rebon/cli` on the remote.
    Npm,
    /// Nothing is installed; `rebon` is expected on the remote `PATH`.
    System,
}

impl InstallStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Push => "push",
            Self::Npm => "npm",
            Self::System => "system",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "fetch" | "download" => Some(Self::Fetch),
            "push" | "upload" => Some(Self::Push),
            "npm" => Some(Self::Npm),
            "system" | "path" => Some(Self::System),
            _ => None,
        }
    }
}

/// How the ACP stream reaches the remote agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// ssh's own stdin/stdout carry the protocol. One process, no
    /// listening socket on the far end, nothing to firewall.
    #[default]
    Stdio,
    /// The remote listens on a loopback TCP port and ssh forwards it.
    /// For clients that need a socket rather than a pipe.
    Forward,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Forward => "forward",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "stdio" | "pipe" => Some(Self::Stdio),
            "forward" | "tcp" | "port" => Some(Self::Forward),
            _ => None,
        }
    }
}

/// One configured remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteHost {
    /// What the user types: `rebon --remote <name>`.
    pub name: String,
    /// ssh destination host — may be an `~/.ssh/config` alias, and is
    /// never resolved locally.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Default project directory on the remote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Where server builds are installed. Omitted means
    /// `$HOME/.rebon/server`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_config: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_host: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_args: Vec<String>,
    #[serde(default)]
    pub install: InstallStrategy,
    #[serde(default)]
    pub transport: Transport,
    /// Send this machine's resolved provider credentials to the remote
    /// process environment for the life of the connection.
    ///
    /// Off by default. On, the key never touches the server's disk —
    /// but it does reach the server's memory, and anyone who can read
    /// `/proc/<pid>/environ` there can read it. That trade is the
    /// user's to make, so it is a flag and not a default.
    #[serde(default)]
    pub forward_credentials: bool,
    /// Extra environment for the remote process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Platform recorded by the last successful probe. A cache, not a
    /// promise — re-probed whenever an install runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Server version recorded by the last successful install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    #[error("a remote needs a name")]
    EmptyName,
    #[error("`{0}` is not a usable remote name — use letters, digits, `-`, `_`, and `.`")]
    BadName(String),
    #[error(transparent)]
    Target(#[from] TargetError),
}

/// Names double as ACP agent ids and as control-socket key material,
/// so the character set is restricted rather than sanitised silently.
fn validate_name(name: &str) -> Result<(), HostError> {
    if name.trim().is_empty() {
        return Err(HostError::EmptyName);
    }
    if name.trim() != name
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(HostError::BadName(name.to_string()));
    }
    Ok(())
}

impl RemoteHost {
    /// Build a host from a name and a target string.
    pub fn from_target(name: &str, target: &str) -> Result<Self, HostError> {
        validate_name(name)?;
        let target = SshTarget::parse(target)?;
        Ok(Self {
            name: name.to_string(),
            host: target.host,
            user: target.user,
            port: target.port,
            path: target.path,
            server_dir: None,
            identity_file: None,
            ssh_config: None,
            jump_host: None,
            ssh_args: Vec::new(),
            install: InstallStrategy::default(),
            transport: Transport::default(),
            forward_credentials: false,
            env: BTreeMap::new(),
            platform: None,
            installed_version: None,
        })
    }

    pub fn validate(&self) -> Result<(), HostError> {
        validate_name(&self.name)?;
        if self.host.trim().is_empty() {
            return Err(HostError::Target(TargetError::MissingHost(
                self.name.clone(),
            )));
        }
        Ok(())
    }

    pub fn target(&self) -> SshTarget {
        SshTarget {
            user: self.user.clone(),
            host: self.host.clone(),
            port: self.port,
            path: self.path.clone(),
        }
    }

    /// The ssh knobs this host implies, before any per-command
    /// overrides (interactive mode, forwarding).
    pub fn ssh_options(&self) -> SshOptions {
        SshOptions {
            identity_file: self.identity_file.clone(),
            config_file: self.ssh_config.clone(),
            jump_host: self.jump_host.clone(),
            extra_args: self.ssh_args.clone(),
            ..SshOptions::default()
        }
    }

    /// Root directory for installed server builds, as a shell word.
    ///
    /// The default is written as `"$HOME"/.rebon/server` rather than
    /// `~/.rebon/server` on purpose: the path is going through
    /// [`crate::remote::shquote::sh_quote`], which would make a literal `~`
    /// out of a tilde, and a directory called `~` in the user's cwd is
    /// a classic way to lose a file.
    pub fn server_root_expr(&self) -> String {
        match self.server_dir.as_deref().map(str::trim) {
            Some(dir) if !dir.is_empty() => crate::remote::shquote::sh_quote(dir),
            _ => "\"$HOME\"/.rebon/server".to_string(),
        }
    }

    /// Absolute remote path of the server executable for a version, as
    /// a shell word.
    pub fn server_binary_expr(&self, version: &str, binary_name: &str) -> String {
        format!(
            "{}/{}/{}",
            self.server_root_expr(),
            crate::remote::shquote::sh_quote(version),
            crate::remote::shquote::sh_quote(binary_name)
        )
    }

    /// One-line summary for `rebon remote list`.
    pub fn summary(&self) -> String {
        let mut out = self.target().to_string();
        if let Some(path) = &self.path {
            if !out.ends_with(path) {
                out.push(' ');
                out.push_str(path);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_string_fills_in_user_port_and_path() {
        let host = RemoteHost::from_target("prod", "deploy@build.example:2222/srv/app").unwrap();
        assert_eq!(host.user.as_deref(), Some("deploy"));
        assert_eq!(host.host, "build.example");
        assert_eq!(host.port, Some(2222));
        assert_eq!(host.path.as_deref(), Some("/srv/app"));

        let url =
            RemoteHost::from_target("prod", "ssh://deploy@build.example:2222/srv/app").unwrap();
        assert_eq!(url.port, Some(2222));
        assert_eq!(url.path.as_deref(), Some("/srv/app"));
    }

    #[test]
    fn names_that_would_break_an_agent_id_are_rejected() {
        for bad in ["", " ", "has space", "slash/name", "colon:name", " leading"] {
            assert!(
                RemoteHost::from_target(bad, "host").is_err(),
                "expected `{bad}` to be rejected"
            );
        }
        for good in ["prod", "prod-web", "prod_web", "web.1", "A1"] {
            assert!(
                RemoteHost::from_target(good, "host").is_ok(),
                "expected `{good}` to be accepted"
            );
        }
    }

    #[test]
    fn the_default_server_root_expands_home_on_the_remote() {
        let host = RemoteHost::from_target("prod", "host").unwrap();
        // Not a literal tilde: it must survive sh quoting and still
        // expand on the far end.
        assert_eq!(host.server_root_expr(), "\"$HOME\"/.rebon/server");
        assert!(!host.server_root_expr().contains('~'));
    }

    #[test]
    fn a_configured_server_dir_is_quoted() {
        let mut host = RemoteHost::from_target("prod", "host").unwrap();
        host.server_dir = Some("/opt/rebon servers".into());
        assert_eq!(host.server_root_expr(), "'/opt/rebon servers'");
    }

    #[test]
    fn an_empty_server_dir_falls_back_to_the_default() {
        let mut host = RemoteHost::from_target("prod", "host").unwrap();
        host.server_dir = Some("   ".into());
        assert_eq!(host.server_root_expr(), "\"$HOME\"/.rebon/server");
    }

    #[test]
    fn the_binary_path_is_versioned_so_upgrades_do_not_overwrite_a_running_server() {
        let host = RemoteHost::from_target("prod", "host").unwrap();
        assert_eq!(
            host.server_binary_expr("0.15.0", "rebon"),
            "\"$HOME\"/.rebon/server/0.15.0/rebon"
        );
    }

    #[test]
    fn strategy_and_transport_parse_their_aliases() {
        assert_eq!(
            InstallStrategy::parse("FETCH"),
            Some(InstallStrategy::Fetch)
        );
        assert_eq!(
            InstallStrategy::parse("upload"),
            Some(InstallStrategy::Push)
        );
        assert_eq!(
            InstallStrategy::parse("path"),
            Some(InstallStrategy::System)
        );
        assert_eq!(InstallStrategy::parse("nope"), None);
        assert_eq!(Transport::parse("tcp"), Some(Transport::Forward));
        assert_eq!(Transport::parse("pipe"), Some(Transport::Stdio));
        assert_eq!(Transport::parse("nope"), None);
    }

    #[test]
    fn defaults_are_the_safe_ones() {
        let host = RemoteHost::from_target("prod", "host").unwrap();
        assert_eq!(host.install, InstallStrategy::Fetch);
        assert_eq!(host.transport, Transport::Stdio);
        // Credentials leaving this machine is never the default.
        assert!(!host.forward_credentials);
    }

    #[test]
    fn a_host_round_trips_through_json_without_noise() {
        let host = RemoteHost::from_target("prod", "deploy@host").unwrap();
        let json = serde_json::to_string(&host).unwrap();
        // Optional fields that are unset must not be written back out,
        // or every `remote add` rewrites the file with nulls.
        assert!(!json.contains("null"), "{json}");
        assert!(!json.contains("sshArgs"), "{json}");
        let back: RemoteHost = serde_json::from_str(&json).unwrap();
        assert_eq!(back, host);
    }
}
