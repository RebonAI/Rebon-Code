//! # runtime — wrapping a command so the OS confines it
//!
//! This is the runtime half of the sandbox. Its sibling
//! [`crate::view`] owns the view models `/sandbox` and `/doctor`
//! render; this module owns the thing they describe — taking a command
//! the model asked to run and turning it into a command the operating
//! system will refuse to let out of its box.
//!
//! ## The one seam
//!
//! Everything reduces to one call:
//!
//! ```text
//! SandboxRuntime::wrap(&CommandRequest) -> WrappedCommand
//! ```
//!
//! [`CommandRequest`] says *what to run and under what rules*;
//! [`WrappedCommand`] is a program, an argv, and an environment
//! mutation — the four things a spawner needs and nothing else. There
//! is no second entry point for Windows, and that is deliberate: the
//! shape that made two entry points necessary elsewhere (a shell
//! string on Unix, an argv on Windows) is here folded into
//! [`BinShell`], which carries the shell *and* its leading arguments.
//! `sh -lc`, `bash -c`, `pwsh -NoProfile -NonInteractive -Command`,
//! and `pwsh … -EncodedCommand` are all the same shape, so the
//! PowerShell tool's three passing modes are three different
//! `BinShell` values rather than three code paths through the
//! sandbox.
//!
//! ## What each platform actually does
//!
//! | | mechanism | filesystem | network |
//! |---|---|---|---|
//! | Linux | `bwrap` mount namespace | bind mounts | namespace + loopback proxy |
//! | macOS | `sandbox-exec` seatbelt | profile rules | profile rules |
//! | Windows | `sandbox-win.exe` helper | deny ACEs | WFP filters on the sandbox SID |
//!
//! They are not equivalent and this crate does not pretend they are.
//! Where a platform cannot express a rule it says so through a
//! [`Warning`] (macOS cannot fake file contents) or refuses outright
//! (Windows cannot scope an ACL to one command). What it never does
//! is quietly enforce less than it was asked for.
//!
//! ## The rules that are not negotiable
//!
//! 1. **A sandbox that cannot be built is an error.** Never a
//!    passthrough. Missing dependency, unresolvable path, over-long
//!    command line — all refuse. See [`SandboxError`].
//! 2. **Strict mode verifies rather than assumes.** Before and after
//!    the wrap, [`SandboxRuntime::assert_confined`] asks the OS to
//!    actually confine a probe process. A machine where the sandbox
//!    silently does nothing must not look like one where it works.
//! 3. **A command may narrow its rules, never widen them.** The
//!    session config is the outer bound; see
//!    [`EffectiveConfig::merge`].
//! 4. **An unrestricted command is spawned unchanged.** The fast path
//!    is what makes sandboxing affordable enough to leave on.
//! 5. **Every dropped rule is a value, not a log line.** Warnings
//!    ride back on [`WrappedCommand::warnings`] so a caller can
//!    surface them and a test can assert on them.
//!
//! ## Lifecycle
//!
//! ```text
//! probe_support()          → what this machine has
//! SandboxRuntime::new()    → compile the session config
//!   per command:
//!     assert_confined()    → strict mode, before
//!     wrap()               → program + argv + env
//!     assert_confined()    → strict mode, after
//!     spawn
//! reset()                  → release session-scoped resources
//! ```

pub mod config;
pub mod confined;
pub mod env;
pub mod error;
pub mod fs_probe;
pub mod linux;
pub mod macos;
pub mod macos_monitor;
pub mod mask_redirect;
pub mod session;
pub mod settings;
pub mod support;
pub mod windows;

pub use config::{
    BinShell, CommandRequest, CredentialEnvRule, CredentialFileRule, CredentialsConfig,
    EffectiveConfig, FilesystemConfig, MaskedFileBind, MiscConfig, NetworkConfig, ReadRules,
    RuntimePaths, SessionSandboxConfig, WriteRules,
};
pub use confined::{
    assert_confined, ConfinedProbe, ConfinedVerdict, ConfinementContext, PlatformProbe, SandboxMode,
};
pub use error::{warning_code, SandboxError, Warning, SANDBOX_NOT_CONFINED_MESSAGE};
pub use fs_probe::{FsProbe, RealFs};
pub use macos_monitor::{Monitor, MonitorError, ViolationSink};
pub use session::{SandboxSetupError, SessionSandbox};
pub use settings::SandboxSettings;
pub use settings::SettingsError;
pub use support::{
    current_platform, has_backend, probe_support, PathLookup, ProgramLookup, SupportReport,
};
pub use windows::SandboxWinStatus;

use crate::view::platform::SandboxPlatform;
use std::path::PathBuf;
use std::sync::Arc;

/// Which mechanism produced a [`WrappedCommand`].
///
/// Reported back rather than inferred from the platform, because a
/// caller that wants to know "was this actually confined" must not
/// have to re-derive it — a `Passthrough` on Linux and a
/// `Bubblewrap` on Linux are the same platform and opposite answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBackend {
    /// The command had no rules to apply and was not wrapped.
    Passthrough,
    Bubblewrap,
    Seatbelt,
    SandboxWin,
}

impl SandboxBackend {
    /// Whether the OS is enforcing anything for this command.
    pub fn is_confined(&self) -> bool {
        !matches!(self, SandboxBackend::Passthrough)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxBackend::Passthrough => "passthrough",
            SandboxBackend::Bubblewrap => "bubblewrap",
            SandboxBackend::Seatbelt => "seatbelt",
            SandboxBackend::SandboxWin => "sandbox-win",
        }
    }
}

/// A command ready to spawn.
///
/// The spawner applies these four things and nothing else: run
/// `program` with `args`, set `env_set`, remove `env_unset`, and
/// `cwd` if present. Deliberately not a `std::process::Command` —
/// callers here use `tokio::process::Command`, the desktop app may
/// use something else, and a plain data value keeps this crate free
/// of an async runtime dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub env_unset: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub backend: SandboxBackend,
    /// Rules the backend could not apply. Empty is the normal case.
    pub warnings: Vec<Warning>,
}

impl WrappedCommand {
    /// The unwrapped command, for the RFC §3.3 fast path.
    fn passthrough(request: &CommandRequest) -> Self {
        let argv = request.bin_shell.argv(&request.command);
        Self {
            program: argv[0].clone(),
            args: argv[1..].to_vec(),
            env_set: Vec::new(),
            env_unset: Vec::new(),
            cwd: request.cwd.clone(),
            backend: SandboxBackend::Passthrough,
            warnings: Vec::new(),
        }
    }
}

/// How a session's sandbox was set up.
pub struct SandboxRuntimeInit {
    pub platform: SandboxPlatform,
    pub session: SessionSandboxConfig,
    pub mode: SandboxMode,
    /// Ties seatbelt deny messages back to this session. Ignored off
    /// macOS.
    pub log_tag: String,
    /// A debugger attached to the agent needs the ptrace the sandbox
    /// drops; see [`ConfinementContext::debug_session`].
    pub debug_session: bool,
    pub support: SupportReport,
    pub probe: Arc<dyn ConfinedProbe>,
    /// Anything that must stay alive for as long as the session does.
    ///
    /// The loopback proxy is the reason this exists: it is a running
    /// listener, its ports are already baked into the environment every
    /// wrapped command gets, and it has to stop when the session stops.
    /// Held as an opaque handle rather than a typed field so this crate
    /// does not have to depend on the crate that starts it — it stays a
    /// pure command-wrapping seam with no async runtime.
    pub session_resources: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

/// The session-scoped sandbox.
///
/// Immutable after construction on purpose. Every mutable thing a
/// sandbox needs — proxy ports, ACL state, socket paths — is resolved
/// into the config before the runtime exists, so a command's wrap
/// cannot be affected by anything that happened after the session
/// started. A configuration change is a new runtime, which is also
/// what the Windows ACL layer requires (RFC §6.4).
pub struct SandboxRuntime {
    platform: SandboxPlatform,
    session: SessionSandboxConfig,
    mode: SandboxMode,
    log_tag: String,
    debug_session: bool,
    support: SupportReport,
    probe: Arc<dyn ConfinedProbe>,
    /// Dropped with the runtime; see [`SandboxRuntimeInit::session_resources`].
    _session_resources: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl std::fmt::Debug for SandboxRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxRuntime")
            .field("platform", &self.platform)
            .field("mode", &self.mode)
            .field("usable", &self.support.is_usable())
            .finish()
    }
}

impl SandboxRuntime {
    /// RFC §12 — compile the session configuration.
    ///
    /// Does not fail. A machine that cannot sandbox produces a
    /// runtime whose [`SandboxRuntime::is_usable`] is false and whose
    /// every restricted wrap returns the reason; making the
    /// constructor fallible would push the same branch into every
    /// caller and lose the reason on the way.
    pub fn new(init: SandboxRuntimeInit) -> Self {
        let mut session = init.session;
        init.support.apply_to(&mut session.runtime);
        Self {
            platform: init.platform,
            session,
            mode: init.mode,
            log_tag: init.log_tag,
            debug_session: init.debug_session,
            support: init.support,
            probe: init.probe,
            _session_resources: init.session_resources,
        }
    }

    pub fn platform(&self) -> SandboxPlatform {
        self.platform
    }

    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    pub fn support(&self) -> &SupportReport {
        &self.support
    }

    pub fn session_config(&self) -> &SessionSandboxConfig {
        &self.session
    }

    /// Whether this machine can confine anything at all.
    pub fn is_usable(&self) -> bool {
        has_backend(self.platform) && self.support.is_usable()
    }

    /// RFC §9 — refuse the command unless confinement is real.
    ///
    /// `sandbox_requested` is the caller's own answer to "was this
    /// command going to be sandboxed": an excluded command, or one
    /// the user allowed to run unsandboxed, is not subject to the
    /// probe because nothing claimed it was confined.
    pub fn assert_confined(&self, sandbox_requested: bool) -> Result<(), SandboxError> {
        assert_confined(
            ConfinementContext {
                sandbox_requested,
                mode: self.mode,
                debug_session: self.debug_session,
            },
            self.probe.as_ref(),
        )
    }

    /// Wrap one command — the seam.
    pub fn wrap(&self, request: &CommandRequest) -> Result<WrappedCommand, SandboxError> {
        self.wrap_with_fs(request, &RealFs)
    }

    /// [`SandboxRuntime::wrap`] against an injected filesystem, so
    /// the Linux mount plan can be exercised anywhere.
    pub fn wrap_with_fs(
        &self,
        request: &CommandRequest,
        fs: &dyn FsProbe,
    ) -> Result<WrappedCommand, SandboxError> {
        let effective = EffectiveConfig::merge(&self.session, request);

        // RFC §3.3. Checked before anything else, including the
        // platform check: a command with no rules runs identically on
        // a machine with no sandbox, and refusing it there would make
        // an unsupported platform unusable rather than unsandboxed.
        if effective.is_unrestricted() {
            return Ok(WrappedCommand::passthrough(request));
        }

        if !has_backend(self.platform) {
            return Err(SandboxError::UnsupportedPlatform {
                platform: self.platform.as_wire(),
            });
        }
        if !self.support.is_usable() {
            return Err(SandboxError::NotInitialized {
                detail: self.support.errors.join("; "),
            });
        }

        let mut env_plan = env::build_env_plan(&effective);
        // Linux serves a mask by binding the fake file over the real one, so
        // it needs none of this. The other two degrade the mask to a denial,
        // and this turns some of those denials back into a served fake —
        // without ever removing the denial. See `mask_redirect`.
        if matches!(
            self.platform,
            SandboxPlatform::Macos | SandboxPlatform::Windows
        ) {
            env::apply_mask_redirects(&mut env_plan, &effective);
        }
        let env_plan = env_plan;

        let (program, args, warnings, backend) = match self.platform {
            SandboxPlatform::Linux => {
                let (program, args, warnings) = linux::build_bwrap_argv(&effective, &env_plan, fs)?;
                (program, args, warnings, SandboxBackend::Bubblewrap)
            }
            SandboxPlatform::Macos => {
                let (program, args, warnings) =
                    macos::build_seatbelt_argv(&effective, &self.log_tag, &env_plan)?;
                (program, args, warnings, SandboxBackend::Seatbelt)
            }
            SandboxPlatform::Windows => {
                let (program, args, warnings) =
                    windows::build_sandbox_win_argv(&effective, &env_plan)?;
                (program, args, warnings, SandboxBackend::SandboxWin)
            }
            SandboxPlatform::Unknown => {
                return Err(SandboxError::UnsupportedPlatform {
                    platform: self.platform.as_wire(),
                })
            }
        };

        let mut warnings = warnings;
        warnings.extend(self.degradation_warnings(&effective));

        // The environment reaches the child two different ways and
        // only one of them is this vector. bubblewrap takes
        // `--setenv` and `sandbox-win` takes `--env`, both already in
        // `args`; seatbelt takes neither, so the spawner has to
        // apply it. Returning it in every case keeps the spawner's
        // job identical across platforms — applying it twice is a
        // no-op, and leaving it out on macOS would silently drop the
        // proxy configuration.
        Ok(WrappedCommand {
            program,
            args,
            env_set: env_plan.set,
            env_unset: env_plan.unset,
            cwd: request.cwd.clone(),
            backend,
            warnings,
        })
    }

    /// Rules that were configured, could not be enforced as written,
    /// and turned into something else.
    ///
    /// Every entry here is a case where the sandbox is doing
    /// something *different* from what the settings asked for. RFC
    /// §11.3 requires those to be auditable rather than inferred, and
    /// the direction of the difference is not the point: a rule that
    /// silently became stricter is as misleading as one that became
    /// weaker, because the user debugs the resulting failure
    /// somewhere else entirely.
    ///
    /// Emitted once per wrapped command rather than once per session
    /// so the warning travels with the command it applied to — that
    /// is the object the caller has in hand when something fails.
    fn degradation_warnings(&self, config: &EffectiveConfig) -> Vec<Warning> {
        let mut warnings = Vec::new();
        let backend = match self.platform {
            SandboxPlatform::Linux => linux::BACKEND,
            SandboxPlatform::Macos => macos::BACKEND,
            _ => windows::BACKEND,
        };

        // Domain rules are decided by the loopback proxy on every
        // platform: Linux has no kernel domain filter, seatbelt
        // cannot express a hostname, and WFP is keyed on a SID. With
        // no proxy listening there is nothing to make the decision,
        // and every backend falls through to blocking the network
        // outright — a strictly *stronger* rule than "allow these
        // domains", and one the user did not ask for. Someone whose
        // `allowedDomains` command cannot reach the network has no
        // way to tell that from a broken network.
        if config.network_restricted && config.runtime.http_proxy_port.is_none() {
            let named: Vec<String> = config
                .network
                .allowed_domains
                .iter()
                .chain(config.network.denied_domains.iter())
                .cloned()
                .collect();
            if !named.is_empty() {
                warnings.push(Warning::new(
                    backend,
                    crate::runtime::error::warning_code::DOMAIN_RULES_WITHOUT_PROXY,
                    format!(
                        "domain rules ({}) cannot be applied because no sandbox proxy is \
                         running — the command was given no network access at all rather \
                         than access to those domains",
                        named.join(", ")
                    ),
                ));
            }
        }

        // The wrap seam produces an argv. bubblewrap's `--seccomp`
        // takes a file descriptor, not a path, so a filter cannot be
        // named in one — there is no spelling of this that works
        // until the seam can pass descriptors.
        if self.platform == SandboxPlatform::Linux {
            if let Some(filter) = &config.runtime.seccomp_config {
                warnings.push(Warning::new(
                    backend,
                    crate::runtime::error::warning_code::SECCOMP_NOT_APPLIED,
                    format!(
                        "the seccomp filter {} is configured but not applied — bubblewrap \
                         takes a file descriptor and this wrap produces an argv; syscall \
                         filtering is off",
                        filter.display()
                    ),
                ));
            }
        }

        warnings
    }

    /// RFC §12 — release session-scoped resources.
    ///
    /// Returns the paths whose ACLs the Windows helper still holds,
    /// so the caller can hand them to `sandbox-win.exe reset`. On Unix
    /// there is nothing to release: mounts and profiles live and die
    /// with the process they were applied to.
    pub fn reset(&self) -> Vec<PathBuf> {
        if self.platform != SandboxPlatform::Windows {
            return Vec::new();
        }
        self.session
            .filesystem
            .allow_write
            .iter()
            .chain(self.session.filesystem.deny_read.iter())
            .chain(self.session.filesystem.deny_write.iter())
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fs_probe::FakeFs;

    struct AlwaysConfined;
    impl ConfinedProbe for AlwaysConfined {
        fn probe(&self) -> ConfinedVerdict {
            ConfinedVerdict::confined("test")
        }
    }

    struct NeverConfined;
    impl ConfinedProbe for NeverConfined {
        fn probe(&self) -> ConfinedVerdict {
            ConfinedVerdict::unconfined("test says no")
        }
    }

    fn runtime(
        platform: SandboxPlatform,
        mutate: impl FnOnce(&mut SessionSandboxConfig),
    ) -> SandboxRuntime {
        let mut session = SessionSandboxConfig::default();
        mutate(&mut session);
        SandboxRuntime::new(SandboxRuntimeInit {
            platform,
            session,
            mode: SandboxMode::Strict,
            log_tag: "tag-1".into(),
            debug_session: false,
            support: SupportReport {
                platform,
                ..Default::default()
            },
            probe: Arc::new(AlwaysConfined),
            session_resources: None,
        })
    }

    fn request() -> CommandRequest {
        CommandRequest::new("echo hi", BinShell::posix())
    }

    fn restricted_request() -> CommandRequest {
        request().with_network_restriction(true)
    }

    #[test]
    fn an_unrestricted_command_is_returned_unchanged() {
        let wrapped = runtime(SandboxPlatform::Linux, |_| {})
            .wrap(&request())
            .unwrap();

        assert_eq!(wrapped.backend, SandboxBackend::Passthrough);
        assert!(!wrapped.backend.is_confined());
        assert_eq!(wrapped.program, "sh");
        assert_eq!(wrapped.args, vec!["-lc", "echo hi"]);
        assert!(wrapped.env_set.is_empty());
        assert!(wrapped.env_unset.is_empty());
    }

    #[test]
    fn the_fast_path_works_on_a_platform_with_no_backend() {
        let wrapped = runtime(SandboxPlatform::Unknown, |_| {})
            .wrap(&request())
            .unwrap();
        assert_eq!(wrapped.backend, SandboxBackend::Passthrough);
    }

    #[test]
    fn a_restricted_command_on_a_platform_with_no_backend_is_refused() {
        let error = runtime(SandboxPlatform::Unknown, |_| {})
            .wrap(&restricted_request())
            .unwrap_err();
        assert!(matches!(
            error,
            SandboxError::UnsupportedPlatform {
                platform: "unknown"
            }
        ));
    }

    #[test]
    fn linux_restricted_command_goes_through_bubblewrap() {
        let runtime = runtime(SandboxPlatform::Linux, |session| {
            session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
        });

        let wrapped = runtime
            .wrap_with_fs(&restricted_request(), &FakeFs::new())
            .unwrap();

        assert_eq!(wrapped.backend, SandboxBackend::Bubblewrap);
        assert!(wrapped.backend.is_confined());
        assert_eq!(wrapped.program, "/usr/bin/bwrap");
        assert!(wrapped.args.contains(&"--unshare-all".to_string()));
    }

    #[test]
    fn macos_restricted_command_goes_through_seatbelt() {
        let runtime = runtime(SandboxPlatform::Macos, |_| {});

        let wrapped = runtime.wrap(&restricted_request()).unwrap();

        assert_eq!(wrapped.backend, SandboxBackend::Seatbelt);
        assert_eq!(wrapped.program, macos::SANDBOX_EXEC);
        assert!(wrapped.args[1].contains("(deny default"));
    }

    #[test]
    fn windows_restricted_command_goes_through_sandbox_win() {
        let runtime = runtime(SandboxPlatform::Windows, |session| {
            session.runtime.sandbox_win_path = Some(PathBuf::from(r"C:\Rebon\sandbox-win.exe"));
        });

        let wrapped = runtime.wrap(&restricted_request()).unwrap();

        assert_eq!(wrapped.backend, SandboxBackend::SandboxWin);
        assert_eq!(wrapped.args[0], "exec");
        assert!(wrapped.args.contains(&"--block-network".to_string()));
    }

    #[test]
    fn an_unusable_machine_refuses_restricted_commands_with_the_reason() {
        let mut runtime = runtime(SandboxPlatform::Linux, |_| {});
        runtime.support.errors = vec!["bwrap (bubblewrap) was not found on PATH".into()];

        let error = runtime.wrap(&restricted_request()).unwrap_err();

        match error {
            SandboxError::NotInitialized { detail } => assert!(detail.contains("bwrap")),
            other => panic!("expected NotInitialized, got {other:?}"),
        }
    }

    #[test]
    fn an_unusable_machine_still_runs_unrestricted_commands() {
        let mut runtime = runtime(SandboxPlatform::Linux, |_| {});
        runtime.support.errors = vec!["bwrap (bubblewrap) was not found on PATH".into()];

        assert!(runtime.wrap(&request()).is_ok());
    }

    #[test]
    fn env_plan_is_returned_on_every_platform() {
        for platform in [
            SandboxPlatform::Linux,
            SandboxPlatform::Macos,
            SandboxPlatform::Windows,
        ] {
            let runtime = runtime(platform, |session| {
                session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
                session.runtime.sandbox_win_path = Some(PathBuf::from(r"C:\sandbox-win.exe"));
                session.runtime.http_proxy_port = Some(3128);
            });

            let wrapped = runtime
                .wrap_with_fs(&restricted_request(), &FakeFs::new())
                .unwrap();

            assert!(
                wrapped.env_set.iter().any(|(key, _)| key == "http_proxy"),
                "{platform:?} lost the proxy configuration"
            );
            assert!(
                wrapped.env_unset.contains(&"no_proxy".to_string()),
                "{platform:?} lost the proxy bypass strip"
            );
        }
    }

    #[test]
    fn cwd_rides_along_for_the_spawner() {
        let wrapped = runtime(SandboxPlatform::Linux, |session| {
            session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
        })
        .wrap_with_fs(&restricted_request().with_cwd("/work"), &FakeFs::new())
        .unwrap();

        assert_eq!(wrapped.cwd, Some(PathBuf::from("/work")));
    }

    #[test]
    fn warnings_ride_back_rather_than_failing_the_wrap() {
        let runtime = runtime(SandboxPlatform::Macos, |session| {
            session.filesystem.allow_write = vec![PathBuf::from("/work/*/build")];
        });

        let wrapped = runtime.wrap(&request()).unwrap();

        assert_eq!(wrapped.warnings.len(), 1);
        assert_eq!(wrapped.warnings[0].code, warning_code::GLOB_WRITE_PATTERN);
    }

    #[test]
    fn domain_rules_without_a_proxy_say_the_network_was_cut_instead() {
        // The gap this closes: `allowedDomains` reaches every backend as
        // "restrict the network", and every backend's way of doing that is
        // to route through a proxy. With no proxy listening, all three
        // block the network outright — a *stronger* rule than the one
        // configured, and one the user gets no notice of. Somebody whose
        // build cannot reach the registry has no way to tell that from a
        // broken network.
        for platform in [
            SandboxPlatform::Linux,
            SandboxPlatform::Macos,
            SandboxPlatform::Windows,
        ] {
            let runtime = runtime(platform, |session| {
                session.network.allowed_domains = vec!["api.example.com".into()];
                session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
                session.runtime.sandbox_win_path = Some(PathBuf::from(r"C:\Rebon\sandbox-win.exe"));
                // No `http_proxy_port`: nothing is listening.
            });

            let wrapped = runtime
                .wrap_with_fs(&restricted_request(), &FakeFs::new())
                .unwrap();

            let warning = wrapped
                .warnings
                .iter()
                .find(|w| w.code == warning_code::DOMAIN_RULES_WITHOUT_PROXY)
                .unwrap_or_else(|| panic!("{platform:?} degraded silently"));
            assert!(warning.detail.contains("api.example.com"), "{warning:?}");
            assert!(
                warning.detail.contains("no network access at all"),
                "{warning:?}"
            );
        }
    }

    #[test]
    fn domain_rules_with_a_proxy_are_not_warned_about() {
        let runtime = runtime(SandboxPlatform::Macos, |session| {
            session.network.allowed_domains = vec!["api.example.com".into()];
            session.runtime.http_proxy_port = Some(3128);
        });

        let wrapped = runtime.wrap(&restricted_request()).unwrap();

        assert!(!wrapped
            .warnings
            .iter()
            .any(|w| w.code == warning_code::DOMAIN_RULES_WITHOUT_PROXY));
    }

    #[test]
    fn a_command_with_no_domain_rules_is_not_warned_about() {
        // `needs_network_restriction` can be set for reasons other than
        // domains. Warning there would be noise about a rule nobody wrote.
        let runtime = runtime(SandboxPlatform::Macos, |_| {});

        let wrapped = runtime.wrap(&restricted_request()).unwrap();

        assert!(!wrapped
            .warnings
            .iter()
            .any(|w| w.code == warning_code::DOMAIN_RULES_WITHOUT_PROXY));
    }

    #[test]
    fn a_configured_seccomp_filter_says_it_is_not_applied() {
        // The field is parseable, the file is probed, and bubblewrap takes
        // a file descriptor rather than a path — so the wrap seam, which
        // produces an argv, cannot carry it. Saying so is the difference
        // between "syscall filtering is on" and "there is a setting for it".
        let runtime = runtime(SandboxPlatform::Linux, |session| {
            session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
            session.runtime.seccomp_config = Some(PathBuf::from("/etc/rebon/seccomp.bpf"));
        });

        let wrapped = runtime
            .wrap_with_fs(&restricted_request(), &FakeFs::new())
            .unwrap();

        let warning = wrapped
            .warnings
            .iter()
            .find(|w| w.code == warning_code::SECCOMP_NOT_APPLIED)
            .expect("a configured filter that is not applied must say so");
        assert!(warning.detail.contains("seccomp.bpf"), "{warning:?}");
        assert!(warning.detail.contains("filtering is off"), "{warning:?}");
    }

    #[test]
    fn no_seccomp_configured_means_no_seccomp_warning() {
        let runtime = runtime(SandboxPlatform::Linux, |session| {
            session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
        });

        let wrapped = runtime
            .wrap_with_fs(&restricted_request(), &FakeFs::new())
            .unwrap();

        assert!(!wrapped
            .warnings
            .iter()
            .any(|w| w.code == warning_code::SECCOMP_NOT_APPLIED));
    }

    #[test]
    fn the_unrestricted_fast_path_still_carries_no_warnings() {
        // Degradation warnings are computed after the fast path returns,
        // so a command with no rules must not start reporting them.
        let runtime = runtime(SandboxPlatform::Linux, |session| {
            session.runtime.seccomp_config = Some(PathBuf::from("/etc/rebon/seccomp.bpf"));
        });

        let wrapped = runtime.wrap(&request()).unwrap();

        assert_eq!(wrapped.backend, SandboxBackend::Passthrough);
        assert!(wrapped.warnings.is_empty());
    }

    #[test]
    fn strict_mode_refuses_a_command_when_confinement_is_not_real() {
        let runtime = SandboxRuntime::new(SandboxRuntimeInit {
            platform: SandboxPlatform::Linux,
            session: SessionSandboxConfig::default(),
            mode: SandboxMode::Strict,
            log_tag: "t".into(),
            debug_session: false,
            support: SupportReport::default(),
            probe: Arc::new(NeverConfined),
            session_resources: None,
        });

        assert!(matches!(
            runtime.assert_confined(true),
            Err(SandboxError::NotConfined { .. })
        ));
        assert!(
            runtime.assert_confined(false).is_ok(),
            "a command that was never going to be sandboxed is not probed"
        );
    }

    #[test]
    fn relaxed_mode_does_not_refuse() {
        let runtime = SandboxRuntime::new(SandboxRuntimeInit {
            platform: SandboxPlatform::Linux,
            session: SessionSandboxConfig::default(),
            mode: SandboxMode::Relaxed,
            log_tag: "t".into(),
            debug_session: false,
            support: SupportReport::default(),
            probe: Arc::new(NeverConfined),
            session_resources: None,
        });

        assert!(runtime.assert_confined(true).is_ok());
    }

    #[test]
    fn support_paths_are_folded_into_the_session_config() {
        let runtime = SandboxRuntime::new(SandboxRuntimeInit {
            platform: SandboxPlatform::Linux,
            session: SessionSandboxConfig::default(),
            mode: SandboxMode::Strict,
            log_tag: "t".into(),
            debug_session: false,
            support: SupportReport {
                platform: SandboxPlatform::Linux,
                bwrap_path: Some(PathBuf::from("/usr/bin/bwrap")),
                socat_path: Some(PathBuf::from("/usr/bin/socat")),
                ..Default::default()
            },
            probe: Arc::new(AlwaysConfined),
            session_resources: None,
        });

        assert_eq!(
            runtime.session_config().runtime.bwrap_path,
            Some(PathBuf::from("/usr/bin/bwrap"))
        );
    }

    #[test]
    fn reset_reports_acl_paths_on_windows_and_nothing_elsewhere() {
        let windows = runtime(SandboxPlatform::Windows, |session| {
            session.filesystem.allow_write = vec![PathBuf::from(r"C:\work")];
            session.filesystem.deny_read = vec![PathBuf::from(r"C:\secret")];
        });
        let released = windows.reset();
        assert!(released.contains(&PathBuf::from(r"C:\work")));
        assert!(released.contains(&PathBuf::from(r"C:\secret")));

        let linux = runtime(SandboxPlatform::Linux, |session| {
            session.filesystem.allow_write = vec![PathBuf::from("/work")];
        });
        assert!(linux.reset().is_empty());
    }

    #[test]
    fn the_powershell_encoded_command_shape_survives_every_backend() {
        // RFC §7.3 of the PowerShell tool: on Windows the whole
        // `pwsh … -EncodedCommand` prefix is the argv start; on Unix
        // under a sandbox the same prefix is the shell. One
        // `BinShell` covers both, which is the property that lets the
        // PowerShell tool hook in without a platform branch.
        let shell = BinShell::new("pwsh", ["-NoProfile", "-NonInteractive", "-EncodedCommand"]);
        let payload = "ZQBjAGgAbwAgAGgAaQA=";

        for platform in [
            SandboxPlatform::Linux,
            SandboxPlatform::Macos,
            SandboxPlatform::Windows,
        ] {
            let runtime = runtime(platform, |session| {
                session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
                session.runtime.sandbox_win_path = Some(PathBuf::from(r"C:\sandbox-win.exe"));
            });
            let request =
                CommandRequest::new(payload, shell.clone()).with_network_restriction(true);

            let wrapped = runtime.wrap_with_fs(&request, &FakeFs::new()).unwrap();
            let tail = &wrapped.args[wrapped.args.len() - 5..];

            assert_eq!(
                tail,
                &[
                    "pwsh",
                    "-NoProfile",
                    "-NonInteractive",
                    "-EncodedCommand",
                    payload
                ],
                "{platform:?} did not preserve the encoded-command argv"
            );
        }
    }

    #[test]
    fn backend_names_are_stable() {
        assert_eq!(SandboxBackend::Passthrough.as_str(), "passthrough");
        assert_eq!(SandboxBackend::Bubblewrap.as_str(), "bubblewrap");
        assert_eq!(SandboxBackend::Seatbelt.as_str(), "seatbelt");
        assert_eq!(SandboxBackend::SandboxWin.as_str(), "sandbox-win");
    }

    #[test]
    fn a_write_root_reaches_the_linux_mount_plan_through_the_runtime() {
        let fs = FakeFs::new().dir("/work");
        let runtime = runtime(SandboxPlatform::Linux, |session| {
            session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
            session.filesystem.allow_write = vec![PathBuf::from("/work")];
        });

        let wrapped = runtime.wrap_with_fs(&request(), &fs).unwrap();

        assert_eq!(wrapped.backend, SandboxBackend::Bubblewrap);
        let joined = wrapped.args.join(" ");
        assert!(joined.contains("--bind /work /work"));
        assert!(joined.contains("--ro-bind / /"));
        assert!(
            joined.contains("--share-net"),
            "a filesystem-only restriction must not also cut the network"
        );
    }

    #[test]
    fn per_command_acl_request_is_refused_on_windows_only() {
        let mut req = restricted_request();
        req.allow_write = vec![PathBuf::from(r"C:\other")];

        let windows = runtime(SandboxPlatform::Windows, |session| {
            session.runtime.sandbox_win_path = Some(PathBuf::from(r"C:\sandbox-win.exe"));
        });
        assert!(matches!(
            windows.wrap(&req),
            Err(SandboxError::PerExecAclUnsupported { .. })
        ));

        let linux = runtime(SandboxPlatform::Linux, |session| {
            session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
        });
        assert!(linux.wrap_with_fs(&req, &FakeFs::new()).is_ok());
    }

    #[test]
    fn debug_sessions_skip_the_probe_but_still_wrap() {
        let runtime = SandboxRuntime::new(SandboxRuntimeInit {
            platform: SandboxPlatform::Macos,
            session: SessionSandboxConfig::default(),
            mode: SandboxMode::Strict,
            log_tag: "t".into(),
            debug_session: true,
            support: SupportReport {
                platform: SandboxPlatform::Macos,
                ..Default::default()
            },
            probe: Arc::new(NeverConfined),
            session_resources: None,
        });

        assert!(runtime.assert_confined(true).is_ok());
        assert_eq!(
            runtime.wrap(&restricted_request()).unwrap().backend,
            SandboxBackend::Seatbelt
        );
    }

    #[test]
    fn a_deny_read_path_that_exists_reaches_the_plan_through_the_runtime() {
        let fs = FakeFs::new().dir("/etc").dir("/etc/secrets");
        let runtime = runtime(SandboxPlatform::Linux, |session| {
            session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
            session.filesystem.deny_read = vec![PathBuf::from("/etc/secrets")];
        });

        let wrapped = runtime.wrap_with_fs(&request(), &fs).unwrap();

        assert!(wrapped.args.join(" ").contains("--tmpfs /etc/secrets"));
    }
}
