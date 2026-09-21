//! Dependency discovery and the support report — RFC §10.
//!
//! Two audiences, one report. `/sandbox` and `/doctor` render it as
//! rows a user can act on; the runtime reads the same report to
//! decide whether it may accept a session at all. Keeping them on one
//! structure is what stops the doctor from saying "available" while
//! the executor refuses every command.
//!
//! The report is a *value*, not a side effect: [`probe_support`] does
//! the filesystem work and everything downstream is pure. That means
//! the whole verdict matrix is testable without a bubblewrap install.

use crate::runtime::config::RuntimePaths;
use crate::runtime::windows::SandboxWinStatus;
use crate::view::dependency::SandboxDependencyCheck;
use crate::view::platform::SandboxPlatform;
use std::path::{Path, PathBuf};

/// What was found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportReport {
    pub platform: SandboxPlatform,
    /// Resolved native paths, ready to be copied into
    /// [`RuntimePaths`].
    pub bwrap_path: Option<PathBuf>,
    pub socat_path: Option<PathBuf>,
    pub ripgrep_path: Option<PathBuf>,
    pub seccomp_config: Option<PathBuf>,
    pub sandbox_win: SandboxWinStatus,
    pub sandbox_win_path: Option<PathBuf>,
    /// Free-form messages, in the shape the existing doctor surface
    /// already classifies (`String::contains` on `"bwrap"`,
    /// `"socat"`, `"ripgrep"`). The wording is chosen to hit those
    /// buckets deliberately — see
    /// [`crate::view::dependency::classify_errors`].
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// `Unknown` is the default platform, not the host's.
///
/// `SandboxPlatform` deliberately has no `Default` of its own — the
/// `/sandbox` UI in this crate pins its variants, and adding one would be a
/// statement about which platform is "normal". A default report is an empty
/// one, and an empty report knows nothing about the host, so
/// `Unknown` is the honest value.
impl Default for SupportReport {
    fn default() -> Self {
        Self {
            platform: SandboxPlatform::Unknown,
            bwrap_path: None,
            socat_path: None,
            ripgrep_path: None,
            seccomp_config: None,
            sandbox_win: SandboxWinStatus::default(),
            sandbox_win_path: None,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

impl SupportReport {
    /// Whether a sandbox can be built on this machine at all.
    pub fn is_usable(&self) -> bool {
        self.errors.is_empty()
    }

    /// Project into the shape `/doctor` and the `/sandbox` UI already
    /// consume.
    pub fn dependency_check(&self) -> SandboxDependencyCheck {
        SandboxDependencyCheck {
            errors: self.errors.clone(),
            warnings: self.warnings.clone(),
        }
    }

    /// Fill in the native halves of a session's runtime paths.
    ///
    /// Fills blanks only. A path the caller already set — from an
    /// explicit override, or from a probe run earlier with more
    /// context — survives, because a report that found nothing must
    /// not be able to erase a path that works. Ports, sockets, and
    /// the CA path are never touched: they belong to the proxy and
    /// are set by whoever starts it.
    pub fn apply_to(&self, runtime: &mut RuntimePaths) {
        fill(&mut runtime.bwrap_path, &self.bwrap_path);
        fill(&mut runtime.socat_path, &self.socat_path);
        fill(&mut runtime.seccomp_config, &self.seccomp_config);
        fill(&mut runtime.sandbox_win_path, &self.sandbox_win_path);
    }
}

fn fill(slot: &mut Option<PathBuf>, discovered: &Option<PathBuf>) {
    if slot.is_none() {
        slot.clone_from(discovered);
    }
}

/// The platform this build is running on.
///
/// Deliberately derived from `cfg!` rather than
/// `std::env::consts::OS` string matching so an unhandled target is a
/// compile-time-visible `Unknown` rather than a typo nobody notices.
pub fn current_platform() -> SandboxPlatform {
    if cfg!(target_os = "macos") {
        SandboxPlatform::Macos
    } else if cfg!(target_os = "linux") {
        SandboxPlatform::Linux
    } else if cfg!(target_os = "windows") {
        SandboxPlatform::Windows
    } else {
        SandboxPlatform::Unknown
    }
}

/// Whether this crate can actually confine commands on `platform`.
///
/// This is **not** the same predicate as
/// [`SandboxPlatform::is_supported`], and the difference is load
/// bearing. That one answers "does the `/sandbox` UI offer this
/// platform", and it says macOS and Linux only — a rule this crate's
/// `/sandbox` UI pins with tests because a user on an unsupported
/// platform must never be told the sandbox is active.
///
/// This one answers "does a backend exist here", and Windows is
/// included because [`crate::runtime::windows`] builds a real `sandbox-win` argv.
/// The two stay separate rather than being unified: whether Windows
/// counts as supported depends on whether the helper is installed,
/// which is a runtime fact, and the UI predicate is a compile-time
/// one. Reconciling them means checking [`SandboxWinStatus::is_ready`],
/// not widening the UI's answer.
pub fn has_backend(platform: SandboxPlatform) -> bool {
    matches!(
        platform,
        SandboxPlatform::Macos | SandboxPlatform::Linux | SandboxPlatform::Windows
    )
}

/// How the probe looks for a program. Injected so the whole verdict
/// matrix is testable without installing anything.
pub trait ProgramLookup {
    /// Absolute path to `name`, or `None` if it is not reachable.
    fn find(&self, name: &str) -> Option<PathBuf>;
}

/// `PATH` resolution against the real filesystem.
pub struct PathLookup;

impl ProgramLookup for PathLookup {
    fn find(&self, name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        let extensions: Vec<String> = if cfg!(windows) {
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_string())
                .split(';')
                .filter(|ext| !ext.is_empty())
                .map(|ext| ext.to_ascii_lowercase())
                .collect()
        } else {
            vec![String::new()]
        };
        for dir in std::env::split_paths(&path) {
            for extension in &extensions {
                let candidate = if extension.is_empty() {
                    dir.join(name)
                } else {
                    dir.join(format!("{name}{extension}"))
                };
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }
}

/// Run the dependency probe — RFC §10.
///
/// `sandbox_enabled` gates the whole thing: probing costs several
/// `PATH` walks per session, and a user who turned the sandbox off
/// should not pay for them or see install hints for a feature they
/// disabled.
pub fn probe_support(
    platform: SandboxPlatform,
    sandbox_enabled: bool,
    lookup: &dyn ProgramLookup,
    sandbox_win: SandboxWinStatus,
    sandbox_win_path: Option<PathBuf>,
) -> SupportReport {
    let mut report = SupportReport {
        platform,
        sandbox_win,
        sandbox_win_path,
        ..Default::default()
    };

    if !has_backend(platform) {
        report
            .errors
            .push(crate::view::dependency::UNSUPPORTED_PLATFORM_ERROR.to_string());
        return report;
    }
    if !sandbox_enabled {
        return report;
    }

    // ripgrep is checked on every platform because the sandbox's own
    // file-listing paths shell out to it; a missing `rg` degrades
    // searches inside the sandbox in a way that looks like the
    // sandbox blocking them.
    report.ripgrep_path = lookup.find("rg");
    if report.ripgrep_path.is_none() {
        report
            .errors
            .push("ripgrep (rg) was not found on PATH".to_string());
    }

    match platform {
        SandboxPlatform::Linux => {
            report.bwrap_path = lookup.find("bwrap");
            if report.bwrap_path.is_none() {
                report
                    .errors
                    .push("bwrap (bubblewrap) was not found on PATH".to_string());
            }
            report.socat_path = lookup.find("socat");
            if report.socat_path.is_none() {
                // An error, not a warning: without socat a
                // network-restricted command cannot reach the proxy,
                // and "no network at all" is a different sandbox than
                // the one the user configured.
                report
                    .errors
                    .push("socat was not found on PATH".to_string());
            }
        }
        SandboxPlatform::Windows => {
            if !sandbox_win.is_ready() {
                for step in sandbox_win.remediation() {
                    report.errors.push(step);
                }
            }
        }
        // macOS ships `sandbox-exec`; there is nothing to install.
        SandboxPlatform::Macos | SandboxPlatform::Unknown => {}
    }

    report
}

/// Whether a seccomp filter file is usable.
///
/// A configured-but-unreadable filter is a warning rather than an
/// error on purpose: bubblewrap still confines the filesystem and the
/// network without it, so refusing the whole session would trade a
/// partial sandbox for no sandbox.
pub fn check_seccomp(config: Option<&Path>, report: &mut SupportReport) {
    let Some(config) = config else {
        return;
    };
    if config.is_file() {
        report.seccomp_config = Some(config.to_path_buf());
    } else {
        report.warnings.push(format!(
            "seccomp filter {} could not be read; syscall filtering is off",
            config.display()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct FakeLookup(BTreeMap<String, PathBuf>);

    impl FakeLookup {
        fn with(names: &[&str]) -> Self {
            Self(
                names
                    .iter()
                    .map(|name| {
                        (
                            (*name).to_string(),
                            PathBuf::from(format!("/usr/bin/{name}")),
                        )
                    })
                    .collect(),
            )
        }
    }

    impl ProgramLookup for FakeLookup {
        fn find(&self, name: &str) -> Option<PathBuf> {
            self.0.get(name).cloned()
        }
    }

    fn ready_windows() -> SandboxWinStatus {
        SandboxWinStatus {
            binary_present: true,
            version: Some(crate::runtime::windows::SUPPORTED_STATUS_VERSION),
            user_provisioned: true,
            credentials_present: true,
            wfp_installed: true,
        }
    }

    #[test]
    fn unsupported_platform_reports_the_sentinel_and_probes_nothing() {
        let lookup = FakeLookup::with(&[]);
        let report = probe_support(
            SandboxPlatform::Unknown,
            true,
            &lookup,
            SandboxWinStatus::default(),
            None,
        );

        assert_eq!(
            report.errors,
            vec![crate::view::dependency::UNSUPPORTED_PLATFORM_ERROR]
        );
        assert!(report.ripgrep_path.is_none());
    }

    #[test]
    fn a_disabled_sandbox_probes_nothing_and_reports_clean() {
        let lookup = FakeLookup::with(&[]);
        let report = probe_support(
            SandboxPlatform::Linux,
            false,
            &lookup,
            SandboxWinStatus::default(),
            None,
        );

        assert!(report.errors.is_empty());
        assert!(report.warnings.is_empty());
        assert!(report.bwrap_path.is_none());
        assert!(report.is_usable());
    }

    #[test]
    fn linux_needs_ripgrep_bwrap_and_socat() {
        let lookup = FakeLookup::with(&[]);
        let report = probe_support(
            SandboxPlatform::Linux,
            true,
            &lookup,
            SandboxWinStatus::default(),
            None,
        );

        // The wording must land in the doctor's existing buckets.
        let classification = crate::view::dependency::classify_errors(&report.dependency_check());
        assert!(classification.ripgrep_missing);
        assert!(classification.bwrap_missing);
        assert!(classification.socat_missing);
        assert!(classification.other_errors.is_empty());
        assert!(!report.is_usable());
    }

    #[test]
    fn a_complete_linux_box_reports_clean_and_resolved_paths() {
        let lookup = FakeLookup::with(&["rg", "bwrap", "socat"]);
        let report = probe_support(
            SandboxPlatform::Linux,
            true,
            &lookup,
            SandboxWinStatus::default(),
            None,
        );

        assert!(report.is_usable());
        assert_eq!(report.bwrap_path, Some(PathBuf::from("/usr/bin/bwrap")));
        assert_eq!(report.socat_path, Some(PathBuf::from("/usr/bin/socat")));
    }

    #[test]
    fn macos_needs_only_ripgrep() {
        let lookup = FakeLookup::with(&["rg"]);
        let report = probe_support(
            SandboxPlatform::Macos,
            true,
            &lookup,
            SandboxWinStatus::default(),
            None,
        );

        assert!(report.is_usable());
        assert!(report.bwrap_path.is_none());
    }

    #[test]
    fn windows_reports_the_helper_remediation_as_errors() {
        let lookup = FakeLookup::with(&["rg"]);
        let report = probe_support(
            SandboxPlatform::Windows,
            true,
            &lookup,
            SandboxWinStatus::default(),
            None,
        );

        assert!(!report.is_usable());
        assert!(report.errors.iter().any(|e| e.contains("sandbox-win.exe")));
    }

    #[test]
    fn a_ready_windows_box_reports_clean() {
        let lookup = FakeLookup::with(&["rg"]);
        let report = probe_support(
            SandboxPlatform::Windows,
            true,
            &lookup,
            ready_windows(),
            Some(PathBuf::from(r"C:\Rebon\sandbox-win.exe")),
        );

        assert!(report.is_usable());
    }

    #[test]
    fn apply_to_fills_only_the_discovered_fields() {
        let lookup = FakeLookup::with(&["rg", "bwrap", "socat"]);
        let report = probe_support(
            SandboxPlatform::Linux,
            true,
            &lookup,
            SandboxWinStatus::default(),
            None,
        );
        let mut runtime = RuntimePaths {
            http_proxy_port: Some(3128),
            ..Default::default()
        };

        report.apply_to(&mut runtime);

        assert_eq!(runtime.bwrap_path, Some(PathBuf::from("/usr/bin/bwrap")));
        assert_eq!(
            runtime.http_proxy_port,
            Some(3128),
            "the probe must not clobber the proxy's own fields"
        );
    }

    #[test]
    fn seccomp_file_missing_is_a_warning_not_an_error() {
        let mut report = SupportReport::default();
        check_seccomp(Some(Path::new("/nonexistent/seccomp.bpf")), &mut report);

        assert!(report.errors.is_empty());
        assert_eq!(report.warnings.len(), 1);
        assert!(report.is_usable());
    }

    #[test]
    fn seccomp_file_present_is_recorded() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut report = SupportReport::default();

        check_seccomp(Some(temp.path()), &mut report);

        assert_eq!(report.seccomp_config, Some(temp.path().to_path_buf()));
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn backend_availability_includes_windows_while_the_ui_predicate_does_not() {
        assert!(has_backend(SandboxPlatform::Windows));
        assert!(
            !SandboxPlatform::Windows.is_supported(),
            "the UI predicate stays as it is; the two answer different questions"
        );
        assert!(!has_backend(SandboxPlatform::Unknown));
    }

    #[test]
    fn current_platform_matches_the_build_target() {
        let platform = current_platform();
        if cfg!(target_os = "windows") {
            assert_eq!(platform, SandboxPlatform::Windows);
        } else if cfg!(target_os = "macos") {
            assert_eq!(platform, SandboxPlatform::Macos);
        } else if cfg!(target_os = "linux") {
            assert_eq!(platform, SandboxPlatform::Linux);
        }
    }

    #[test]
    fn path_lookup_finds_a_real_program() {
        let lookup = PathLookup;
        let probe = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(
            lookup.find(probe).is_some(),
            "{probe} should be resolvable on PATH"
        );
        assert!(lookup.find("rebon-definitely-not-a-program").is_none());
    }
}
