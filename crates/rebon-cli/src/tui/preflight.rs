//! Startup preflight checks that run before the main TUI session.

use std::path::{Path, PathBuf};

use rebon_dialog::invalid_settings::ValidationErrorInput;
use rebon_dialog::mcp_server_approval::McpServerApprovalAction;
use rebon_dialog::mcp_server_multiselect::McpServerMultiselectAction;
use serde::Deserialize;

// The settings files themselves are read and written one layer down, where
// the desktop app and ACP can reach them too (`project_settings`). What is
// left here is the preflight question, not the file.
use crate::project_settings::{
    load_local_settings_object, merge_string_list, write_local_settings_object,
};
use crate::ui_config::{
    InlineUiSettings, LocalSettingsSnapshot, MathRenderingMode, SettingsScan, StatusLineConfig,
    StatusLineKind, UiMode, UiSettingsSource, STATUS_LINE_MAX_PADDING,
};

#[derive(Debug, Deserialize)]
struct ProjectMcpFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: std::collections::BTreeMap<String, serde_json::Value>,
}

pub fn scan_settings_with_overrides(
    config_dir: &Path,
    cwd: &Path,
    setting_overrides: &[String],
) -> SettingsScan {
    let mut scan = SettingsScan::default();
    let files = settings_files(config_dir, cwd);
    for file in files {
        let Some(result) = parse_settings_file(&file) else {
            continue;
        };
        match result {
            Ok(value) => match file.kind {
                SettingsKind::User => scan.user_ui = value.ui,
                SettingsKind::Project => scan.project_ui = value.ui,
                SettingsKind::Local => scan.local_settings = value.local,
                SettingsKind::Flag => {
                    scan.flag_ui = merge_ui_settings(scan.flag_ui.take(), value.ui)
                }
            },
            Err(errors) => scan.errors.extend(errors),
        }
    }
    for (index, raw) in setting_overrides.iter().enumerate() {
        let Some(result) = parse_flag_settings(cwd, raw, index) else {
            continue;
        };
        match result {
            Ok(value) => scan.flag_ui = merge_ui_settings(scan.flag_ui.take(), value.ui),
            Err(errors) => scan.errors.extend(errors),
        }
    }
    scan
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsKind {
    User,
    Project,
    Local,
    Flag,
}

#[derive(Debug, Clone)]
struct SettingsFile {
    kind: SettingsKind,
    path: PathBuf,
}

/// The settings chain as `rebon_config` defines it, tagged with the layer
/// each file is. One definition of "which files count" for the whole process:
/// the sandbox plugin, `/doctor`, the plugin switches and this scan all read
/// the same three paths, cowork mode included.
fn settings_files(config_dir: &Path, cwd: &Path) -> Vec<SettingsFile> {
    crate::rebon_config::settings_files(config_dir, cwd)
        .into_iter()
        .map(|(layer, path)| SettingsFile {
            kind: match layer {
                "user" => SettingsKind::User,
                "project" => SettingsKind::Project,
                _ => SettingsKind::Local,
            },
            path,
        })
        .collect()
}

pub fn pending_project_mcp_servers(
    cwd: &Path,
    local_settings: &LocalSettingsSnapshot,
) -> Vec<String> {
    let file_path = cwd.join(".mcp.json");
    let Ok(bytes) = std::fs::read(&file_path) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_slice::<ProjectMcpFile>(&bytes) else {
        return Vec::new();
    };
    parsed
        .mcp_servers
        .into_keys()
        .filter(|name| project_mcp_server_status(name, local_settings) == McpProjectStatus::Pending)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpProjectStatus {
    Approved,
    Rejected,
    Pending,
}

fn project_mcp_server_status(
    server_name: &str,
    local_settings: &LocalSettingsSnapshot,
) -> McpProjectStatus {
    if local_settings
        .disabled_mcpjson_servers
        .iter()
        .any(|name| name == server_name)
    {
        return McpProjectStatus::Rejected;
    }
    if local_settings.enable_all_project_mcp_servers
        || local_settings
            .enabled_mcpjson_servers
            .iter()
            .any(|name| name == server_name)
    {
        return McpProjectStatus::Approved;
    }
    McpProjectStatus::Pending
}

pub fn apply_mcp_approval_action(
    cwd: &Path,
    action: McpServerApprovalAction,
) -> anyhow::Result<()> {
    let mut settings = load_local_settings_object(cwd)?;
    match action {
        McpServerApprovalAction::Approve {
            server_name,
            enable_all,
        } => {
            merge_string_list(&mut settings, "enabledMcpjsonServers", &[server_name]);
            if enable_all {
                settings.insert(
                    "enableAllProjectMcpServers".into(),
                    serde_json::Value::Bool(true),
                );
            }
        }
        McpServerApprovalAction::Reject { server_name } => {
            merge_string_list(&mut settings, "disabledMcpjsonServers", &[server_name]);
        }
    }
    write_local_settings_object(cwd, &settings)
}

pub fn apply_mcp_multiselect_action(
    cwd: &Path,
    action: McpServerMultiselectAction,
) -> anyhow::Result<()> {
    let mut settings = load_local_settings_object(cwd)?;
    match action {
        McpServerMultiselectAction::Apply {
            approved, rejected, ..
        } => {
            if !approved.is_empty() {
                merge_string_list(&mut settings, "enabledMcpjsonServers", &approved);
            }
            if !rejected.is_empty() {
                merge_string_list(&mut settings, "disabledMcpjsonServers", &rejected);
            }
        }
    }
    write_local_settings_object(cwd, &settings)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ParsedSettingsSnapshot {
    local: LocalSettingsSnapshot,
    ui: Option<UiSettingsSource>,
}

fn merge_ui_settings(
    base: Option<UiSettingsSource>,
    next: Option<UiSettingsSource>,
) -> Option<UiSettingsSource> {
    match (base, next) {
        (None, None) => None,
        (Some(base), None) => Some(base),
        (None, Some(next)) => Some(next),
        (Some(mut base), Some(next)) => {
            if next.ui_mode.is_some() {
                base.ui_mode = next.ui_mode;
            }
            if next.math_rendering.is_some() {
                base.math_rendering = next.math_rendering;
            }
            if next.inline.is_some() {
                base.inline = next.inline;
            }
            if next.status_line.is_some() {
                base.status_line = next.status_line;
            }
            Some(base)
        }
    }
}

fn parse_flag_settings(
    cwd: &Path,
    raw: &str,
    index: usize,
) -> Option<Result<ParsedSettingsSnapshot, Vec<ValidationErrorInput>>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let display_path = format!("--settings[{}]", index + 1);
    let bytes = if trimmed.starts_with('{') {
        trimmed.as_bytes().to_vec()
    } else {
        let path = crate::rebon_config::resolve_against_cwd(cwd, trimmed);
        match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                return Some(Err(vec![ValidationErrorInput {
                    path: path.display().to_string(),
                    message: err.to_string(),
                }]));
            }
        }
    };
    Some(parse_settings_bytes(
        &display_path,
        SettingsKind::Flag,
        &bytes,
    ))
}

fn parse_settings_file(
    file: &SettingsFile,
) -> Option<Result<ParsedSettingsSnapshot, Vec<ValidationErrorInput>>> {
    let bytes = match std::fs::read(&file.path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            return Some(Err(vec![ValidationErrorInput {
                path: file.path.display().to_string(),
                message: err.to_string(),
            }]));
        }
    };

    Some(parse_settings_bytes(
        &file.path.display().to_string(),
        file.kind,
        &bytes,
    ))
}

fn parse_settings_bytes(
    path: &str,
    kind: SettingsKind,
    bytes: &[u8],
) -> Result<ParsedSettingsSnapshot, Vec<ValidationErrorInput>> {
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(err) => {
            return Err(vec![ValidationErrorInput {
                path: path.to_string(),
                message: format!("Invalid JSON: {err}"),
            }]);
        }
    };

    let Some(object) = value.as_object() else {
        return Err(vec![ValidationErrorInput {
            path: path.to_string(),
            message: String::from("Expected object, but received non-object JSON value"),
        }]);
    };

    let mut errors = Vec::new();
    let mut snapshot = LocalSettingsSnapshot::default();
    let mut ui = UiSettingsSource::default();

    if let Some(value) = object.get("enableAllProjectMcpServers") {
        match value.as_bool() {
            Some(enabled) => snapshot.enable_all_project_mcp_servers = enabled,
            None => errors.push(error_path(
                path,
                "enableAllProjectMcpServers",
                "Expected boolean",
            )),
        }
    }

    if let Some(value) = object.get("enabledMcpjsonServers") {
        match array_of_strings(value) {
            Ok(values) => snapshot.enabled_mcpjson_servers = values,
            Err(message) => errors.push(error_path(path, "enabledMcpjsonServers", message)),
        }
    }

    if let Some(value) = object.get("disabledMcpjsonServers") {
        match array_of_strings(value) {
            Ok(values) => snapshot.disabled_mcpjson_servers = values,
            Err(message) => errors.push(error_path(path, "disabledMcpjsonServers", message)),
        }
    }

    if let Some(value) = object.get("uiMode") {
        match value.as_str().and_then(|s| s.parse::<UiMode>().ok()) {
            Some(mode) if kind != SettingsKind::Local => ui.ui_mode = Some(mode),
            Some(_) => {}
            None => errors.push(error_path(path, "uiMode", "Expected `screen` or `inline`")),
        }
    }

    if let Some(value) = object.get("mathRendering") {
        match value
            .as_str()
            .and_then(|s| s.parse::<MathRenderingMode>().ok())
        {
            Some(mode) if kind != SettingsKind::Local => ui.math_rendering = Some(mode),
            Some(_) => {}
            None => errors.push(error_path(
                path,
                "mathRendering",
                "Expected `off`, `unicode`, or `graphics-auto`",
            )),
        }
    }

    if let Some(value) = object.get("inline") {
        match serde_json::from_value::<InlineUiSettings>(value.clone()) {
            Ok(inline) if kind != SettingsKind::Local => ui.inline = Some(inline),
            Ok(_) => {}
            Err(err) => errors.push(error_path(
                path,
                "inline",
                format!("Invalid inline settings: {err}"),
            )),
        }
    }

    if let Some(value) = object.get("statusLine") {
        match parse_status_line(value) {
            Ok(status_line) if kind != SettingsKind::Local => ui.status_line = Some(status_line),
            Ok(_) => {}
            Err(mut status_errors) => {
                errors.extend(status_errors.drain(..).map(|(field, message)| {
                    let key = if field.is_empty() {
                        String::from("statusLine")
                    } else {
                        format!("statusLine.{field}")
                    };
                    error_path(path, &key, message)
                }));
            }
        }
    }

    if let Some(inline) = &ui.inline {
        if !(4..=30).contains(&inline.viewport_height) {
            errors.push(error_path(
                path,
                "inline.viewportHeight",
                "Expected integer between 4 and 30",
            ));
        }
    }

    if errors.is_empty() {
        Ok(ParsedSettingsSnapshot {
            local: snapshot,
            ui: if ui.ui_mode.is_some()
                || ui.math_rendering.is_some()
                || ui.inline.is_some()
                || ui.status_line.is_some()
            {
                Some(ui)
            } else {
                None
            },
        })
    } else {
        Err(errors)
    }
}

fn parse_status_line(
    value: &serde_json::Value,
) -> Result<StatusLineConfig, Vec<(&'static str, &'static str)>> {
    let Some(object) = value.as_object() else {
        return Err(vec![("", "Expected object")]);
    };
    let mut errors = Vec::new();
    let kind = match object.get("type").and_then(serde_json::Value::as_str) {
        Some("command") => StatusLineKind::Command,
        Some("script") => StatusLineKind::Script,
        _ => {
            errors.push(("type", "Expected `command` or `script`"));
            StatusLineKind::Command
        }
    };
    let command = if kind == StatusLineKind::Command {
        match object.get("command").and_then(serde_json::Value::as_str) {
            Some(command) if !command.trim().is_empty() => command.to_string(),
            _ => {
                errors.push(("command", "Expected non-empty string"));
                String::new()
            }
        }
    } else {
        String::new()
    };
    let script = if kind == StatusLineKind::Script {
        match object.get("script").and_then(serde_json::Value::as_str) {
            Some(script) if !script.trim().is_empty() => Some(PathBuf::from(script)),
            _ => {
                errors.push(("script", "Expected non-empty path"));
                None
            }
        }
    } else {
        None
    };
    let padding = match object.get("padding") {
        Some(value) => match value.as_u64().and_then(|v| u32::try_from(v).ok()) {
            Some(value) if value <= STATUS_LINE_MAX_PADDING => Some(value),
            Some(_) => {
                errors.push(("padding", "Expected integer between 0 and 200"));
                None
            }
            None => {
                errors.push(("padding", "Expected non-negative integer fitting u32"));
                None
            }
        },
        None => None,
    };
    let refresh_interval = match object.get("refreshInterval") {
        Some(value) => match value.as_u64() {
            Some(value) if value >= 1 => Some(value),
            _ => {
                errors.push(("refreshInterval", "Expected integer of at least 1"));
                None
            }
        },
        None => None,
    };
    let hide_vim_mode_indicator = match object.get("hideVimModeIndicator") {
        Some(value) => match value.as_bool() {
            Some(value) => Some(value),
            None => {
                errors.push(("hideVimModeIndicator", "Expected boolean"));
                None
            }
        },
        None => None,
    };
    if errors.is_empty() {
        Ok(StatusLineConfig {
            kind,
            command,
            script,
            padding,
            refresh_interval,
            hide_vim_mode_indicator,
        })
    } else {
        Err(errors)
    }
}

fn array_of_strings(value: &serde_json::Value) -> Result<Vec<String>, &'static str> {
    let Some(items) = value.as_array() else {
        return Err("Expected array of strings");
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err("Expected array of strings");
        };
        out.push(text.to_string());
    }
    Ok(out)
}

fn error_path(path: &str, key: &str, message: impl Into<String>) -> ValidationErrorInput {
    ValidationErrorInput {
        path: path.to_string(),
        message: format!("{key}: {}", message.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_ignored() {
        let file = SettingsFile {
            kind: SettingsKind::User,
            path: PathBuf::from("definitely-missing-file.json"),
        };
        assert!(parse_settings_file(&file).is_none());
    }

    #[test]
    fn the_scan_reads_the_shared_settings_chain_in_layer_order() {
        let config_dir = Path::new("config");
        let cwd = Path::new("project");
        let files = settings_files(config_dir, cwd);
        let kinds: Vec<SettingsKind> = files.iter().map(|file| file.kind).collect();
        assert_eq!(
            kinds,
            [
                SettingsKind::User,
                SettingsKind::Project,
                SettingsKind::Local
            ]
        );
        assert_eq!(
            files[0].path,
            crate::rebon_config::user_settings_file(config_dir),
            "the user layer is whichever file rebon_config says it is, cowork mode included"
        );
        assert_eq!(files[1].path, cwd.join(".rebon").join("settings.json"));
        assert_eq!(
            files[2].path,
            cwd.join(".rebon").join("settings.local.json")
        );
    }

    #[test]
    fn invalid_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{").unwrap();
        let file = SettingsFile {
            kind: SettingsKind::User,
            path,
        };
        let result = parse_settings_file(&file).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn parses_all_math_rendering_values_and_rejects_invalid_value() {
        for (wire, expected) in [
            ("off", MathRenderingMode::Off),
            ("unicode", MathRenderingMode::Unicode),
            ("graphics-auto", MathRenderingMode::GraphicsAuto),
        ] {
            let bytes = format!(r#"{{"mathRendering":"{wire}"}}"#);
            let parsed =
                parse_settings_bytes("settings.json", SettingsKind::User, bytes.as_bytes())
                    .unwrap();
            assert_eq!(parsed.ui.unwrap().math_rendering, Some(expected));
        }

        let errors = parse_settings_bytes(
            "settings.json",
            SettingsKind::User,
            br#"{"mathRendering":"graphics"}"#,
        )
        .unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("mathRendering"));
        assert!(errors[0].message.contains("graphics-auto"));
    }

    #[test]
    fn local_settings_extracts_mcp_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.local.json");
        std::fs::write(
            &path,
            r#"{"enableAllProjectMcpServers":true,"enabledMcpjsonServers":["a"],"disabledMcpjsonServers":["b"]}"#,
        )
        .unwrap();
        let file = SettingsFile {
            kind: SettingsKind::Local,
            path,
        };
        let result = parse_settings_file(&file).unwrap().unwrap();
        assert!(result.local.enable_all_project_mcp_servers);
        assert_eq!(result.local.enabled_mcpjson_servers, vec!["a".to_string()]);
        assert_eq!(result.local.disabled_mcpjson_servers, vec!["b".to_string()]);
    }

    #[test]
    fn parses_status_line_settings_and_rejects_invalid_values() {
        let parsed = parse_settings_bytes(
            "settings.json",
            SettingsKind::User,
            br#"{"statusLine":{"type":"command","command":"echo ok","padding":2,"refreshInterval":1,"hideVimModeIndicator":true}}"#,
        )
        .unwrap();
        let status_line = parsed.ui.unwrap().status_line.unwrap();
        assert_eq!(status_line.command, "echo ok");
        assert!(status_line.script.is_none());
        assert_eq!(status_line.padding, Some(2));
        assert_eq!(status_line.refresh_interval, Some(1));
        assert_eq!(status_line.hide_vim_mode_indicator, Some(true));

        let errors = parse_settings_bytes(
            "settings.json",
            SettingsKind::User,
            br#"{"statusLine":{"type":"template","command":"","padding":-1,"refreshInterval":0,"hideVimModeIndicator":"yes"}}"#,
        )
        .unwrap_err();
        let messages = errors.into_iter().map(|e| e.message).collect::<Vec<_>>();
        assert!(messages.iter().any(|m| m.contains("statusLine.type")));
        assert!(messages.iter().any(|m| m.contains("statusLine.command")));
        assert!(messages.iter().any(|m| m.contains("statusLine.padding")));
        assert!(messages
            .iter()
            .any(|m| m.contains("statusLine.refreshInterval")));
        assert!(messages
            .iter()
            .any(|m| m.contains("statusLine.hideVimModeIndicator")));
    }

    #[test]
    fn parses_synchronous_status_line_script_settings() {
        let parsed = parse_settings_bytes(
            "settings.json",
            SettingsKind::User,
            br#"{"statusLine":{"type":"script","script":".rebon/statusline.js","padding":1,"refreshInterval":2}}"#,
        )
        .unwrap();
        let status_line = parsed.ui.unwrap().status_line.unwrap();
        assert_eq!(status_line.kind, StatusLineKind::Script);
        assert!(status_line.command.is_empty());
        assert_eq!(
            status_line.script.as_deref(),
            Some(Path::new(".rebon/statusline.js"))
        );
        assert_eq!(status_line.padding, Some(1));
        assert_eq!(status_line.refresh_interval, Some(2));

        let errors = parse_settings_bytes(
            "settings.json",
            SettingsKind::User,
            br#"{"statusLine":{"type":"script","script":""}}"#,
        )
        .unwrap_err();
        assert!(errors
            .iter()
            .any(|error| error.message.contains("statusLine.script")));
    }

    #[test]
    fn local_status_line_is_validated_but_ignored_like_other_ui_settings() {
        let parsed = parse_settings_bytes(
            "settings.local.json",
            SettingsKind::Local,
            br#"{"statusLine":{"type":"command","command":"echo local"}}"#,
        )
        .unwrap();
        assert!(parsed.ui.is_none());
    }

    #[test]
    fn scan_settings_merges_flag_status_line_over_project() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(cwd.join(".rebon")).unwrap();
        std::fs::write(
            cwd.join(".rebon/settings.json"),
            r#"{"statusLine":{"type":"command","command":"echo project"}}"#,
        )
        .unwrap();
        let scan = scan_settings_with_overrides(
            &config,
            &cwd,
            &[r#"{"statusLine":{"type":"command","command":"echo flag"}}"#.into()],
        );
        assert_eq!(
            scan.flag_ui.unwrap().status_line.unwrap().command,
            "echo flag"
        );
        assert_eq!(
            scan.project_ui.unwrap().status_line.unwrap().command,
            "echo project"
        );
    }
}
