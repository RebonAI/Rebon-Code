//! Where this Rebon came from, and the text every front end prints about it.
//!
//! [`crate::updater::installation_detection`] decides an installation source
//! from a path and a set of hints; this module is what supplies the hints for
//! *this* process (the running executable, the npm prefixes for this platform,
//! the workspace root of a dev build) and formats the answer. The notice a
//! finished update check leaves behind lives here too. Interactive surfaces
//! show a summary; `rebon update status` retains the diagnostic details.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rebon_config::UpdatePreferences;

use crate::updater::{
    detect_installation_from_path, InstallationDetection, InstallationDetectionOptions,
};

/// Detect the current Rebon installation source without side effects.
pub fn detect_current_installation() -> InstallationDetection {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Ok(install) = current_npm_installation_for(&exe) {
                return install.detection();
            }
            detect_installation_from_path(&exe, &current_detection_options())
        }
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

/// Launcher hints are untrusted input, not authority to run npm. An install is
/// accepted only after its manifests, package layout and running binary agree.
#[derive(Debug, serde::Deserialize)]
struct NpmLauncherSource {
    wrapper: PathBuf,
    platform: PathBuf,
    executable: PathBuf,
    node: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct NpmInstallation {
    pub wrapper: PathBuf,
    pub prefix: PathBuf,
    pub node: PathBuf,
    pub global: bool,
    pub platform_package: String,
}

impl NpmInstallation {
    fn detection(&self) -> InstallationDetection {
        InstallationDetection {
            installation_type: if self.global {
                crate::InstallationType::NpmGlobal
            } else {
                crate::InstallationType::NpmLocal
            },
            evidence: format!(
                "validated npm wrapper {} (prefix {})",
                self.wrapper.display(),
                self.prefix.display()
            ),
        }
    }
}

pub(crate) fn current_npm_installation() -> anyhow::Result<NpmInstallation> {
    current_npm_installation_for(&std::env::current_exe()?)
}

fn current_npm_installation_for(exe: &Path) -> anyhow::Result<NpmInstallation> {
    let raw = std::env::var("REBON_NPM_INSTALL")
        .map_err(|_| anyhow::anyhow!("This installation has no verified npm wrapper source; run Rebon through its npm launcher to update it."))?;
    let managed = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|v| !v.is_empty())
                .map(|v| PathBuf::from(v).join(".rebon"))
        })
        .map(|base| base.join("rebon/bin"));
    validate_npm_installation(exe, &raw, managed.as_deref(), cfg!(windows))
}

fn manifest(path: &Path) -> anyhow::Result<serde_json::Value> {
    use anyhow::Context;
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid package manifest {}", path.display()))?)
}

fn absolute_real_path(path: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "npm source path is not absolute: {}",
        path.display()
    );
    Ok(std::fs::canonicalize(path)?)
}

fn package_bin(package: &serde_json::Value) -> Option<&str> {
    package["bin"]
        .as_str()
        .or_else(|| package["bin"]["rebon"].as_str())
}

fn validate_npm_installation(
    executable: &Path,
    raw: &str,
    managed_bin: Option<&Path>,
    windows: bool,
) -> anyhow::Result<NpmInstallation> {
    use anyhow::{ensure, Context};
    let source: NpmLauncherSource =
        serde_json::from_str(raw).context("invalid npm launcher source")?;
    let wrapper = absolute_real_path(&source.wrapper)?;
    let platform = absolute_real_path(&source.platform)?;
    let exe = absolute_real_path(executable)?;
    ensure!(
        absolute_real_path(&source.executable)? == exe,
        "npm launcher source does not match this executable"
    );
    let node = absolute_real_path(&source.node)?;
    ensure!(
        node.is_file()
            && matches!(
                node.file_name().and_then(|s| s.to_str()),
                Some("node" | "node.exe")
            ),
        "npm launcher Node path is invalid"
    );
    let package = manifest(&wrapper.join("package.json"))?;
    ensure!(
        package["name"] == "@rebon/cli" && package_bin(&package) == Some("bin/rebon.js"),
        "npm wrapper identity/bin mismatch"
    );
    ensure!(
        wrapper.join("bin/rebon.js").is_file(),
        "npm wrapper launcher is missing"
    );
    let version = package["version"]
        .as_str()
        .context("npm wrapper version missing")?;
    semver::Version::parse(version).context("invalid npm wrapper version")?;
    let platform_package = manifest(&platform.join("package.json"))?;
    let name = platform_package["name"]
        .as_str()
        .context("npm platform name missing")?;
    ensure!(
        matches!(
            name,
            "@rebon/cli-win32-x64"
                | "@rebon/cli-darwin-x64"
                | "@rebon/cli-darwin-arm64"
                | "@rebon/cli-linux-x64"
                | "@rebon/cli-linux-arm64"
        ),
        "unrecognized npm platform package"
    );
    ensure!(
        name.starts_with("@rebon/cli-win32-") == windows,
        "npm platform does not match operating system"
    );
    ensure!(
        platform_package["version"] == version && package["optionalDependencies"][name] == version,
        "npm wrapper/platform version mismatch"
    );
    let modules = wrapper
        .parent()
        .and_then(Path::parent)
        .context("invalid npm wrapper layout")?;
    ensure!(
        modules.file_name().is_some_and(|n| n == "node_modules")
            && wrapper == modules.join("@rebon/cli"),
        "npm wrapper is not installed in node_modules (linked installs are unsupported)"
    );
    let hoisted = modules.join(name);
    let nested = wrapper.join("node_modules").join(name);
    ensure!(
        [hoisted, nested].iter().any(|p| p == &platform),
        "npm platform is unrelated to wrapper"
    );
    let payload = platform.join(if windows {
        "payload/rebon.exe"
    } else {
        "bin/rebon"
    });
    ensure!(
        package_bin(&platform_package) == Some(if windows { "bin/rebon.js" } else { "bin/rebon" }),
        "npm platform bin mismatch"
    );
    let payload = absolute_real_path(&payload)?;
    ensure!(
        payload.starts_with(&platform),
        "npm payload resolves outside its platform package"
    );
    if exe != payload {
        ensure!(
            windows,
            "running executable is outside its npm platform package"
        );
        let bin = absolute_real_path(managed_bin.context("managed npm directory unavailable")?)?;
        let installed = manifest(&bin.join("current.json"))?;
        let key = installed["versionKey"]
            .as_str()
            .context("managed version key missing")?;
        ensure!(
            !key.is_empty()
                && key
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                && key != "."
                && key != "..",
            "invalid managed version key"
        );
        let relative = format!("versions/{key}/rebon.exe");
        ensure!(
            installed["package"] == name
                && installed["version"] == version
                && installed["path"] == relative,
            "managed npm manifest mismatch"
        );
        ensure!(
            absolute_real_path(&bin.join(relative))? == exe && exe.starts_with(&bin),
            "executable is not the managed npm binary"
        );
        let size = std::fs::metadata(&payload)?.len();
        ensure!(
            installed["payload"]["size"].as_u64() == Some(size)
                && std::fs::metadata(&exe)?.len() == size,
            "managed npm payload mismatch"
        );
    }

    // Local direct dependencies take precedence over global-looking directory
    // names. Never promote a transitive dependency or a local install to -g.
    let owner = modules.parent().context("npm prefix missing")?;
    ensure!(!owner.components().any(|c| c.as_os_str() == "node_modules"), "nested/transitive npm wrappers cannot update themselves; update the owning package instead");
    let owner_manifest = owner.join("package.json");
    let (prefix, global) = if owner_manifest.exists() {
        let project = manifest(&owner_manifest)?;
        ensure!(["dependencies", "devDependencies", "optionalDependencies"].iter().any(|section| project[section]["@rebon/cli"].is_string()), "npm wrapper is not a direct project dependency; update it using that project's package manager");
        (owner.to_path_buf(), false)
    } else {
        let prefix = if windows {
            owner.to_path_buf()
        } else {
            ensure!(
                owner.file_name().is_some_and(|n| n == "lib"),
                "cannot confirm npm global prefix"
            );
            owner
                .parent()
                .context("npm global prefix missing")?
                .to_path_buf()
        };
        let launcher = wrapper.join("bin/rebon.js");
        let valid_shim = if windows {
            std::fs::read_to_string(prefix.join("rebon.cmd")).is_ok_and(|s| {
                s.replace('\\', "/")
                    .contains("%dp0%/node_modules/@rebon/cli/bin/rebon.js")
            })
        } else {
            std::fs::canonicalize(prefix.join("bin/rebon")).ok()
                == std::fs::canonicalize(launcher).ok()
        };
        ensure!(
            valid_shim,
            "cannot confirm npm global launcher/prefix; refusing a global fallback"
        );
        (prefix, true)
    };
    Ok(NpmInstallation {
        wrapper,
        prefix,
        node,
        global,
        platform_package: name.to_string(),
    })
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
    format_update_summary(notice, &status)
}

fn format_update_summary(notice: Option<&UpdateNoticeState>, info: &UpdateStatusInfo) -> String {
    let mut lines = match notice {
        Some(notice) => vec![
            format!(
                "Rebon {} -> {}",
                notice.current_version, notice.latest_version
            ),
            "Update available.".to_string(),
        ],
        None => vec![
            format!("Rebon {}", env!("CARGO_PKG_VERSION")),
            "No update notice. Run /update check to check without installing.".to_string(),
        ],
    };
    lines.extend([
        String::new(),
        format!(
            "Channel: {} | Installation: {}",
            info.channel,
            info.installation.source_label()
        ),
        format!(
            "Update checks: {}",
            if info.disabled { "disabled" } else { "enabled" }
        ),
        format!(
            "Automatic installation: unavailable (preference {}).",
            if info.auto_install { "on" } else { "off" }
        ),
        String::new(),
        update_action(&info.installation).to_string(),
    ]);
    lines.join("\n")
}

pub(crate) fn update_action(installation: &InstallationDetection) -> &'static str {
    use crate::updater::InstallationType;
    match installation.installation_type {
        InstallationType::NpmGlobal | InstallationType::NpmLocal => {
            "Run /update to check and install. Restart Rebon after updating."
        }
        InstallationType::Native => {
            "Use the original installer to update this standalone installation."
        }
        InstallationType::Development => "Update and rebuild your development checkout.",
        InstallationType::PackageManager => "Use your package manager to update Rebon.",
        InstallationType::Unknown => {
            "Installation source could not be verified. Use the original installer to update."
        }
    }
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
        "Automatic installation: unavailable (preference {}).\nInstallation: {}.\n{}",
        if prefs.auto_install { "on" } else { "off" },
        installation.source_label(),
        update_action(installation)
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

    fn write_json(path: &Path, value: serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value.to_string()).unwrap();
    }

    fn npm_fixture(root: &Path, global: bool) -> (PathBuf, String, PathBuf) {
        let prefix = root.join("prefix with spaces");
        let wrapper = prefix.join("node_modules/@rebon/cli");
        let platform = prefix.join("node_modules/@rebon/cli-win32-x64");
        write_json(
            &wrapper.join("package.json"),
            serde_json::json!({
                "name": "@rebon/cli", "version": "1.2.3", "bin": {"rebon": "bin/rebon.js"},
                "optionalDependencies": {"@rebon/cli-win32-x64": "1.2.3"}
            }),
        );
        std::fs::create_dir_all(wrapper.join("bin")).unwrap();
        std::fs::write(wrapper.join("bin/rebon.js"), "launcher").unwrap();
        write_json(
            &platform.join("package.json"),
            serde_json::json!({
                "name": "@rebon/cli-win32-x64", "version": "1.2.3", "bin": {"rebon": "bin/rebon.js"}
            }),
        );
        std::fs::create_dir_all(platform.join("payload")).unwrap();
        std::fs::write(platform.join("payload/rebon.exe"), "binary").unwrap();
        if global {
            std::fs::write(
                prefix.join("rebon.cmd"),
                "\"%dp0%\\node_modules\\@rebon\\cli\\bin\\rebon.js\"",
            )
            .unwrap();
        } else {
            write_json(
                &prefix.join("package.json"),
                serde_json::json!({
                    "dependencies": {"@rebon/cli": "^1.2.3"}
                }),
            );
        }
        let managed = root.join("local/rebon/bin");
        let exe = managed.join("versions/1.2.3/rebon.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "binary").unwrap();
        write_json(
            &managed.join("current.json"),
            serde_json::json!({
                "package": "@rebon/cli-win32-x64", "version": "1.2.3", "versionKey": "1.2.3",
                "path": "versions/1.2.3/rebon.exe", "payload": {"size": 6}
            }),
        );
        let node = root.join("node.exe");
        std::fs::write(&node, "node").unwrap();
        let raw = serde_json::json!({ "wrapper": wrapper, "platform": platform, "executable": exe, "node": node }).to_string();
        (exe, raw, managed)
    }

    #[test]
    fn managed_npm_provenance_preserves_global_and_local_scope() {
        for global in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (exe, raw, managed) = npm_fixture(root.path(), global);
            let install = validate_npm_installation(&exe, &raw, Some(&managed), true).unwrap();
            assert_eq!(install.global, global);
            assert_eq!(
                install.prefix,
                std::fs::canonicalize(root.path().join("prefix with spaces")).unwrap()
            );
            assert_eq!(
                install.detection().source_label(),
                if global { "npm-global" } else { "npm-local" }
            );
            assert!(!install.detection().auto_install_supported());
        }
    }

    #[test]
    fn npm_source_rejects_forged_or_stale_metadata() {
        let root = tempfile::tempdir().unwrap();
        let (exe, raw, managed) = npm_fixture(root.path(), true);
        assert!(
            validate_npm_installation(&exe, r#"{"source":"npm"}"#, Some(&managed), true).is_err()
        );
        let native = root.path().join("rebon.exe");
        std::fs::write(&native, "native").unwrap();
        let mut source: serde_json::Value = serde_json::from_str(&raw).unwrap();
        source["executable"] = serde_json::json!(native);
        assert!(
            validate_npm_installation(&native, &source.to_string(), Some(&managed), true).is_err()
        );
        let package = root
            .path()
            .join("prefix with spaces/node_modules/@rebon/cli/package.json");
        write_json(
            &package,
            serde_json::json!({"name": "evil", "version": "1.2.3"}),
        );
        assert!(validate_npm_installation(&exe, &raw, Some(&managed), true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_npm_binary_preserves_local_and_custom_global_prefix() {
        for global in [false, true] {
            for name in ["@rebon/cli-linux-x64", "@rebon/cli-darwin-arm64"] {
                let root = tempfile::tempdir().unwrap();
                let (_, raw, _) = npm_fixture(root.path(), false);
                let mut source: serde_json::Value = serde_json::from_str(&raw).unwrap();
                let prefix = root.path().join("prefix with spaces");
                let modules = if global {
                    std::fs::create_dir_all(prefix.join("lib")).unwrap();
                    std::fs::rename(prefix.join("node_modules"), prefix.join("lib/node_modules"))
                        .unwrap();
                    std::fs::remove_file(prefix.join("package.json")).unwrap();
                    prefix.join("lib/node_modules")
                } else {
                    prefix.join("node_modules")
                };
                let wrapper = modules.join("@rebon/cli");
                let platform = modules.join(name);
                std::fs::rename(modules.join("@rebon/cli-win32-x64"), &platform).unwrap();
                write_json(
                    &wrapper.join("package.json"),
                    serde_json::json!({
                        "name": "@rebon/cli", "version": "1.2.3", "bin": {"rebon":"bin/rebon.js"},
                        "optionalDependencies": { (name): "1.2.3" }
                    }),
                );
                write_json(
                    &platform.join("package.json"),
                    serde_json::json!({
                        "name": name, "version": "1.2.3", "bin": {"rebon":"bin/rebon"}
                    }),
                );
                let exe = platform.join("bin/rebon");
                std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
                std::fs::write(&exe, "binary").unwrap();
                if global {
                    std::fs::create_dir_all(prefix.join("bin")).unwrap();
                    std::os::unix::fs::symlink(
                        wrapper.join("bin/rebon.js"),
                        prefix.join("bin/rebon"),
                    )
                    .unwrap();
                }
                source["wrapper"] = serde_json::json!(wrapper);
                source["platform"] = serde_json::json!(platform);
                source["executable"] = serde_json::json!(exe);
                let install =
                    validate_npm_installation(&exe, &source.to_string(), None, false).unwrap();
                assert_eq!(install.global, global);
                assert_eq!(install.prefix, std::fs::canonicalize(prefix).unwrap());
                let outside = root.path().join("development-rebon");
                std::fs::write(&outside, "binary").unwrap();
                std::fs::remove_file(&exe).unwrap();
                std::os::unix::fs::symlink(outside, &exe).unwrap();
                assert!(validate_npm_installation(&exe, &source.to_string(), None, false).is_err());
            }
        }
    }

    #[test]
    fn unrelated_platform_and_stale_managed_manifest_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let (exe, raw, managed) = npm_fixture(root.path(), true);
        let mut source: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let old_platform = PathBuf::from(source["platform"].as_str().unwrap());
        let unrelated = root.path().join("other-platform");
        std::fs::rename(&old_platform, &unrelated).unwrap();
        source["platform"] = serde_json::json!(unrelated);
        assert!(
            validate_npm_installation(&exe, &source.to_string(), Some(&managed), true).is_err()
        );
        std::fs::rename(&unrelated, old_platform).unwrap();
        write_json(
            &managed.join("current.json"),
            serde_json::json!({
                "package":"@rebon/cli-win32-x64", "version":"9.0.0", "versionKey":"../escape", "path":"versions/../escape/rebon.exe"
            }),
        );
        assert!(validate_npm_installation(&exe, &raw, Some(&managed), true).is_err());
    }

    #[test]
    fn unknown_scope_and_transitive_local_install_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let (exe, raw, managed) = npm_fixture(root.path(), false);
        write_json(
            &root.path().join("prefix with spaces/package.json"),
            serde_json::json!({"dependencies": {"other": "1"}}),
        );
        assert!(validate_npm_installation(&exe, &raw, Some(&managed), true).is_err());
    }

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
        assert!(status.contains("Automatic installation: unavailable (preference on)"));
        assert!(status.contains("Installation: npm-global"));
        assert!(status.contains("Run /update to check and install"));
        assert!(!status.contains("service install"));
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

        let status = format_update_summary(
            Some(&notice),
            &UpdateStatusInfo::from_preferences(&prefs, detection()),
        );

        assert_eq!(status, "Rebon 1.0.0 -> 1.1.0\nUpdate available.\n\nChannel: latest | Installation: npm-global\nUpdate checks: enabled\nAutomatic installation: unavailable (preference on).\n\nRun /update to check and install. Restart Rebon after updating.");
        assert!(!status.contains("installationEvidence"));
        assert!(!status.contains("dismissedVersion"));
    }

    #[test]
    fn no_notice_does_not_claim_the_installation_is_current() {
        let mut preferences = prefs();
        preferences.disabled = true;
        preferences.auto_install = false;
        let status = format_update_summary(
            None,
            &UpdateStatusInfo::from_preferences(&preferences, detection()),
        );
        assert!(status.contains("No update notice"));
        assert!(status.contains("/update check"));
        assert!(status.contains("Update checks: disabled"));
        assert!(status.contains("preference off"));
        assert!(!status.contains("up to date"));
        assert!(!status.contains("1.2.3"));
    }

    #[test]
    fn only_npm_installations_offer_the_install_command() {
        for installation_type in [
            InstallationType::NpmGlobal,
            InstallationType::NpmLocal,
            InstallationType::Native,
            InstallationType::Development,
            InstallationType::PackageManager,
            InstallationType::Unknown,
        ] {
            let installation = InstallationDetection {
                installation_type,
                evidence: "test installation".to_string(),
            };
            assert_eq!(
                update_action(&installation).contains("Run /update to check and install"),
                matches!(
                    installation_type,
                    InstallationType::NpmGlobal | InstallationType::NpmLocal
                ),
            );
            assert!(!update_action(&installation).contains("npm install -g"));
        }
    }
}
