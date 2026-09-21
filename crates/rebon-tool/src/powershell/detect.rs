//! Runtime detection for the PowerShell tool.
//!
//! Two questions, both answered once per process and cached:
//!
//! * **Which binary?** `pwsh` (PowerShell 7+) is preferred everywhere;
//!   `powershell.exe` (Windows PowerShell 5.1) is the last-resort fallback on
//!   Windows. The candidate order below is deliberate, plus a `REBON_POWERSHELL_PATH`
//!   override that mirrors `REBON_GIT_BASH_PATH` in `bash.rs`.
//! * **Which edition?** The two differ enough in *syntax* (`&&`/`||` exist only
//!   in 7+) that the model-facing description has to branch on it — see
//!   [`super::prompt`].

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Explicit override, checked before anything else.
pub const POWERSHELL_PATH_ENV_VAR: &str = "REBON_POWERSHELL_PATH";

/// Which PowerShell implementation a path resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerShellEdition {
    /// `pwsh` — PowerShell 7+, cross-platform. Has `&&`/`||`, `$PSStyle`,
    /// `$PSNativeCommandUseErrorActionPreference`.
    Core,
    /// `powershell.exe` — Windows PowerShell 5.1. Statement separator `;` only.
    Desktop,
}

impl PowerShellEdition {
    /// Basename, minus a `.exe` suffix, compared case-insensitively.
    pub fn from_path(path: &Path) -> Self {
        let stem = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .trim_end_matches(".exe")
            .trim_end_matches(".EXE")
            .to_ascii_lowercase();
        if stem == "pwsh" {
            Self::Core
        } else {
            Self::Desktop
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Desktop => "desktop",
        }
    }
}

/// Why a particular candidate won — logged once so a surprising choice
/// (Windows PowerShell 5.1 on a box that was supposed to have pwsh 7) is
/// visible in the trace rather than inferred from behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionReason {
    /// `REBON_POWERSHELL_PATH` pointed at it.
    EnvOverride,
    /// Found on `PATH` and nothing was odd about it.
    Path,
    /// PATH resolved to a snap wrapper, so a non-snap install was used
    /// instead (the snap build's sandbox breaks cwd and env).
    SnapWorkaround,
    /// One of the well-known Windows install locations.
    WindowsFallbackPath,
    /// No `pwsh` anywhere; Windows PowerShell 5.1 was used.
    FellBackToPowerShell5,
}

impl DetectionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EnvOverride => "env_override",
            Self::Path => "path",
            Self::SnapWorkaround => "snap_workaround",
            Self::WindowsFallbackPath => "windows_fallback_path",
            Self::FellBackToPowerShell5 => "fell_back_to_powershell_5",
        }
    }
}

/// A resolved PowerShell runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerShellRuntime {
    pub path: PathBuf,
    pub edition: PowerShellEdition,
    pub reason: DetectionReason,
}

static RUNTIME: OnceLock<Option<PowerShellRuntime>> = OnceLock::new();

/// The PowerShell runtime for this process, or `None` when none is installed.
///
/// Cached: detection touches the filesystem and (on Windows) spawns
/// `where.exe`, and the answer cannot change under a running process in any
/// way worth re-probing for.
pub fn runtime() -> Option<&'static PowerShellRuntime> {
    RUNTIME.get_or_init(detect).as_ref()
}

/// Whether a PowerShell runtime exists at all.
pub fn is_available() -> bool {
    runtime().is_some()
}

/// The message shown when a PowerShell command is attempted with no runtime
/// installed. It has to be actionable rather than a bare
/// "not available".
pub fn unavailable_message() -> String {
    if cfg!(windows) {
        format!(
            "PowerShell is not available. Windows ships Windows PowerShell 5.1 at \
             %SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe; if it has been \
             removed, install PowerShell 7 (`winget install Microsoft.PowerShell`) or point \
             {POWERSHELL_PATH_ENV_VAR} at an existing pwsh.exe. Use the Bash tool in the \
             meantime."
        )
    } else {
        format!(
            "PowerShell is not available. Install PowerShell 7 (see \
             https://learn.microsoft.com/powershell/scripting/install/installing-powershell) \
             or point {POWERSHELL_PATH_ENV_VAR} at an existing `pwsh` binary. Use the Bash \
             tool in the meantime."
        )
    }
}

fn detect() -> Option<PowerShellRuntime> {
    let found = detect_uncached();
    match &found {
        Some(runtime) => tracing::debug!(
            path = %runtime.path.display(),
            edition = runtime.edition.as_str(),
            reason = runtime.reason.as_str(),
            "shell_powershell_detect",
        ),
        None => tracing::debug!("shell_powershell_detect: no PowerShell runtime found"),
    }
    found
}

fn detect_uncached() -> Option<PowerShellRuntime> {
    if let Some(path) = env_override() {
        return Some(finish(path, DetectionReason::EnvOverride));
    }
    platform_candidates()
}

fn env_override() -> Option<PathBuf> {
    let raw = std::env::var(POWERSHELL_PATH_ENV_VAR).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    // A bare name is resolved through PATH; a path must actually exist so a
    // stale override degrades to normal detection instead of failing every
    // command with ENOENT.
    if path.components().count() == 1 {
        return which(trimmed);
    }
    path.is_file().then_some(path)
}

fn finish(path: PathBuf, reason: DetectionReason) -> PowerShellRuntime {
    PowerShellRuntime {
        edition: PowerShellEdition::from_path(&path),
        path,
        reason,
    }
}

#[cfg(windows)]
fn platform_candidates() -> Option<PowerShellRuntime> {
    // Windows order: the 7.x install first, then the two per-user
    // locations, then Windows PowerShell 5.1.
    let well_known_pwsh = [
        std::env::var_os("ProgramFiles")
            .map(|dir| PathBuf::from(dir).join(r"PowerShell\7\pwsh.exe")),
        std::env::var_os("LOCALAPPDATA")
            .map(|dir| PathBuf::from(dir).join(r"Microsoft\WindowsApps\pwsh.exe")),
        std::env::var_os("USERPROFILE")
            .map(|dir| PathBuf::from(dir).join(r".dotnet\tools\pwsh.exe")),
    ];
    for candidate in well_known_pwsh.into_iter().flatten() {
        if candidate.is_file() {
            return Some(finish(candidate, DetectionReason::WindowsFallbackPath));
        }
    }
    if let Some(path) = which("pwsh") {
        return Some(finish(path, DetectionReason::Path));
    }
    if let Some(path) = which("powershell") {
        return Some(finish(path, DetectionReason::FellBackToPowerShell5));
    }
    let system_powershell = std::env::var_os("SystemRoot")
        .map(|dir| PathBuf::from(dir).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"))?;
    system_powershell
        .is_file()
        .then(|| finish(system_powershell, DetectionReason::FellBackToPowerShell5))
}

#[cfg(not(windows))]
fn platform_candidates() -> Option<PowerShellRuntime> {
    let path = which("pwsh")?;
    // The snap build runs confined and reports a cwd/env that do not
    // match what we spawned it with, so prefer any non-snap install.
    if path.to_string_lossy().contains("/snap/") {
        for candidate in [
            PathBuf::from("/opt/microsoft/powershell/7/pwsh"),
            PathBuf::from("/usr/bin/pwsh"),
        ] {
            if candidate.is_file() && !candidate.to_string_lossy().contains("/snap/") {
                return Some(finish(candidate, DetectionReason::SnapWorkaround));
            }
        }
    }
    Some(finish(path, DetectionReason::Path))
}

/// Minimal `which`: resolve `name` against `PATH`.
///
/// Windows walks `PATH` in-process with the `PATHEXT` extensions and sees
/// the App Execution Alias reparse points that `pwsh` installs from the
/// Store use (`crate::path_lookup`) — it used to spawn `where.exe` for
/// this, 100–300 ms on the first tool snapshot of every process; elsewhere
/// we scan `PATH` ourselves rather than depend on `which(1)` being present
/// in a container image.
#[cfg(windows)]
fn which(name: &str) -> Option<PathBuf> {
    crate::path_lookup::executables_on_path(name)
        .into_iter()
        .next()
}

#[cfg(not(windows))]
fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edition_is_read_from_the_binary_name() {
        assert_eq!(
            PowerShellEdition::from_path(Path::new("/usr/bin/pwsh")),
            PowerShellEdition::Core
        );
        assert_eq!(
            PowerShellEdition::from_path(Path::new("PWSH.EXE")),
            PowerShellEdition::Core
        );
        assert_eq!(
            PowerShellEdition::from_path(Path::new("powershell")),
            PowerShellEdition::Desktop
        );
    }

    /// Backslash paths only split into a file name on Windows; elsewhere
    /// they are one opaque component, which is also where they never occur.
    #[cfg(windows)]
    #[test]
    fn edition_is_read_from_a_windows_binary_path() {
        assert_eq!(
            PowerShellEdition::from_path(Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe")),
            PowerShellEdition::Core
        );
        assert_eq!(
            PowerShellEdition::from_path(Path::new(
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
            )),
            PowerShellEdition::Desktop
        );
    }

    #[test]
    fn unavailable_message_names_a_next_step() {
        let message = unavailable_message();
        assert!(message.contains(POWERSHELL_PATH_ENV_VAR), "{message}");
        assert!(message.contains("Bash"), "{message}");
    }

    /// Detection is cached and must never panic, whatever the host looks like.
    #[test]
    fn detection_is_stable_across_calls() {
        assert_eq!(runtime().is_some(), is_available());
        assert_eq!(runtime(), runtime());
    }

    /// Windows always has *some* PowerShell; a missing one means detection is
    /// broken rather than the box being unusual.
    #[cfg(windows)]
    #[test]
    fn windows_always_resolves_a_runtime() {
        let runtime = runtime().expect("Windows ships Windows PowerShell 5.1");
        assert!(runtime.path.is_file(), "{}", runtime.path.display());
    }
}
