//! Building the `ssh` command line.
//!
//! Everything here is a pure function from configuration to argv, so
//! the interesting decisions — which `-o` options are safe defaults,
//! when multiplexing is available, how a remote command is quoted —
//! are testable without a server to connect to.
//!
//! Two of those decisions are load-bearing enough to state up front:
//!
//! - **`BatchMode=yes` by default.** A prompt from ssh on a connection
//!   whose stdio *is* the ACP framing would be written into the
//!   protocol stream, and the client would see a parse error instead
//!   of "this host wants a passphrase". Interactive auth is opt-in and
//!   only offered on paths that own a terminal (`rebon remote install`,
//!   `rebon remote doctor`), never on the session transport.
//! - **Multiplexing is Unix-only.** Win32 OpenSSH does not implement
//!   `ControlMaster`; passing it there makes ssh fail outright rather
//!   than degrade. So the control socket is offered where it works and
//!   silently skipped where it does not, which is why
//!   [`SshOptions::multiplex`] defaults off on Windows.

use std::path::{Path, PathBuf};

use crate::remote::shquote::sh_join;
use crate::remote::target::SshTarget;

/// How long a multiplexed master connection lingers after the last
/// client leaves. Long enough that `probe → install → connect` reuses
/// one TCP handshake and one auth, short enough that a laptop lid does
/// not leave a socket open all afternoon.
const CONTROL_PERSIST_SECS: u32 = 300;

/// Server keepalive. Four missed 15-second probes drops the session,
/// which surfaces a dead network as a closed ACP connection in about a
/// minute instead of hanging until TCP gives up.
const ALIVE_INTERVAL_SECS: u32 = 15;
const ALIVE_COUNT_MAX: u32 = 4;

/// Knobs that are not part of the destination itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshOptions {
    /// The ssh executable. Overridable so a user can point at a
    /// specific OpenSSH build.
    pub program: String,
    pub identity_file: Option<String>,
    /// `-F` — an alternate `ssh_config`.
    pub config_file: Option<String>,
    /// `-J` — jump host / bastion.
    pub jump_host: Option<String>,
    /// Verbatim extra arguments, inserted before the destination.
    pub extra_args: Vec<String>,
    /// `-o ControlMaster=auto` and friends. See the module note: this
    /// is off on Windows because ssh there rejects it.
    pub multiplex: bool,
    /// Directory the control socket lives in. Ignored when
    /// [`Self::multiplex`] is false.
    pub control_dir: Option<PathBuf>,
    /// `-o BatchMode=yes`. Off only for commands that own a terminal
    /// and can let ssh prompt.
    pub batch_mode: bool,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self {
            program: "ssh".to_string(),
            identity_file: None,
            config_file: None,
            jump_host: None,
            extra_args: Vec::new(),
            multiplex: cfg!(unix),
            control_dir: None,
            batch_mode: true,
        }
    }
}

impl SshOptions {
    /// Let ssh talk to the user: passphrase prompts, host-key
    /// confirmation, keyboard-interactive auth.
    ///
    /// Only correct where Rebon is not also using stdio for a
    /// protocol.
    pub fn interactive(mut self) -> Self {
        self.batch_mode = false;
        self
    }

    pub fn with_control_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.control_dir = Some(dir.into());
        self
    }
}

/// What ssh should do once connected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshCommand {
    /// Run a command through the remote login shell.
    Shell(String),
    /// No remote command: hold the connection open for forwarding.
    /// Implies `-N`.
    None,
}

/// A local↔remote port forward (`-L`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortForward {
    pub local_port: u16,
    pub remote_port: u16,
}

/// Build the argv for an ssh invocation.
///
/// The result never goes through a local shell — it is handed to
/// `Command::new(argv[0]).args(&argv[1..])` — so only the *remote*
/// command string needs quoting, and that has already happened by the
/// time it arrives here.
pub fn ssh_argv(
    target: &SshTarget,
    options: &SshOptions,
    command: &SshCommand,
    forward: Option<PortForward>,
) -> Vec<String> {
    let mut argv = vec![options.program.clone()];

    // No pseudo-terminal. With one, the remote tty would translate
    // `\n` to `\r\n` in the ACP stream and echo everything Rebon
    // writes back at it.
    argv.push("-T".to_string());

    if options.batch_mode {
        argv.push("-o".to_string());
        argv.push("BatchMode=yes".to_string());
    }

    argv.push("-o".to_string());
    argv.push(format!("ServerAliveInterval={ALIVE_INTERVAL_SECS}"));
    argv.push("-o".to_string());
    argv.push(format!("ServerAliveCountMax={ALIVE_COUNT_MAX}"));

    if let Some(port) = target.port {
        argv.push("-p".to_string());
        argv.push(port.to_string());
    }
    if let Some(identity) = &options.identity_file {
        argv.push("-i".to_string());
        argv.push(identity.clone());
    }
    if let Some(config) = &options.config_file {
        argv.push("-F".to_string());
        argv.push(config.clone());
    }
    if let Some(jump) = &options.jump_host {
        argv.push("-J".to_string());
        argv.push(jump.clone());
    }

    if options.multiplex {
        if let Some(dir) = &options.control_dir {
            argv.push("-o".to_string());
            argv.push("ControlMaster=auto".to_string());
            argv.push("-o".to_string());
            argv.push(format!(
                "ControlPath={}",
                control_socket_path(dir, target).display()
            ));
            argv.push("-o".to_string());
            argv.push(format!("ControlPersist={CONTROL_PERSIST_SECS}"));
        }
    }

    if let Some(forward) = forward {
        // Without this ssh reports the forward failure on stderr and
        // keeps running, leaving a client to connect to a port nobody
        // is listening on and time out with no explanation.
        argv.push("-o".to_string());
        argv.push("ExitOnForwardFailure=yes".to_string());
        argv.push("-L".to_string());
        argv.push(format!(
            "127.0.0.1:{}:127.0.0.1:{}",
            forward.local_port, forward.remote_port
        ));
    }

    argv.extend(options.extra_args.iter().cloned());

    if matches!(command, SshCommand::None) {
        argv.push("-N".to_string());
    }

    argv.push(target.destination());

    if let SshCommand::Shell(line) = command {
        // `--` first: a remote command that starts with a dash would
        // otherwise be read as more ssh options.
        argv.push("--".to_string());
        argv.push(line.clone());
    }

    argv
}

/// Where the multiplexing control socket for a target lives.
///
/// Unix domain sockets have a hard path limit (104–108 bytes), and
/// ssh fails the whole connection when it is exceeded. So the target
/// contributes a short fixed-width hash rather than its name — a
/// bastion alias plus a long config directory would otherwise blow the
/// budget on exactly the setups that most want multiplexing.
pub fn control_socket_path(dir: &Path, target: &SshTarget) -> PathBuf {
    dir.join(format!("{}.sock", short_hash(&target.identity())))
}

/// FNV-1a, truncated to 8 hex digits.
///
/// Not a security boundary — a collision would share a control socket
/// between two hosts, so the input is the full identity and the width
/// is generous enough that it does not happen in a config file sized
/// for humans.
fn short_hash(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (hash ^ (hash >> 32)) as u32)
}

/// Wrap a remote command so it runs under an explicit `sh`, with the
/// remote working directory already set.
///
/// `ssh host -- cmd` runs `cmd` through the *login* shell, which may
/// be fish, csh, or something that does not understand the scripts
/// this crate generates. Naming `/bin/sh` removes the guesswork.
pub fn sh_command(script: &str) -> SshCommand {
    SshCommand::Shell(sh_join(["/bin/sh", "-c", script]))
}

/// A remote command line that first changes into `cwd`.
pub fn sh_command_in(cwd: &str, script: &str) -> SshCommand {
    let script = format!("cd {} && {script}", crate::remote::shquote::sh_quote(cwd));
    sh_command(&script)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(raw: &str) -> SshTarget {
        SshTarget::parse(raw).expect("target")
    }

    fn opts() -> SshOptions {
        SshOptions {
            multiplex: false,
            ..SshOptions::default()
        }
    }

    #[test]
    fn a_minimal_invocation_disables_the_pty_and_prompts() {
        let argv = ssh_argv(
            &target("host"),
            &opts(),
            &SshCommand::Shell("echo hi".into()),
            None,
        );
        assert_eq!(argv[0], "ssh");
        assert!(argv.contains(&"-T".to_string()), "{argv:?}");
        assert!(
            argv.windows(2).any(|w| w == ["-o", "BatchMode=yes"]),
            "{argv:?}"
        );
        // The destination comes before `--`, and the command after it.
        let dash = argv.iter().position(|a| a == "--").expect("--");
        assert_eq!(argv[dash - 1], "host");
        assert_eq!(argv[dash + 1], "echo hi");
    }

    #[test]
    fn a_remote_command_starting_with_a_dash_cannot_be_read_as_an_option() {
        let argv = ssh_argv(
            &target("host"),
            &opts(),
            &SshCommand::Shell("--acp".into()),
            None,
        );
        let dash = argv.iter().position(|a| a == "--").expect("--");
        assert_eq!(argv[dash + 1], "--acp");
    }

    #[test]
    fn the_port_goes_to_dash_p_not_onto_the_destination() {
        // `ssh host:2222` is not a thing; it would be read as a
        // hostname containing a colon.
        let argv = ssh_argv(&target("me@host:2222"), &opts(), &SshCommand::None, None);
        assert!(argv.windows(2).any(|w| w == ["-p", "2222"]), "{argv:?}");
        assert!(argv.contains(&"me@host".to_string()), "{argv:?}");
        assert!(!argv.iter().any(|a| a.contains("host:2222")), "{argv:?}");
    }

    #[test]
    fn no_command_means_dash_n() {
        let argv = ssh_argv(&target("host"), &opts(), &SshCommand::None, None);
        assert!(argv.contains(&"-N".to_string()), "{argv:?}");
        assert!(!argv.contains(&"--".to_string()), "{argv:?}");
    }

    #[test]
    fn keepalive_is_always_set() {
        // A remote session with no traffic for minutes at a time is
        // the normal case while the model is thinking; without probes
        // a NAT box silently drops it.
        let argv = ssh_argv(&target("host"), &opts(), &SshCommand::None, None);
        assert!(
            argv.windows(2)
                .any(|w| w == ["-o", "ServerAliveInterval=15"]),
            "{argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["-o", "ServerAliveCountMax=4"]),
            "{argv:?}"
        );
    }

    #[test]
    fn interactive_mode_drops_batch_mode_only() {
        let argv = ssh_argv(
            &target("host"),
            &opts().interactive(),
            &SshCommand::None,
            None,
        );
        assert!(!argv.iter().any(|a| a == "BatchMode=yes"), "{argv:?}");
        assert!(argv.contains(&"-T".to_string()), "{argv:?}");
    }

    #[test]
    fn identity_config_and_jump_host_are_forwarded() {
        let options = SshOptions {
            identity_file: Some("/home/me/.ssh/id_ed25519".into()),
            config_file: Some("/home/me/.ssh/work_config".into()),
            jump_host: Some("bastion.example".into()),
            extra_args: vec!["-o".into(), "StrictHostKeyChecking=accept-new".into()],
            ..opts()
        };
        let argv = ssh_argv(&target("host"), &options, &SshCommand::None, None);
        assert!(
            argv.windows(2)
                .any(|w| w == ["-i", "/home/me/.ssh/id_ed25519"]),
            "{argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["-F", "/home/me/.ssh/work_config"]),
            "{argv:?}"
        );
        assert!(
            argv.windows(2).any(|w| w == ["-J", "bastion.example"]),
            "{argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["-o", "StrictHostKeyChecking=accept-new"]),
            "{argv:?}"
        );
    }

    #[test]
    fn extra_args_land_before_the_destination() {
        // ssh stops reading options at the destination, so anything
        // appended after it would be treated as the remote command.
        let options = SshOptions {
            extra_args: vec!["-vvv".into()],
            ..opts()
        };
        let argv = ssh_argv(&target("host"), &options, &SshCommand::None, None);
        let flag = argv.iter().position(|a| a == "-vvv").expect("-vvv");
        let dest = argv.iter().position(|a| a == "host").expect("host");
        assert!(flag < dest, "{argv:?}");
    }

    #[test]
    fn multiplexing_is_opt_in_and_needs_a_directory() {
        // Without a control dir there is nowhere to put the socket, so
        // the options are skipped rather than emitted half-formed.
        let bare = SshOptions {
            multiplex: true,
            control_dir: None,
            ..opts()
        };
        let argv = ssh_argv(&target("host"), &bare, &SshCommand::None, None);
        assert!(
            !argv.iter().any(|a| a.starts_with("ControlPath=")),
            "{argv:?}"
        );

        let wired = SshOptions {
            multiplex: true,
            control_dir: Some(PathBuf::from("/tmp/rebon-ssh")),
            ..opts()
        };
        let argv = ssh_argv(&target("host"), &wired, &SshCommand::None, None);
        assert!(argv.iter().any(|a| a == "ControlMaster=auto"), "{argv:?}");
        assert!(argv.iter().any(|a| a == "ControlPersist=300"), "{argv:?}");
        let expected = format!(
            "ControlPath={}",
            control_socket_path(Path::new("/tmp/rebon-ssh"), &target("host")).display()
        );
        assert!(argv.contains(&expected), "{argv:?}");
    }

    #[test]
    fn control_sockets_are_short_and_per_target() {
        let dir = Path::new("/tmp/rebon-ssh");
        let a = control_socket_path(dir, &target("me@host"));
        let b = control_socket_path(dir, &target("me@host:2222"));
        let c = control_socket_path(dir, &target("me@host"));
        // Same host on two ports is two servers.
        assert_ne!(a, b);
        assert_eq!(a, c);
        // The unix socket path budget is ~104 bytes; the file name
        // must not be what eats it.
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), "deadbeef.sock".len(), "{name}");
    }

    #[test]
    fn a_forward_exits_rather_than_silently_not_listening() {
        let argv = ssh_argv(
            &target("host"),
            &opts(),
            &SshCommand::None,
            Some(PortForward {
                local_port: 7788,
                remote_port: 41000,
            }),
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["-o", "ExitOnForwardFailure=yes"]),
            "{argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["-L", "127.0.0.1:7788:127.0.0.1:41000"]),
            "{argv:?}"
        );
    }

    #[test]
    fn a_forward_binds_loopback_only() {
        // Binding `*` would expose someone else's remote agent to the
        // local network.
        let argv = ssh_argv(
            &target("host"),
            &opts(),
            &SshCommand::None,
            Some(PortForward {
                local_port: 1,
                remote_port: 2,
            }),
        );
        let spec = argv
            .iter()
            .find(|a| a.contains(":1:"))
            .expect("forward spec");
        assert!(spec.starts_with("127.0.0.1:"), "{spec}");
    }

    #[test]
    fn sh_command_names_the_shell_instead_of_trusting_the_login_shell() {
        let SshCommand::Shell(line) = sh_command("echo hi") else {
            panic!("expected a shell command");
        };
        assert_eq!(line, "/bin/sh -c 'echo hi'");
    }

    #[test]
    fn sh_command_in_quotes_a_hostile_project_path() {
        let SshCommand::Shell(line) = sh_command_in("/srv/a b'; rm -rf ~", "rebon --acp") else {
            panic!("expected a shell command");
        };
        // One `cd` argument, and the injected text stays inside it.
        assert!(line.contains(r#"'\''"#), "{line}");
        assert!(!line.contains("&& rm -rf"), "{line}");
    }
}
