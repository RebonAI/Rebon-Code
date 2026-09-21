//! Actually running ssh.
//!
//! Synchronous and thread-based rather than async, because every
//! caller is a one-shot CLI command that has nothing else to do while
//! it waits. The session transport does not come through here at all —
//! that one is spawned by `rebon-acp-client`, which owns the child for
//! the life of the connection.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::remote::host::{InstallStrategy, RemoteHost};
use crate::remote::platform::RemotePlatform;
use crate::remote::script::{
    self, InstallOutcome, PackageSource, ProbeReport, INSTALL_OK, INSTALL_SKIPPED,
};
use crate::remote::ssh::{sh_command, ssh_argv, SshOptions};

/// Default npm registry the fetch strategy pulls from.
pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

/// The result of one remote command.
#[derive(Debug, Clone)]
pub struct SshRun {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl SshRun {
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }

    /// stderr trimmed to something worth putting in an error message.
    ///
    /// ssh is chatty on failure and the useful line is usually the
    /// last one, not the first — "Permission denied (publickey)" comes
    /// after any banner the host prints.
    pub fn failure_detail(&self) -> String {
        let tail: Vec<&str> = self
            .stderr
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .rev()
            .take(4)
            .collect();
        if tail.is_empty() {
            match self.status {
                Some(code) => format!("ssh exited with status {code}"),
                None => "ssh was terminated by a signal".to_string(),
            }
        } else {
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("could not run `{program}`: {source}{}", hint(.source))]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{context} failed on `{host}`:\n{detail}")]
    Remote {
        context: String,
        host: String,
        detail: String,
    },
    #[error("could not read {path}: {source}")]
    ReadPackage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Probe(#[from] script::ProbeParseError),
    #[error(transparent)]
    Platform(#[from] crate::remote::platform::PlatformError),
    #[error("`{host}` runs Windows; Rebon's remote install needs a POSIX shell there. Install the server by hand and use `--install system`.")]
    NoPosixShell { host: String },
    /// A push install had no package to send.
    ///
    /// Push sends a *package*, not the running executable: the install
    /// script unpacks `package/bin/`, which carries ripgrep beside the
    /// binary, and a bare executable would leave `Grep` broken on the
    /// far end. So `--package` is required whatever this machine's
    /// platform is — the platform note is added only when it is also
    /// wrong, because that is the mistake worth naming twice.
    #[error("a push install to `{host}` needs a package to send: pass `--package <path to a {remote} @rebon/cli tarball>`, or use `--install fetch` to let the host download its own.{}", platform_note(*remote, *local))]
    PushNeedsPackage {
        host: String,
        remote: RemotePlatform,
        local: Option<RemotePlatform>,
    },
}

/// Extra sentence for a push whose local platform could not have
/// produced a usable package anyway.
fn platform_note(remote: RemotePlatform, local: Option<RemotePlatform>) -> String {
    match local {
        Some(local) if local == remote => String::new(),
        Some(local) => format!(
            " Note that this machine is {local} and `{remote}` is what the host needs, so a package built here will not run there."
        ),
        None => format!(
            " Note that this machine is not one of the platforms Rebon publishes, so the {remote} package has to come from elsewhere."
        ),
    }
}

fn hint(source: &std::io::Error) -> String {
    if source.kind() == std::io::ErrorKind::NotFound {
        " (is OpenSSH installed and on PATH?)".to_string()
    } else {
        String::new()
    }
}

/// Directory for ssh control sockets, created on demand.
///
/// Returns `None` when multiplexing is unavailable or the directory
/// cannot be made — a missing control socket costs a handshake, so it
/// degrades rather than fails.
pub fn control_dir(config_dir: &Path) -> Option<PathBuf> {
    if !cfg!(unix) {
        // Win32 OpenSSH has no ControlMaster; see `ssh::SshOptions`.
        return None;
    }
    let dir = config_dir.join("ssh");
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::debug!(dir = %dir.display(), error = %err, "rebon: no ssh control directory; multiplexing off");
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The socket grants whoever can open it the ability to run
        // commands on the remote as the connected user.
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(dir)
}

/// Run a script on the remote and collect its output.
///
/// `stdin_file`, when given, is streamed to the remote command — that
/// is how the push install gets its bytes across.
pub fn run_script(
    host: &RemoteHost,
    options: &SshOptions,
    script: &str,
    stdin_file: Option<&Path>,
) -> Result<SshRun, ExecError> {
    let argv = ssh_argv(&host.target(), options, &sh_command(script), None);
    let (program, args) = argv.split_first().expect("ssh_argv yields a program");

    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin_file.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| ExecError::Spawn {
            program: program.clone(),
            source,
        })?;

    // Drain both pipes on their own threads. A remote that writes more
    // than a pipe buffer's worth of stderr — npm does, easily — would
    // otherwise block forever while this side is blocked writing
    // stdin, and neither would ever finish.
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    if let Some(path) = stdin_file {
        let bytes = std::fs::read(path).map_err(|source| ExecError::ReadPackage {
            path: path.to_path_buf(),
            source,
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            // A write error here means the remote closed early —
            // its own stderr says why, so the error is not raised
            // over the top of it.
            if let Err(err) = stdin.write_all(&bytes) {
                tracing::debug!(error = %err, "rebon: remote closed stdin during upload");
            }
        }
    }
    // Dropping stdin is what tells `cat` on the far end that the
    // transfer is over. Without it the remote waits for EOF forever.
    drop(child.stdin.take());

    let status = child.wait().map_err(|source| ExecError::Spawn {
        program: program.clone(),
        source,
    })?;
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    Ok(SshRun {
        status: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Ask the remote what it is and what it has.
pub fn probe(
    host: &RemoteHost,
    options: &SshOptions,
    version: &str,
) -> Result<ProbeReport, ExecError> {
    // The binary name is a chicken-and-egg problem: it depends on the
    // platform, which is what the probe is for. `rebon` is right on
    // the four Unix platforms, and a Windows remote fails the POSIX
    // shell check before the answer matters.
    let script = script::probe_script(host, version, "rebon");
    let run = run_script(host, options, &script, None)?;
    if !run.ok() && run.stdout.is_empty() {
        return Err(ExecError::Remote {
            context: "connecting".to_string(),
            host: host.name.clone(),
            detail: run.failure_detail(),
        });
    }
    Ok(script::parse_probe(&run.stdout)?)
}

/// What an install decided to do.
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub outcome: InstallOutcome,
    pub platform: RemotePlatform,
    pub version: String,
    pub strategy: InstallStrategy,
    /// Where the package came from, for the summary line.
    pub source: String,
}

/// Options for [`install`].
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub force: bool,
    /// Overrides the package for a push install. Required when the
    /// remote's platform differs from this machine's.
    pub package: Option<PathBuf>,
    pub registry: Option<String>,
}

/// Put a server build on the remote.
pub fn install(
    host: &RemoteHost,
    options: &SshOptions,
    version: &str,
    report: &ProbeReport,
    install_options: &InstallOptions,
) -> Result<InstallReport, ExecError> {
    let platform = report.platform()?;
    if !platform.has_posix_shell() && host.install != InstallStrategy::System {
        return Err(ExecError::NoPosixShell {
            host: host.name.clone(),
        });
    }
    let registry = install_options
        .registry
        .as_deref()
        .unwrap_or(DEFAULT_REGISTRY);

    let (script, stdin_file, source) = match host.install {
        InstallStrategy::System => {
            // Nothing to install; just confirm the promise holds.
            return if report.path_rebon.is_some() {
                Ok(InstallReport {
                    outcome: InstallOutcome::AlreadyPresent,
                    platform,
                    version: report
                        .server_version
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                    strategy: host.install,
                    source: "remote PATH".to_string(),
                })
            } else {
                Err(ExecError::Remote {
                    context: "checking for rebon on the remote PATH".to_string(),
                    host: host.name.clone(),
                    detail: "`rebon` is not on the remote PATH. Install it there, or switch this remote to `--install fetch`.".to_string(),
                })
            };
        }
        InstallStrategy::Npm => (
            script::npm_install_script(version),
            None,
            format!("npm ({registry})"),
        ),
        InstallStrategy::Fetch => {
            let url = platform.tarball_url(registry, version);
            let script = script::install_script(
                host,
                version,
                platform.binary_name(),
                &PackageSource::Url(url.clone()),
                install_options.force,
            );
            (script, None, url)
        }
        InstallStrategy::Push => {
            let package = match &install_options.package {
                Some(path) => path.clone(),
                None => {
                    return Err(ExecError::PushNeedsPackage {
                        host: host.name.clone(),
                        remote: platform,
                        local: RemotePlatform::local(),
                    });
                }
            };
            let script = script::install_script(
                host,
                version,
                platform.binary_name(),
                &PackageSource::Stdin,
                install_options.force,
            );
            let display = package.display().to_string();
            (script, Some(package), display)
        }
    };

    let run = run_script(host, options, &script, stdin_file.as_deref())?;
    let outcome = script::parse_install_outcome(&run.stdout);
    if !run.ok() || outcome == InstallOutcome::Unknown {
        return Err(ExecError::Remote {
            context: format!("installing the {version} server"),
            host: host.name.clone(),
            detail: run.failure_detail(),
        });
    }

    Ok(InstallReport {
        outcome,
        platform,
        version: version.to_string(),
        strategy: host.install,
        source,
    })
}

/// Remove installed server builds.
pub fn uninstall(
    host: &RemoteHost,
    options: &SshOptions,
    version: Option<&str>,
) -> Result<(), ExecError> {
    let script = script::uninstall_script(host, version);
    let run = run_script(host, options, &script, None)?;
    if !run.ok() || !run.stdout.contains(INSTALL_OK) {
        return Err(ExecError::Remote {
            context: "removing the server build".to_string(),
            host: host.name.clone(),
            detail: run.failure_detail(),
        });
    }
    Ok(())
}

/// Marker strings re-exported so the CLI can talk about outcomes
/// without depending on the script module directly.
pub const OK_MARKER: &str = INSTALL_OK;
pub const SKIPPED_MARKER: &str = INSTALL_SKIPPED;

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: Option<i32>, stdout: &str, stderr: &str) -> SshRun {
        SshRun {
            status,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn failure_detail_keeps_the_last_lines_in_order() {
        // ssh prints the banner first and the reason last.
        let run = run(
            Some(255),
            "",
            "Welcome!\n\n\nPermission denied (publickey).\n",
        );
        assert_eq!(
            run.failure_detail(),
            "Welcome!\nPermission denied (publickey)."
        );
        assert!(!run.ok());
    }

    #[test]
    fn failure_detail_falls_back_to_the_exit_code() {
        assert_eq!(
            run(Some(3), "", "  \n").failure_detail(),
            "ssh exited with status 3"
        );
        assert_eq!(
            run(None, "", "").failure_detail(),
            "ssh was terminated by a signal"
        );
    }

    #[test]
    fn failure_detail_is_bounded() {
        let noisy = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let detail = run(Some(1), "", &noisy).failure_detail();
        assert_eq!(detail.lines().count(), 4, "{detail}");
        assert!(detail.ends_with("line 99"), "{detail}");
    }

    #[test]
    fn zero_status_is_the_only_success() {
        assert!(run(Some(0), "", "").ok());
        assert!(!run(Some(1), "", "").ok());
        assert!(!run(None, "", "").ok());
    }

    #[test]
    fn a_missing_ssh_binary_says_so() {
        let err = ExecError::Spawn {
            program: "ssh".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        assert!(err.to_string().contains("is OpenSSH installed"), "{err}");
    }

    #[test]
    fn a_push_without_a_package_asks_for_one_and_offers_the_alternative() {
        // Same platform on both sides: a push still needs a package,
        // because it sends a tarball and not the running executable.
        let err = ExecError::PushNeedsPackage {
            host: "prod".into(),
            remote: RemotePlatform::LinuxX64,
            local: Some(RemotePlatform::LinuxX64),
        };
        let text = err.to_string();
        assert!(text.contains("--package"), "{text}");
        assert!(text.contains("--install fetch"), "{text}");
        // And it must not claim a mismatch that does not exist.
        assert!(!text.contains("will not run there"), "{text}");
    }

    #[test]
    fn a_push_across_platforms_says_so_as_well() {
        let err = ExecError::PushNeedsPackage {
            host: "prod".into(),
            remote: RemotePlatform::LinuxArm64,
            local: Some(RemotePlatform::Win32X64),
        };
        let text = err.to_string();
        assert!(text.contains("linux-arm64"), "{text}");
        assert!(text.contains("win32-x64"), "{text}");
        assert!(text.contains("will not run there"), "{text}");
    }

    #[test]
    fn a_push_from_an_unpublished_platform_says_the_package_must_come_from_elsewhere() {
        let err = ExecError::PushNeedsPackage {
            host: "prod".into(),
            remote: RemotePlatform::LinuxArm64,
            local: None,
        };
        assert!(
            err.to_string().contains("has to come from elsewhere"),
            "{err}"
        );
    }

    #[test]
    fn a_windows_remote_is_refused_before_a_half_install_happens() {
        let err = ExecError::NoPosixShell {
            host: "winbox".into(),
        };
        assert!(err.to_string().contains("--install system"), "{err}");
    }

    #[test]
    fn control_dir_is_absent_on_windows_where_ssh_cannot_use_it() {
        let dir = std::env::temp_dir().join(format!("rebon-ctl-{}", std::process::id()));
        let resolved = control_dir(&dir);
        if cfg!(unix) {
            assert!(resolved.is_some());
            assert!(resolved.unwrap().ends_with("ssh"));
        } else {
            assert!(resolved.is_none());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
