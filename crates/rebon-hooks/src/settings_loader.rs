//! Load hooks from `settings.json`-style files.
//!
//! The concrete filesystem side of [`crate::grouping::HookSourceProvider`].
//! It reads the three editable settings files (user, project, local),
//! parses the `hooks` key out of each, tags every entry with the right
//! [`HookSource`], and returns a flat `Vec<IndividualHookConfig>`.
//!
//! ## Expected JSON shape
//!
//! ```json
//! {
//!   "hooks": {
//!     "PreToolUse": [
//!       {
//!         "matcher": "Bash",
//!         "hooks": [
//!           { "type": "command", "command": "echo hi", "shell": "bash", "timeout": 30 }
//!         ]
//!       }
//!     ]
//!   }
//! }
//! ```
//!
//! Any other top-level keys are ignored — this module cares only
//! about the `hooks` subobject. Invalid hook entries are skipped with
//! a [`SettingsLoadWarning`] so one bad entry can't disable the rest
//! of the file.
//!
//! ## Why no serde derive on `HookCommand`
//!
//! `HookCommand` and its four variant structs deliberately do **not**
//! derive `Serialize`/`Deserialize`. The in-memory shape wants
//! `Option<u64>` for `timeout` and `Vec<(String, String)>` for HTTP
//! headers, while the settings JSON spells headers as a map, so the two
//! shapes do not line up. The parser is hand-rolled here instead: the
//! in-memory shape stays ergonomic and the JSON contract lives in one
//! place, [`parse_settings_value`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::event::parse_hook_event;
use crate::hook_command::{
    AgentHook, BashCommandHook, HookCommand, HttpHook, PromptHook, ShellKind,
};
use crate::hook_source::HookSource;
use crate::individual_hook::IndividualHookConfig;

/// Non-fatal warning emitted while parsing a settings file. One
/// warning per offending hook — the loader keeps going so a single
/// malformed entry can't brick an otherwise valid file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsLoadWarning {
    pub source: HookSource,
    pub path: PathBuf,
    pub event_name: Option<String>,
    pub matcher: Option<String>,
    pub reason: String,
}

/// Result of loading one settings file. `hooks` is the accepted
/// entries; `warnings` is per-hook skip reasons.
#[derive(Debug, Default, Clone)]
pub struct LoadedHooks {
    pub hooks: Vec<IndividualHookConfig>,
    pub warnings: Vec<SettingsLoadWarning>,
}

impl LoadedHooks {
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty() && self.warnings.is_empty()
    }

    pub fn extend(&mut self, other: LoadedHooks) {
        self.hooks.extend(other.hooks);
        self.warnings.extend(other.warnings);
    }
}

/// Fatal error: the file could not be read or its top-level JSON is
/// unusable. Missing files are **not** fatal — they're represented as
/// `Ok(LoadedHooks::default())` so the caller can blindly call
/// `load_from_path` for all three settings locations.
#[derive(Debug, thiserror::Error)]
pub enum SettingsLoadError {
    #[error("settings file is not valid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("settings file I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Load hooks from the given file. Returns `Ok(default)` if the file
/// does not exist: a missing settings file counts as empty, not as an
/// error, and a file holding only whitespace is treated the same way.
pub fn load_from_path(path: &Path, source: HookSource) -> Result<LoadedHooks, SettingsLoadError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(LoadedHooks::default()),
        Err(err) => return Err(err.into()),
    };
    if text.trim().is_empty() {
        return Ok(LoadedHooks::default());
    }
    let value: Value = serde_json::from_str(&text)?;
    Ok(parse_settings_value(&value, source, path))
}

/// Parse an already-deserialized settings JSON value. Exposed for
/// tests and callers that obtain the JSON through another path
/// (in-memory, cached, merged, etc.).
pub fn parse_settings_value(value: &Value, source: HookSource, source_path: &Path) -> LoadedHooks {
    let mut out = LoadedHooks::default();
    let hooks_obj = match value.get("hooks").and_then(Value::as_object) {
        Some(obj) => obj,
        None => return out,
    };

    for (event_name, matcher_list) in hooks_obj {
        let event = match parse_hook_event(event_name) {
            Some(e) => e,
            None => {
                out.warnings.push(SettingsLoadWarning {
                    source,
                    path: source_path.to_path_buf(),
                    event_name: Some(event_name.clone()),
                    matcher: None,
                    reason: format!("unknown hook event `{event_name}`"),
                });
                continue;
            }
        };
        let entries = match matcher_list.as_array() {
            Some(arr) => arr,
            None => {
                out.warnings.push(SettingsLoadWarning {
                    source,
                    path: source_path.to_path_buf(),
                    event_name: Some(event_name.clone()),
                    matcher: None,
                    reason: "expected array of matcher entries".into(),
                });
                continue;
            }
        };
        for entry in entries {
            let matcher = entry
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::to_string);
            let hook_values = match entry.get("hooks").and_then(Value::as_array) {
                Some(arr) => arr,
                None => {
                    out.warnings.push(SettingsLoadWarning {
                        source,
                        path: source_path.to_path_buf(),
                        event_name: Some(event_name.clone()),
                        matcher: matcher.clone(),
                        reason: "matcher entry missing `hooks` array".into(),
                    });
                    continue;
                }
            };
            for hook in hook_values {
                match parse_hook_command(hook) {
                    Ok(config) => out.hooks.push(IndividualHookConfig {
                        event,
                        config,
                        matcher: matcher.clone(),
                        source,
                        plugin_name: None,
                    }),
                    Err(reason) => out.warnings.push(SettingsLoadWarning {
                        source,
                        path: source_path.to_path_buf(),
                        event_name: Some(event_name.clone()),
                        matcher: matcher.clone(),
                        reason,
                    }),
                }
            }
        }
    }
    out
}

fn parse_hook_command(value: &Value) -> Result<HookCommand, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "hook entry must be an object".to_string())?;
    let type_str = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "hook entry missing `type` discriminator".to_string())?;

    match type_str {
        "command" => {
            let command = required_string(obj, "command")?;
            Ok(HookCommand::Command(BashCommandHook {
                command,
                r#if: optional_string(obj, "if"),
                shell: parse_shell(obj)?,
                timeout: optional_u64(obj, "timeout")?,
                status_message: optional_string(obj, "statusMessage"),
                once: optional_bool(obj, "once"),
                r#async: optional_bool(obj, "async"),
                async_rewake: optional_bool(obj, "asyncRewake"),
            }))
        }
        "prompt" => Ok(HookCommand::Prompt(PromptHook {
            prompt: required_string(obj, "prompt")?,
            r#if: optional_string(obj, "if"),
            timeout: optional_u64(obj, "timeout")?,
            model: optional_string(obj, "model"),
            status_message: optional_string(obj, "statusMessage"),
            once: optional_bool(obj, "once"),
        })),
        "agent" => Ok(HookCommand::Agent(AgentHook {
            prompt: required_string(obj, "prompt")?,
            r#if: optional_string(obj, "if"),
            timeout: optional_u64(obj, "timeout")?,
            model: optional_string(obj, "model"),
            status_message: optional_string(obj, "statusMessage"),
            once: optional_bool(obj, "once"),
        })),
        "http" => Ok(HookCommand::Http(HttpHook {
            url: required_string(obj, "url")?,
            r#if: optional_string(obj, "if"),
            timeout: optional_u64(obj, "timeout")?,
            headers: parse_headers(obj)?,
            allowed_env_vars: optional_string_vec(obj, "allowedEnvVars")?,
            status_message: optional_string(obj, "statusMessage"),
            once: optional_bool(obj, "once"),
        })),
        other => Err(format!("unsupported hook type `{other}`")),
    }
}

fn parse_shell(obj: &serde_json::Map<String, Value>) -> Result<Option<ShellKind>, String> {
    match obj.get("shell") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => match s.as_str() {
            "bash" => Ok(Some(ShellKind::Bash)),
            "powershell" => Ok(Some(ShellKind::Powershell)),
            "node" => Ok(Some(ShellKind::Node)),
            other => Err(format!("unsupported shell `{other}`")),
        },
        Some(_) => Err("`shell` must be a string".into()),
    }
}

fn parse_headers(
    obj: &serde_json::Map<String, Value>,
) -> Result<Option<Vec<(String, String)>>, String> {
    match obj.get("headers") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => {
            let mut out = Vec::with_capacity(map.len());
            for (name, value) in map {
                let v = value
                    .as_str()
                    .ok_or_else(|| format!("`headers.{name}` must be a string"))?;
                out.push((name.clone(), v.to_string()));
            }
            Ok(Some(out))
        }
        Some(_) => Err("`headers` must be an object".into()),
    }
}

fn required_string(obj: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("`{key}` is required and must be a string"))
}

fn optional_string(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

fn optional_u64(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u64>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                Ok(Some(u))
            } else if let Some(f) = n.as_f64() {
                if f >= 0.0 && f.fract() == 0.0 {
                    Ok(Some(f as u64))
                } else {
                    Err(format!("`{key}` must be a non-negative integer"))
                }
            } else {
                Err(format!("`{key}` must be a non-negative integer"))
            }
        }
        Some(_) => Err(format!("`{key}` must be a number")),
    }
}

fn optional_bool(obj: &serde_json::Map<String, Value>, key: &str) -> Option<bool> {
    obj.get(key).and_then(Value::as_bool)
}

fn optional_string_vec(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for (i, v) in arr.iter().enumerate() {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("`{key}[{i}]` must be a string"))?;
                out.push(s.to_string());
            }
            Ok(Some(out))
        }
        Some(_) => Err(format!("`{key}` must be an array of strings")),
    }
}

/// Path-resolution helper that fans out the three canonical editable
/// settings files given a user home directory and a project working
/// directory.
///
/// The three files, in the order they are read:
///
/// | Source          | Path                                       |
/// |-----------------|--------------------------------------------|
/// | UserSettings    | `<home>/.rebon/settings.json`              |
/// | ProjectSettings | `<project>/.rebon/settings.json`           |
/// | LocalSettings   | `<project>/.rebon/settings.local.json`     |
///
/// Rebon keeps all three under `.rebon/`.
#[derive(Debug, Clone)]
pub struct SettingsPaths {
    pub user: PathBuf,
    pub project: PathBuf,
    pub local: PathBuf,
}

impl SettingsPaths {
    pub fn resolve(home_dir: &Path, project_dir: &Path) -> Self {
        Self::from_config_dir(&home_dir.join(".rebon"), project_dir)
    }

    pub fn from_config_dir(config_dir: &Path, project_dir: &Path) -> Self {
        Self {
            user: config_dir.join("settings.json"),
            project: project_dir.join(".rebon").join("settings.json"),
            local: project_dir.join(".rebon").join("settings.local.json"),
        }
    }
}

/// Load all three editable settings files at once. Missing files are
/// treated as empty. Warnings accumulate across files.
pub fn load_all_editable(paths: &SettingsPaths) -> Result<LoadedHooks, SettingsLoadError> {
    let mut merged = LoadedHooks::default();
    merged.extend(load_from_path(&paths.user, HookSource::UserSettings)?);
    merged.extend(load_from_path(&paths.project, HookSource::ProjectSettings)?);
    merged.extend(load_from_path(&paths.local, HookSource::LocalSettings)?);
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookEvent;
    use serde_json::json;

    fn p() -> &'static Path {
        Path::new("/tmp/test/settings.json")
    }

    #[test]
    fn parse_accepts_command_hook_with_all_fields() {
        let v = json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "lint.sh",
                        "shell": "bash",
                        "timeout": 30,
                        "if": "Bash(git *)",
                        "statusMessage": "Linting…",
                        "once": true,
                        "async": false,
                        "asyncRewake": false
                    }]
                }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.hooks.len(), 1);
        let hook = &loaded.hooks[0];
        assert_eq!(hook.event, HookEvent::PreToolUse);
        assert_eq!(hook.matcher.as_deref(), Some("Bash"));
        assert_eq!(hook.source, HookSource::UserSettings);
        match &hook.config {
            HookCommand::Command(c) => {
                assert_eq!(c.command, "lint.sh");
                assert_eq!(c.shell, Some(ShellKind::Bash));
                assert_eq!(c.timeout, Some(30));
                assert_eq!(c.r#if.as_deref(), Some("Bash(git *)"));
                assert_eq!(c.status_message.as_deref(), Some("Linting…"));
                assert_eq!(c.once, Some(true));
            }
            other => panic!("expected command hook, got {other:?}"),
        }
    }

    #[test]
    fn parse_accepts_node_shell_for_javascript_hooks() {
        let v = json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "hooks": [{
                        "type": "command",
                        "command": "process.stdout.write('hi')",
                        "shell": "node"
                    }]
                }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::LocalSettings, p());
        assert_eq!(loaded.hooks.len(), 1);
        match &loaded.hooks[0].config {
            HookCommand::Command(c) => assert_eq!(c.shell, Some(ShellKind::Node)),
            other => panic!("expected command hook, got {other:?}"),
        }
    }

    #[test]
    fn parse_accepts_prompt_hook() {
        let v = json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "hooks": [{
                        "type": "prompt",
                        "prompt": "Summarise",
                        "model": "opus"
                    }]
                }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());
        assert_eq!(loaded.hooks.len(), 1);
        match &loaded.hooks[0].config {
            HookCommand::Prompt(h) => {
                assert_eq!(h.prompt, "Summarise");
                assert_eq!(h.model.as_deref(), Some("opus"));
            }
            other => panic!("expected prompt, got {other:?}"),
        }
    }

    #[test]
    fn parse_accepts_http_hook_with_headers_and_allowed_envs() {
        let v = json!({
            "hooks": {
                "PostToolUse": [{
                    "hooks": [{
                        "type": "http",
                        "url": "https://example.com/hook",
                        "headers": { "X-Token": "abc" },
                        "allowedEnvVars": ["HOME", "REBON_MODE"]
                    }]
                }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::ProjectSettings, p());
        assert_eq!(loaded.hooks.len(), 1);
        match &loaded.hooks[0].config {
            HookCommand::Http(h) => {
                assert_eq!(h.url, "https://example.com/hook");
                let headers = h.headers.as_ref().unwrap();
                assert_eq!(headers.len(), 1);
                assert_eq!(headers[0], ("X-Token".into(), "abc".into()));
                assert_eq!(
                    h.allowed_env_vars.as_ref().unwrap(),
                    &vec!["HOME".to_string(), "REBON_MODE".to_string()]
                );
            }
            other => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    fn parse_accepts_agent_hook_with_model_status_and_once() {
        let v = json!({
            "hooks": {
                "SubagentStart": [{
                    "matcher": "verification",
                    "hooks": [{
                        "type": "agent",
                        "prompt": "Inspect the diff",
                        "if": "Bash(*)",
                        "timeout": 45,
                        "model": "sonnet",
                        "statusMessage": "Reviewing hooks",
                        "once": true
                    }]
                }]
            }
        });

        let loaded = parse_settings_value(&v, HookSource::ProjectSettings, p());

        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.hooks.len(), 1);
        let hook = &loaded.hooks[0];
        assert_eq!(hook.event, HookEvent::SubagentStart);
        assert_eq!(hook.matcher.as_deref(), Some("verification"));
        assert_eq!(hook.source, HookSource::ProjectSettings);
        match &hook.config {
            HookCommand::Agent(agent) => {
                assert_eq!(agent.prompt, "Inspect the diff");
                assert_eq!(agent.r#if.as_deref(), Some("Bash(*)"));
                assert_eq!(agent.timeout, Some(45));
                assert_eq!(agent.model.as_deref(), Some("sonnet"));
                assert_eq!(agent.status_message.as_deref(), Some("Reviewing hooks"));
                assert_eq!(agent.once, Some(true));
            }
            other => panic!("expected agent hook, got {other:?}"),
        }
    }

    #[test]
    fn parse_preserves_integer_float_timeout() {
        let v = json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{ "type": "command", "command": "x", "timeout": 2.0 }]
                }]
            }
        });

        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());

        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        match &loaded.hooks[0].config {
            HookCommand::Command(command) => assert_eq!(command.timeout, Some(2)),
            other => panic!("expected command hook, got {other:?}"),
        }
    }

    #[test]
    fn parse_skips_unknown_event_with_warning() {
        let v = json!({
            "hooks": {
                "NotAnEvent": [{ "hooks": [{ "type": "command", "command": "x" }] }],
                "Setup": [{ "hooks": [{ "type": "command", "command": "y" }] }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());
        assert_eq!(loaded.hooks.len(), 1);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].reason.contains("unknown hook event"));
    }

    #[test]
    fn parse_skips_unknown_hook_type_with_warning() {
        let v = json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [
                        { "type": "unknown" },
                        { "type": "command", "command": "y" }
                    ]
                }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());
        assert_eq!(loaded.hooks.len(), 1);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].reason.contains("unsupported hook type"));
    }

    #[test]
    fn parse_requires_command_field() {
        let v = json!({
            "hooks": {
                "PreToolUse": [{ "hooks": [{ "type": "command" }] }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());
        assert!(loaded.hooks.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].reason.contains("`command` is required"));
    }

    #[test]
    fn parse_skips_non_array_event_with_warning_and_continues() {
        let v = json!({
            "hooks": {
                "PreToolUse": { "not": "an array" },
                "Setup": [{ "hooks": [{ "type": "command", "command": "ok" }] }]
            }
        });

        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());

        assert_eq!(loaded.hooks.len(), 1);
        assert_eq!(loaded.warnings.len(), 1);
        assert_eq!(loaded.warnings[0].event_name.as_deref(), Some("PreToolUse"));
        assert!(loaded.warnings[0]
            .reason
            .contains("expected array of matcher entries"));
    }

    #[test]
    fn parse_skips_matcher_without_hooks_array_and_continues() {
        let v = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash" },
                    { "matcher": "Read", "hooks": [{ "type": "command", "command": "ok" }] }
                ]
            }
        });

        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());

        assert_eq!(loaded.hooks.len(), 1);
        assert_eq!(loaded.hooks[0].matcher.as_deref(), Some("Read"));
        assert_eq!(loaded.warnings.len(), 1);
        assert_eq!(loaded.warnings[0].matcher.as_deref(), Some("Bash"));
        assert!(loaded.warnings[0]
            .reason
            .contains("matcher entry missing `hooks` array"));
    }

    #[test]
    fn parse_skips_non_object_hook_entry_with_warning() {
        let v = json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": ["not an object", { "type": "command", "command": "ok" }]
                }]
            }
        });

        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());

        assert_eq!(loaded.hooks.len(), 1);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0]
            .reason
            .contains("hook entry must be an object"));
    }

    #[test]
    fn parse_rejects_malformed_http_header_value_with_context() {
        let v = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Mcp__server__tool",
                    "hooks": [{
                        "type": "http",
                        "url": "https://example.invalid/hook",
                        "headers": { "X-Bad": 1 }
                    }]
                }]
            }
        });

        let loaded = parse_settings_value(&v, HookSource::ProjectSettings, p());

        assert!(loaded.hooks.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        assert_eq!(
            loaded.warnings[0].matcher.as_deref(),
            Some("Mcp__server__tool")
        );
        assert!(loaded.warnings[0]
            .reason
            .contains("`headers.X-Bad` must be a string"));
    }

    #[test]
    fn parse_rejects_non_string_allowed_env_var_with_context() {
        let v = json!({
            "hooks": {
                "PostToolUse": [{
                    "hooks": [{
                        "type": "http",
                        "url": "https://example.invalid/hook",
                        "allowedEnvVars": ["HOME", 7]
                    }]
                }]
            }
        });

        let loaded = parse_settings_value(&v, HookSource::ProjectSettings, p());

        assert!(loaded.hooks.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0]
            .reason
            .contains("`allowedEnvVars[1]` must be a string"));
    }

    #[test]
    fn parse_rejects_timeout_string() {
        let v = json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{
                        "type": "command", "command": "x", "timeout": "1"
                    }]
                }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());
        assert!(loaded.hooks.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0]
            .reason
            .contains("`timeout` must be a number"));
    }

    #[test]
    fn parse_rejects_negative_timeout() {
        let v = json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{
                        "type": "command", "command": "x", "timeout": -1
                    }]
                }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());
        assert!(loaded.hooks.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].reason.contains("non-negative integer"));
    }

    #[test]
    fn parse_rejects_non_integer_timeout() {
        let v = json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{
                        "type": "command", "command": "x", "timeout": 1.5
                    }]
                }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());
        assert!(loaded.hooks.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].reason.contains("non-negative integer"));
    }

    #[test]
    fn parse_rejects_bad_shell() {
        let v = json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{
                        "type": "command", "command": "x", "shell": "fish"
                    }]
                }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());
        assert!(loaded.hooks.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].reason.contains("unsupported shell"));
    }

    #[test]
    fn parse_handles_missing_hooks_key() {
        let v = json!({ "other": 1 });
        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());
        assert!(loaded.is_empty());
    }

    #[test]
    fn parse_preserves_none_matcher_as_none() {
        let v = json!({
            "hooks": {
                "SessionStart": [{
                    "hooks": [{ "type": "command", "command": "x" }]
                }]
            }
        });
        let loaded = parse_settings_value(&v, HookSource::UserSettings, p());
        assert_eq!(loaded.hooks.len(), 1);
        assert!(loaded.hooks[0].matcher.is_none());
    }

    #[test]
    fn load_from_path_returns_default_for_missing_file() {
        let path = std::env::temp_dir().join("rebon-hooks-missing-this-file-should-not-exist.json");
        let loaded = load_from_path(&path, HookSource::UserSettings).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_from_path_reads_real_file() {
        let dir = tempfile::Builder::new()
            .prefix("rebon-hooks-loader-test-")
            .tempdir()
            .unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path, HookSource::ProjectSettings).unwrap();
        assert_eq!(loaded.hooks.len(), 1);
        assert_eq!(loaded.hooks[0].source, HookSource::ProjectSettings);
    }

    #[test]
    fn load_from_path_reports_invalid_json() {
        let dir = tempfile::Builder::new()
            .prefix("rebon-hooks-loader-bad-json-")
            .tempdir()
            .unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "{this is not json").unwrap();
        let err = load_from_path(&path, HookSource::UserSettings).unwrap_err();
        assert!(matches!(err, SettingsLoadError::InvalidJson(_)));
    }

    #[test]
    fn settings_paths_resolve_uses_rebon_subdir() {
        let paths = SettingsPaths::resolve(Path::new("/home/u"), Path::new("/proj"));
        assert!(
            paths.user.ends_with(".rebon/settings.json")
                || paths.user.ends_with(".rebon\\settings.json")
        );
        assert!(
            paths.project.ends_with(".rebon/settings.json")
                || paths.project.ends_with(".rebon\\settings.json")
        );
        assert!(
            paths.local.ends_with(".rebon/settings.local.json")
                || paths.local.ends_with(".rebon\\settings.local.json")
        );
    }

    #[test]
    fn load_all_editable_tags_each_file_with_matching_source() {
        let dir = tempfile::Builder::new()
            .prefix("rebon-hooks-load-all-")
            .tempdir()
            .unwrap();
        let home = dir.path().join("home");
        let proj = dir.path().join("proj");
        fs::create_dir_all(home.join(".rebon")).unwrap();
        fs::create_dir_all(proj.join(".rebon")).unwrap();

        fs::write(
            home.join(".rebon").join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"u"}]}]}}"#,
        )
        .unwrap();
        fs::write(
            proj.join(".rebon").join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"p"}]}]}}"#,
        )
        .unwrap();
        fs::write(
            proj.join(".rebon").join("settings.local.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"l"}]}]}}"#,
        )
        .unwrap();

        let paths = SettingsPaths::resolve(&home, &proj);
        let loaded = load_all_editable(&paths).unwrap();
        assert_eq!(loaded.hooks.len(), 3);

        let by_source: std::collections::HashMap<HookSource, String> = loaded
            .hooks
            .iter()
            .map(|h| match &h.config {
                HookCommand::Command(c) => (h.source, c.command.clone()),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(by_source[&HookSource::UserSettings], "u");
        assert_eq!(by_source[&HookSource::ProjectSettings], "p");
        assert_eq!(by_source[&HookSource::LocalSettings], "l");
    }

    #[test]
    fn warnings_carry_context_for_operator_diagnostics() {
        let v = json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{ "type": "command" }]
                }]
            }
        });
        let loaded = parse_settings_value(
            &v,
            HookSource::ProjectSettings,
            Path::new("/tmp/proj/.rebon/settings.json"),
        );
        assert_eq!(loaded.warnings.len(), 1);
        let w = &loaded.warnings[0];
        assert_eq!(w.source, HookSource::ProjectSettings);
        assert_eq!(w.event_name.as_deref(), Some("PreToolUse"));
        assert_eq!(w.matcher.as_deref(), Some("Bash"));
        assert!(w.path.ends_with("settings.json"));
    }
}
