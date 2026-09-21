//! Conservative, read-only installation-source detection for update status.
//!
//! This module intentionally does not install, update, register services, or
//! execute package managers. It only classifies paths when the current
//! executable/launcher layout has positive evidence.

use std::path::{Component, Path, PathBuf};

use crate::InstallationType;

/// Read-only result of installation-source detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationDetection {
    pub installation_type: InstallationType,
    pub evidence: String,
}

impl InstallationDetection {
    pub fn source_label(&self) -> &'static str {
        self.installation_type.as_str()
    }

    /// Status-only. Even positively detected npm installs are not
    /// auto-installed: the service, lock, and state plumbing all exists,
    /// but package installation is not wired to it yet.
    pub fn auto_install_supported(&self) -> bool {
        false
    }

    pub fn auto_install_support_label(&self) -> &'static str {
        "no"
    }

    pub fn auto_install_support_reason(&self) -> &'static str {
        match self.installation_type {
            InstallationType::NpmGlobal | InstallationType::NpmLocal => {
                "not active yet; notify-only until updater service/lock/state support lands"
            }
            InstallationType::Development => "development checkout; notify-only",
            InstallationType::Native => {
                "native standalone auto-install is not active yet; notify-only"
            }
            InstallationType::PackageManager => "package-manager installs are notify-only",
            InstallationType::Unknown => "installation source is unknown; notify-only",
        }
    }
}

/// Optional path-only hints supplied by the caller/test harness. No filesystem
/// reads or subprocesses are performed by [`detect_installation_from_path`].
#[derive(Debug, Clone, Default)]
pub struct InstallationDetectionOptions {
    /// Directories known to be workspace checkouts. A `target/debug/rebon` or
    /// `target/release/rebon` path underneath one of these roots is classified
    /// as development.
    pub workspace_roots: Vec<PathBuf>,
    /// Directories known to be global npm prefixes. A verified Rebon npm package
    /// path under one of these roots is classified as `npm-global`; otherwise a
    /// verified npm package path is classified as `npm-local`.
    pub npm_global_prefixes: Vec<PathBuf>,
}

/// Classify an executable path using positive, path-only evidence.
pub fn detect_installation_from_path(
    executable: &Path,
    options: &InstallationDetectionOptions,
) -> InstallationDetection {
    if is_development_target_path(executable, &options.workspace_roots) {
        return InstallationDetection {
            installation_type: InstallationType::Development,
            evidence: format!(
                "executable path is under a workspace target/debug or target/release directory: {}",
                executable.display()
            ),
        };
    }

    if let Some(package) = npm_rebon_package_from_path(executable) {
        let installation_type = if options
            .npm_global_prefixes
            .iter()
            .any(|prefix| path_starts_with(executable, prefix))
        {
            InstallationType::NpmGlobal
        } else {
            InstallationType::NpmLocal
        };
        return InstallationDetection {
            installation_type,
            evidence: format!(
                "executable path is inside verified npm Rebon package {package}: {}",
                executable.display()
            ),
        };
    }

    if looks_like_rebon_executable(executable) {
        return InstallationDetection {
            installation_type: InstallationType::Native,
            evidence: format!(
                "standalone Rebon-named executable without verified npm/dev metadata: {}",
                executable.display()
            ),
        };
    }

    InstallationDetection {
        installation_type: InstallationType::Unknown,
        evidence: format!(
            "no positive Rebon installation-source evidence for executable path: {}",
            executable.display()
        ),
    }
}

fn is_development_target_path(executable: &Path, workspace_roots: &[PathBuf]) -> bool {
    if !looks_like_rebon_executable(executable) {
        return false;
    }
    let components = path_component_strings(executable);
    let has_target_profile = components
        .windows(2)
        .any(|pair| pair[0] == "target" && (pair[1] == "debug" || pair[1] == "release"));
    has_target_profile
        && workspace_roots
            .iter()
            .any(|root| path_starts_with(executable, root))
}

fn npm_rebon_package_from_path(executable: &Path) -> Option<String> {
    if !looks_like_rebon_executable(executable) {
        return None;
    }
    let components = path_component_strings(executable);

    for window in components.windows(5) {
        if window[0] == "node_modules"
            && window[1] == "@rebon"
            && is_scoped_rebon_package(&window[2])
            && window[3] == "bin"
            && is_rebon_launcher_name(&window[4])
        {
            return Some(format!("@rebon/{}", window[2]));
        }
    }

    for window in components.windows(4) {
        if window[0] == "node_modules"
            && is_cli_platform_package(&window[1])
            && window[2] == "bin"
            && is_rebon_executable_name(&window[3])
        {
            return Some(window[1].clone());
        }
    }
    None
}

fn is_scoped_rebon_package(name: &str) -> bool {
    name == "cli" || is_cli_platform_package(name)
}

fn is_cli_platform_package(name: &str) -> bool {
    if !name.starts_with("cli-") {
        return false;
    }
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() != 3 {
        return false;
    }
    matches!(parts[1], "win32" | "darwin" | "linux")
        && matches!(parts[2], "x64" | "arm64" | "arm" | "ia32")
}

fn looks_like_rebon_executable(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_rebon_executable_name)
}

fn is_rebon_executable_name(name: &str) -> bool {
    name == "rebon" || name == "rebon.exe" || name == "rebon.js"
}

fn is_rebon_launcher_name(name: &str) -> bool {
    is_rebon_executable_name(name)
}

fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    if prefix.as_os_str().is_empty() {
        return false;
    }
    path.components()
        .zip(prefix.components())
        .all(|(a, b)| a == b)
        && path.components().count() >= prefix.components().count()
}

fn path_component_strings(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(|s| s.to_string()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> InstallationDetectionOptions {
        InstallationDetectionOptions {
            workspace_roots: vec![PathBuf::from("/repo/rebon")],
            npm_global_prefixes: vec![PathBuf::from("/usr/local/lib")],
        }
    }

    #[test]
    fn detects_development_target_debug_path() {
        let detection =
            detect_installation_from_path(Path::new("/repo/rebon/target/debug/rebon"), &opts());
        assert_eq!(detection.installation_type, InstallationType::Development);
        assert_eq!(detection.source_label(), "development");
        assert!(detection.evidence.contains("target/debug"));
    }

    #[test]
    fn detects_development_target_release_path() {
        let detection =
            detect_installation_from_path(Path::new("/repo/rebon/target/release/rebon"), &opts());
        assert_eq!(detection.installation_type, InstallationType::Development);
    }

    #[test]
    fn does_not_call_non_workspace_target_path_development() {
        let detection =
            detect_installation_from_path(Path::new("/opt/rebon/target/release/rebon"), &opts());
        assert_eq!(detection.installation_type, InstallationType::Native);
    }

    #[test]
    fn detects_npm_global_platform_package_path() {
        let detection = detect_installation_from_path(
            Path::new("/usr/local/lib/node_modules/cli-linux-x64/bin/rebon"),
            &opts(),
        );
        assert_eq!(detection.installation_type, InstallationType::NpmGlobal);
        assert!(detection.evidence.contains("cli-linux-x64"));
    }

    #[test]
    fn detects_npm_global_scoped_wrapper_path() {
        let detection = detect_installation_from_path(
            Path::new("/usr/local/lib/node_modules/@rebon/cli/bin/rebon.js"),
            &opts(),
        );
        assert_eq!(detection.installation_type, InstallationType::NpmGlobal);
        assert!(detection.evidence.contains("@rebon/cli"));
    }

    #[test]
    fn detects_npm_local_scoped_platform_package_path() {
        let detection = detect_installation_from_path(
            Path::new("/home/alice/project/node_modules/@rebon/cli-darwin-arm64/bin/rebon"),
            &opts(),
        );
        assert_eq!(detection.installation_type, InstallationType::NpmLocal);
        assert!(detection.evidence.contains("@rebon/cli-darwin-arm64"));
    }

    #[test]
    fn detects_npm_local_platform_package_path() {
        let detection = detect_installation_from_path(
            Path::new("/home/alice/project/node_modules/cli-darwin-arm64/bin/rebon"),
            &opts(),
        );
        assert_eq!(detection.installation_type, InstallationType::NpmLocal);
    }

    #[test]
    fn rejects_unverified_npm_like_package_name() {
        let detection = detect_installation_from_path(
            Path::new("/home/alice/project/node_modules/not-rebon/bin/rebon"),
            &opts(),
        );
        assert_eq!(detection.installation_type, InstallationType::Native);
    }

    #[test]
    fn unknown_non_rebon_path_falls_back_to_unknown() {
        let detection = detect_installation_from_path(Path::new("/tmp/tool"), &opts());
        assert_eq!(detection.installation_type, InstallationType::Unknown);
        assert_eq!(detection.source_label(), "unknown");
    }

    #[test]
    fn native_rebon_path_is_standalone_notify_only() {
        let detection = detect_installation_from_path(Path::new("/opt/rebon/bin/rebon"), &opts());
        assert_eq!(detection.installation_type, InstallationType::Native);
        assert!(!detection.auto_install_supported());
        assert_eq!(detection.auto_install_support_label(), "no");
        assert!(detection
            .auto_install_support_reason()
            .contains("notify-only"));
    }
}
