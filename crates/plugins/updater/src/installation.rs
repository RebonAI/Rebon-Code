//! Where this Rebon came from, and the text every front end prints about it.
//!
//! [`crate::updater::installation_detection`] decides an installation source
//! from a path and a set of hints; this module is what supplies the hints for
//! *this* process (the running executable, the npm prefixes for this platform,
//! the workspace root of a dev build) and formats the answer. The notice a
//! finished update check leaves behind lives here too, because the same lines
//! carry it: `/update status`, `/status` and `rebon update status` all read
//! one formatter.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rebon_config::UpdatePreferences;

use crate::updater::{
    detect_installation_from_path, InstallationDetection, InstallationDetectionOptions,
};

/// Detect the current Rebon installation source without side effects.
pub fn detect_current_installation() -> InstallationDetection {
    match std::env::current_exe() {
        Ok(exe) => detect_installation_from_path(&exe, &current_detection_options()),
        Err(err) => InstallationDetection {
            installation_type: crate::updater::InstallationType::Unknown,
            evidence: format!("failed to resolve current executable path: {err}"),
        },
    }
}

fn current_detection_options() -> InstallationDetectionOptions {
    let mut workspace_roots = Vec::new();
    if let Some(root) = workspace_root_of(option_env!("CARGO_MANIFEST_DIR")) {
        workspace_roots.push(root);
    }

    InstallationDetectionOptions {
        workspace_roots,
        npm_global_prefixes: npm_global_prefix_hints(),
    }
}

/// The repository root this crate was compiled from, so a binary still
/// sitting under `target/` is recognised as a development build.
///
/// Counted, not searched: this crate's manifest is at
/// `<root>/crates/plugins/updater`, so the root is three levels up. The count
/// depends on where this crate sits, and getting it wrong does not fail to
/// compile — it just stops calling a dev build a dev build, which is why the
/// depth is asserted in a test rather than inlined at the call.
fn workspace_root_of(manifest_dir: Option<&str>) -> Option<PathBuf> {
    let dir = Path::new(manifest_dir?);
    Some(dir.parent()?.parent()?.parent()?.to_path_buf())
}

fn npm_global_prefix_hints() -> Vec<PathBuf> {
    let mut prefixes = Vec::new();
    if let Ok(prefix) = std::env::var("PREFIX") {
        if !prefix.trim().is_empty() {
            prefixes.push(PathBuf::from(prefix).join("lib"));
        }
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                prefixes.push(PathBuf::from(appdata).join("npm"));
            }
        }
    }
    #[cfg(not(windows))]
    {
        prefixes.push(PathBuf::from("/usr/local/lib"));
        prefixes.push(PathBuf::from("/usr/lib"));
        prefixes.push(PathBuf::from("/opt/homebrew/lib"));
    }
    prefixes
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStatusInfo {
    pub disabled: bool,
    pub auto_install: bool,
    pub channel: String,
    pub skipped_version: String,
    pub dismissed_version: String,
    pub installation: InstallationDetection,
}

impl UpdateStatusInfo {
    pub fn from_preferences(
        prefs: &UpdatePreferences,
        installation: InstallationDetection,
    ) -> Self {
        Self {
            disabled: prefs.disabled,
            auto_install: prefs.auto_install,
            channel: prefs.channel_name().unwrap_or("latest").to_string(),
            skipped_version: prefs
                .skipped_version
                .as_deref()
                .unwrap_or("(none)")
                .to_string(),
            dismissed_version: prefs
                .dismissed_version
                .as_deref()
                .unwrap_or("(none)")
                .to_string(),
            installation,
        }
    }
}

/// In-memory notice shown when the startup update check finds a newer npm package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNoticeState {
    pub current_version: String,
    pub latest_version: String,
    pub package_name: String,
    pub command: String,
    pub checked_at: SystemTime,
}

pub fn format_update_status(notice: Option<&UpdateNoticeState>) -> String {
    let prefs = match rebon_config::load_update_preferences() {
        Ok(prefs) => prefs,
        Err(err) => return format!("Failed to load update settings: {err}"),
    };
    format_update_status_with_preferences(notice, &prefs)
}

pub fn format_update_status_with_preferences(
    notice: Option<&UpdateNoticeState>,
    prefs: &UpdatePreferences,
) -> String {
    let status = UpdateStatusInfo::from_preferences(prefs, detect_current_installation());
    let mut lines = format_update_status_lines(&status);
    lines.push(format!(
        "update notice visible: {}",
        if notice.is_some() { "yes" } else { "no" }
    ));
    if let Some(notice) = notice {
        lines.push(format!("package: {}", notice.package_name));
        lines.push(format!("current: {}", notice.current_version));
        lines.push(format!("latest: {}", notice.latest_version));
        lines.push(format!("manual command (not run): {}", notice.command));
    }
    lines.join("\n")
}

pub fn format_update_status_lines(info: &UpdateStatusInfo) -> Vec<String> {
    vec![
        format!("disabled: {}", yes_no(info.disabled)),
        format!("autoInstall: {}", yes_no(info.auto_install)),
        format!("installationSource: {}", info.installation.source_label()),
        format!(
            "autoInstallSupported: {}",
            info.installation.auto_install_support_label()
        ),
        format!(
            "autoInstallSupport: {}",
            info.installation.auto_install_support_reason()
        ),
        format!("installationEvidence: {}", info.installation.evidence),
        format!("channel: {}", info.channel),
        format!("skippedVersion: {}", info.skipped_version),
        format!("dismissedVersion: {}", info.dismissed_version),
    ]
}

pub fn format_auto_install_status(
    prefs: &UpdatePreferences,
    installation: &InstallationDetection,
) -> String {
    format!(
        "Auto install updates: {}. installationSource: {}; autoInstallSupported: {}; autoInstallSupport: {}. Background runner registration is explicit: run `rebon update service install` to register the per-user scheduler. Package installation remains inactive until installer support lands.",
        if prefs.auto_install { "on" } else { "off" },
        installation.source_label(),
        installation.auto_install_support_label(),
        installation.auto_install_support_reason()
    )
}

pub fn format_headless_update_status(
    prefs: &UpdatePreferences,
    installation: InstallationDetection,
) -> String {
    format_update_status_lines(&UpdateStatusInfo::from_preferences(prefs, installation)).join("\n")
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::InstallationType;

    fn prefs() -> UpdatePreferences {
        UpdatePreferences {
            disabled: false,
            channel: Some("stable".to_string()),
            skipped_version: None,
            dismissed_version: Some("1.2.3".to_string()),
            dismissed_at_ms: None,
            auto_install: true,
        }
    }

    fn detection() -> InstallationDetection {
        InstallationDetection {
            installation_type: InstallationType::NpmGlobal,
            evidence: "test npm package path".to_string(),
        }
    }

    /// The depth this crate's manifest sits at under the repository root. A
    /// silent off-by-one here makes `target/debug/rebon` stop reading as a
    /// development build, which no other assertion in the suite notices.
    #[test]
    fn the_workspace_root_is_three_levels_above_this_crates_manifest() {
        assert_eq!(
            workspace_root_of(Some("/repo/crates/plugins/updater")),
            Some(PathBuf::from("/repo"))
        );
        assert_eq!(
            workspace_root_of(Some(env!("CARGO_MANIFEST_DIR")))
                .expect("this crate is nested deeply enough to have a root")
                .join("crates")
                .join("plugins")
                .join("updater"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        );
        assert_eq!(workspace_root_of(None), None);
        assert_eq!(workspace_root_of(Some("updater")), None);
    }

    #[test]
    fn headless_update_status_includes_installation_source_and_notify_only_support() {
        let status = format_headless_update_status(&prefs(), detection());
        assert!(status.contains("installationSource: npm-global"));
        assert!(status.contains("autoInstallSupported: no"));
        assert!(status.contains("autoInstallSupport: not active yet"));
        assert!(status.contains("autoInstall: yes"));
        assert!(status.contains("channel: stable"));
    }

    #[test]
    fn auto_install_status_includes_installation_source() {
        let status = format_auto_install_status(&prefs(), &detection());
        assert!(status.contains("Auto install updates: on"));
        assert!(status.contains("installationSource: npm-global"));
        assert!(status.contains("autoInstallSupported: no"));
        assert!(status.contains("run `rebon update service install`"));
    }

    #[test]
    fn update_status_text_includes_auto_install() {
        // The formatter takes the notice itself, so the test no longer
        // needs a terminal to hold one.
        let notice = UpdateNoticeState {
            current_version: "1.0.0".to_string(),
            latest_version: "1.1.0".to_string(),
            package_name: "@rebon/cli".to_string(),
            command: "npm install -g @rebon/cli@latest".to_string(),
            checked_at: std::time::SystemTime::UNIX_EPOCH,
        };
        let mut prefs = UpdatePreferences::default();
        prefs.auto_install = true;

        let status = format_update_status_with_preferences(Some(&notice), &prefs);

        assert!(status.contains("autoInstall: yes"));
        assert!(status.contains("installationSource:"));
        assert!(status.contains("autoInstallSupported: no"));
        assert!(status.contains("autoInstallSupport:"));
        assert!(status.contains("package: @rebon/cli"));
        assert!(status.contains("manual command (not run): npm install -g @rebon/cli@latest"));
    }
}
