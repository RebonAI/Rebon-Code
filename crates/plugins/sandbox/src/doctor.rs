//! What `/doctor` says about this machine's sandbox.
//!
//! The rows are built here rather than in the terminal for the same reason
//! the wrap is: the plugin is what knows the difference between "unsupported
//! platform", "the helper is not provisioned" and "a settings file is
//! malformed", and a front end that re-derives any of those will eventually
//! disagree with the executor. Rendering stays with the front end —
//! [`DoctorLine`] carries a level, a label and a sentence, and nothing about
//! columns or colour.

use std::path::Path;

use rebon_tool::command_sandbox::{DoctorLine, SandboxDoctor};

use crate::view::platform::SandboxPlatform;

/// The whole sandbox section, plus the `sandbox.enabled` verdict the "Shell
/// commands" section needs in order to know whether `bwrap` and `socat` are
/// required on Linux.
pub fn doctor_report(cwd: &Path, settings_overrides: &[String]) -> SandboxDoctor {
    let settings =
        load_doctor_sandbox_settings(&rebon_config::config_home_dir(), cwd, settings_overrides);
    let enabled_in_settings = settings.enabled;
    SandboxDoctor {
        enabled_in_settings,
        lines: sandbox_doctor_lines(current_sandbox_platform(), settings),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorSandboxSettings {
    enabled: bool,
    lines: Vec<DoctorLine>,
}

fn load_doctor_sandbox_settings(
    config_dir: &Path,
    cwd: &Path,
    runtime_settings: &[String],
) -> DoctorSandboxSettings {
    let mut enabled = None;
    let mut lines = Vec::new();
    for (label, path) in rebon_config::settings_files(config_dir, cwd) {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                lines.push(DoctorLine::warning(
                    format!("settings {label}"),
                    format!("failed to read {}: {err}", path.display()),
                ));
                continue;
            }
        };
        update_sandbox_enabled_from_settings_bytes(
            &mut enabled,
            &mut lines,
            &format!("settings {label}"),
            &bytes,
        );
    }

    for (index, raw) in runtime_settings.iter().enumerate() {
        let label = format!("--settings[{}]", index + 1);
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('{') {
            update_sandbox_enabled_from_settings_bytes(
                &mut enabled,
                &mut lines,
                &label,
                trimmed.as_bytes(),
            );
        } else {
            let path = rebon_config::resolve_against_cwd(cwd, trimmed);
            match std::fs::read(&path) {
                Ok(bytes) => update_sandbox_enabled_from_settings_bytes(
                    &mut enabled,
                    &mut lines,
                    &label,
                    &bytes,
                ),
                Err(err) => lines.push(DoctorLine::warning(
                    label,
                    format!("failed to read {}: {err}", path.display()),
                )),
            }
        }
    }

    let enabled = enabled.unwrap_or(false);
    lines.push(DoctorLine::ok(
        "sandbox.enabled setting",
        if enabled { "true" } else { "false" },
    ));
    DoctorSandboxSettings { enabled, lines }
}

fn update_sandbox_enabled_from_settings_bytes(
    enabled: &mut Option<bool>,
    lines: &mut Vec<DoctorLine>,
    label: &str,
    bytes: &[u8],
) {
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(err) => {
            lines.push(DoctorLine::warning(
                label.to_string(),
                format!("settings are not valid JSON: {err}"),
            ));
            return;
        }
    };
    let Some(object) = value.as_object() else {
        lines.push(DoctorLine::warning(
            label.to_string(),
            "settings payload is not an object",
        ));
        return;
    };
    let Some(sandbox) = object.get("sandbox") else {
        return;
    };
    let Some(sandbox_object) = sandbox.as_object() else {
        lines.push(DoctorLine::warning(
            label.to_string(),
            "sandbox setting is not an object",
        ));
        return;
    };
    if let Some(raw_enabled) = sandbox_object.get("enabled") {
        match raw_enabled.as_bool() {
            Some(value) => *enabled = Some(value),
            None => lines.push(DoctorLine::warning(
                label.to_string(),
                "sandbox.enabled is not a boolean",
            )),
        }
    }
}

fn sandbox_doctor_lines(
    platform: SandboxPlatform,
    settings: DoctorSandboxSettings,
) -> Vec<DoctorLine> {
    let mut lines = settings.lines;
    lines.push(DoctorLine::ok("platform", platform.as_wire()));
    // `has_backend`, not `SandboxPlatform::is_supported()`. The two answer
    // different questions and this one used the wrong one: `is_supported` is
    // the UI's list of platforms it *offers*, which excludes Windows, so
    // every Windows machine was told "unsupported platform" and the helper's
    // remediation — computed, stored, and worded for exactly this screen —
    // was displayed nowhere.
    if !crate::runtime::has_backend(platform) {
        lines.push(DoctorLine::warning(
            "status",
            "unsupported platform; sandbox will not run here",
        ));
        return lines;
    }
    if !settings.enabled {
        lines.push(DoctorLine::ok(
            "status",
            "disabled in settings; dependency enforcement is not required",
        ));
        return lines;
    }

    // The same probe the executor runs, rather than this screen's own
    // re-derivation of it. A doctor that reaches its verdict by a different
    // route than the thing it is diagnosing will eventually disagree with it,
    // and the user believes the doctor.
    // Strict is the default and the conservative reading: it is the mode
    // that refuses `REBON_SANDBOX_WIN_PATH`, so a doctor run under it reports
    // the helper the executor would actually use rather than one an
    // environment variable points at.
    let report = crate::runtime::session::probe_machine(
        crate::runtime::SandboxMode::Strict,
        settings.enabled,
    );
    let dep_check = report.dependency_check();
    for line in sandbox_helper_lines(&report) {
        lines.push(line);
    }
    let classification = crate::view::dependency::classify_errors(&dep_check);
    match crate::view::doctor::build_doctor_view(&crate::view::doctor::DoctorInputs {
        supported_platform: crate::runtime::has_backend(platform),
        sandbox_enabled_in_settings: settings.enabled,
        dep_check: dep_check.clone(),
    }) {
        Some(view) => {
            let level = if view.errors.is_empty() {
                rebon_tool::command_sandbox::DoctorLevel::Warning
            } else {
                rebon_tool::command_sandbox::DoctorLevel::Error
            };
            lines.push(DoctorLine {
                level,
                label: "status".to_string(),
                message: view.status.text().to_string(),
            });
            for error in view.errors {
                lines.push(DoctorLine::error("dependency", error));
            }
            for warning in view.warnings {
                lines.push(DoctorLine::warning("dependency", warning));
            }
            if view.show_install_hint {
                lines.push(DoctorLine::warning(
                    "fix",
                    crate::view::doctor::RUN_SANDBOX_HINT
                        .trim_start_matches('└')
                        .trim(),
                ));
            }
        }
        None => lines.push(DoctorLine::ok(
            "status",
            "available; sandbox dependency check is clean",
        )),
    }

    let mut missing = Vec::new();
    if classification.ripgrep_missing {
        missing.push("ripgrep");
    }
    if classification.bwrap_missing {
        missing.push("bwrap");
    }
    if classification.socat_missing {
        missing.push("socat");
    }
    if classification.seccomp_missing {
        missing.push("seccomp warning");
    }
    if missing.is_empty() && classification.other_errors.is_empty() {
        lines.push(DoctorLine::ok("dependency classification", "clean"));
    } else {
        lines.push(DoctorLine::warning(
            "dependency classification",
            missing.join(", "),
        ));
    }
    lines
}

/// The Windows helper's own state, as lines.
///
/// `SandboxWinStatus` has carried a `remediation()` written for exactly this
/// screen since the backend landed, and nothing ever rendered it: the caller
/// logged it once at session start, into a file the TUI never shows. A user
/// whose sandbox does not work on Windows saw "unsupported platform".
///
/// Empty off Windows, where there is no helper to have a state.
fn sandbox_helper_lines(report: &crate::runtime::SupportReport) -> Vec<DoctorLine> {
    if report.platform != SandboxPlatform::Windows {
        return Vec::new();
    }

    let mut lines = Vec::new();
    match &report.sandbox_win_path {
        Some(path) => lines.push(DoctorLine::ok("helper", path.display().to_string())),
        None => lines.push(DoctorLine::error("helper", "sandbox-win.exe was not found")),
    }
    if let Some(version) = report.sandbox_win.version {
        lines.push(DoctorLine::ok(
            "helper contract",
            format!("version {version}"),
        ));
    }
    if report.sandbox_win.is_ready() {
        lines.push(DoctorLine::ok("helper status", "installed and ready"));
    } else {
        for step in report.sandbox_win.remediation() {
            lines.push(DoctorLine::warning("helper fix", step));
        }
    }
    lines
}

pub(crate) fn current_sandbox_platform() -> SandboxPlatform {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::command_sandbox::DoctorLevel;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn line_with_label<'a>(lines: &'a [DoctorLine], label: &str) -> &'a DoctorLine {
        lines
            .iter()
            .find(|line| line.label == label)
            .unwrap_or_else(|| panic!("missing doctor line `{label}` in {lines:#?}"))
    }

    fn windows_helper_state(ready: bool, path: Option<&str>) -> crate::runtime::SupportReport {
        let status = if ready {
            crate::runtime::SandboxWinStatus {
                binary_present: true,
                version: Some(crate::runtime::windows::SUPPORTED_STATUS_VERSION),
                user_provisioned: true,
                credentials_present: true,
                wfp_installed: true,
            }
        } else {
            crate::runtime::SandboxWinStatus::default()
        };
        crate::runtime::SupportReport {
            platform: SandboxPlatform::Windows,
            sandbox_win: status,
            sandbox_win_path: path.map(PathBuf::from),
            ..Default::default()
        }
    }

    #[test]
    fn the_windows_helper_remediation_actually_reaches_the_doctor() {
        // It has existed on `SandboxWinStatus` since the backend landed and
        // was rendered nowhere: the caller logged it once at session start,
        // into a file the TUI never shows.
        let lines = sandbox_helper_lines(&windows_helper_state(false, None));

        assert!(lines
            .iter()
            .any(|line| line.label == "helper"
                && line.message.contains("sandbox-win.exe was not found")));
        assert!(
            lines.iter().any(|line| line.label == "helper fix"),
            "no remediation line: {lines:?}"
        );
    }

    #[test]
    fn a_ready_windows_helper_reports_its_path_and_contract_version() {
        let lines = sandbox_helper_lines(&windows_helper_state(
            true,
            Some(r"C:\Rebon\sandbox-win.exe"),
        ));

        assert!(lines
            .iter()
            .any(|line| line.message.contains(r"C:\Rebon\sandbox-win.exe")));
        assert!(lines
            .iter()
            .any(|line| line.label == "helper contract" && line.message.contains("version 1")));
        assert!(lines
            .iter()
            .any(|line| line.label == "helper status" && line.level == DoctorLevel::Ok));
    }

    #[test]
    fn helper_lines_are_empty_off_windows() {
        let report = crate::runtime::SupportReport {
            platform: SandboxPlatform::Linux,
            ..Default::default()
        };
        assert!(sandbox_helper_lines(&report).is_empty());
    }

    #[test]
    fn windows_is_not_reported_as_an_unsupported_platform() {
        // `SandboxPlatform::is_supported()` is the UI's list of platforms it
        // offers and excludes Windows; `has_backend` is whether confinement
        // is possible. The doctor used the first, so every Windows machine
        // was told the sandbox could not run there at all.
        assert!(crate::runtime::has_backend(SandboxPlatform::Windows));
        assert!(!SandboxPlatform::Windows.is_supported());

        let lines = sandbox_doctor_lines(
            SandboxPlatform::Windows,
            DoctorSandboxSettings {
                enabled: true,
                lines: Vec::new(),
            },
        );

        assert!(
            !lines
                .iter()
                .any(|line| line.message.contains("unsupported platform")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_disabled_sandbox_still_short_circuits() {
        let lines = sandbox_doctor_lines(
            SandboxPlatform::Windows,
            DoctorSandboxSettings {
                enabled: false,
                lines: Vec::new(),
            },
        );

        assert!(lines
            .iter()
            .any(|line| line.message.contains("disabled in settings")));
    }

    #[test]
    fn a_platform_with_no_backend_is_still_reported_as_unsupported() {
        let lines = sandbox_doctor_lines(
            SandboxPlatform::Unknown,
            DoctorSandboxSettings {
                enabled: true,
                lines: Vec::new(),
            },
        );

        assert!(lines
            .iter()
            .any(|line| line.message.contains("unsupported platform")));
    }

    #[test]
    fn sandbox_settings_loader_reads_runtime_sandbox_enabled() {
        let temp = TempDir::new().unwrap();
        let settings = load_doctor_sandbox_settings(
            temp.path(),
            temp.path(),
            &[r#"{"sandbox":{"enabled":true}}"#.to_string()],
        );

        assert!(settings.enabled);
        assert_eq!(
            line_with_label(&settings.lines, "sandbox.enabled setting").message,
            "true"
        );
    }
}
