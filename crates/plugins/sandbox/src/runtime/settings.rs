//! The `sandbox` block of `settings.json`.
//!
//! One deliberate asymmetry runs through this module: **a malformed
//! sandbox setting does not silently become a permissive default.**
//! A typo in `allowWrite` that produced an empty list would quietly
//! remove a write root the user thought they had; a typo that
//! produced `enabled: false` would turn the sandbox off entirely.
//! Both parse into an error the caller must decide about, and the
//! caller in Rebon refuses to start the session rather than start it
//! with a sandbox nobody configured.
//!
//! Unknown keys are ignored, because a settings file written by a
//! newer Rebon must not stop an older one from starting. Wrong-typed
//! *known* keys are errors, because that is a mistake rather than a
//! version skew.

use crate::runtime::config::{
    CredentialEnvRule, CredentialFileRule, FilesystemConfig, MiscConfig, NetworkConfig,
    SessionSandboxConfig,
};
use crate::runtime::confined::SandboxMode;
use serde_json::Value;
use std::path::PathBuf;

/// The parsed `sandbox` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSettings {
    /// `sandbox.enabled` — off by default.
    pub enabled: bool,
    /// `sandbox.mode` — `"strict"` or `"relaxed"`.
    pub mode: SandboxMode,
    /// Commands whose first token bypasses the sandbox entirely.
    pub excluded_commands: Vec<String>,
    /// Whether `dangerouslyDisableSandbox` on a tool call is honoured.
    ///
    /// The name is the settings-file spelling and it reads backwards
    /// from the enum it feeds: `allowUnsandboxedCommands: true` is
    /// the *less* restrictive setting.
    pub allow_unsandboxed_commands: bool,
    pub session: SessionSandboxConfig,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SandboxMode::Strict,
            excluded_commands: Vec::new(),
            allow_unsandboxed_commands: false,
            session: SessionSandboxConfig::default(),
        }
    }
}

/// A settings key that is present but wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("sandbox setting `{key}` {problem}")]
pub struct SettingsError {
    pub key: String,
    pub problem: String,
}

impl SettingsError {
    fn wrong_type(key: &str, expected: &str) -> Self {
        Self {
            key: key.to_string(),
            problem: format!("must be {expected}"),
        }
    }
}

impl SandboxSettings {
    /// Parse the `sandbox` object out of a whole settings document.
    ///
    /// A document with no `sandbox` key yields the default (sandbox
    /// off) — that is not a mistake, it is every settings file that
    /// predates the feature.
    pub fn from_settings_value(root: &Value) -> Result<Self, SettingsError> {
        let Some(sandbox) = root.get("sandbox") else {
            return Ok(Self::default());
        };
        Self::from_sandbox_value(sandbox)
    }

    /// Parse the `sandbox` object itself.
    pub fn from_sandbox_value(value: &Value) -> Result<Self, SettingsError> {
        let object = value
            .as_object()
            .ok_or_else(|| SettingsError::wrong_type("sandbox", "an object"))?;
        let mut settings = Self::default();

        if let Some(enabled) = object.get("enabled") {
            settings.enabled = enabled
                .as_bool()
                .ok_or_else(|| SettingsError::wrong_type("sandbox.enabled", "a boolean"))?;
        }
        if let Some(mode) = object.get("mode") {
            let mode = mode
                .as_str()
                .ok_or_else(|| SettingsError::wrong_type("sandbox.mode", "a string"))?;
            settings.mode = SandboxMode::from_wire(mode).ok_or_else(|| SettingsError {
                key: "sandbox.mode".into(),
                problem: format!("must be \"strict\" or \"relaxed\", got {mode:?}"),
            })?;
        }
        if let Some(excluded) = object.get("excludedCommands") {
            settings.excluded_commands = string_array("sandbox.excludedCommands", excluded)?;
        }
        if let Some(allow) = object.get("allowUnsandboxedCommands") {
            settings.allow_unsandboxed_commands = allow.as_bool().ok_or_else(|| {
                SettingsError::wrong_type("sandbox.allowUnsandboxedCommands", "a boolean")
            })?;
        }

        if let Some(filesystem) = object.get("filesystem") {
            settings.session.filesystem = parse_filesystem(filesystem)?;
        }
        if let Some(network) = object.get("network") {
            settings.session.network = parse_network(network)?;
        }
        if let Some(credentials) = object.get("credentials") {
            parse_credentials(credentials, &mut settings.session)?;
        }
        if let Some(misc) = object.get("misc") {
            settings.session.misc = parse_misc(misc)?;
        }

        Ok(settings)
    }

    /// Parse from raw bytes, e.g. a `settings.json` read off disk.
    pub fn from_settings_bytes(bytes: &[u8]) -> Result<Self, SettingsError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|err| SettingsError {
            key: "sandbox".into(),
            problem: format!("could not be read: {err}"),
        })?;
        Self::from_settings_value(&value)
    }

    /// Whether `command`'s executable is on the exclusion list.
    ///
    /// Exact match on the first whitespace-delimited token, so
    /// excluding `git` does not also exclude `gitleaks`. Substring
    /// matching here would be a way to smuggle a command past the
    /// sandbox by naming it after an excluded one.
    pub fn is_command_excluded(&self, command: &str) -> bool {
        let executable = command.trim().split_whitespace().next().unwrap_or("");
        !executable.is_empty()
            && self
                .excluded_commands
                .iter()
                .any(|excluded| executable == excluded)
    }
}

fn string_array(key: &str, value: &Value) -> Result<Vec<String>, SettingsError> {
    let array = value
        .as_array()
        .ok_or_else(|| SettingsError::wrong_type(key, "an array of strings"))?;
    array
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| SettingsError::wrong_type(key, "an array of strings"))
        })
        .collect()
}

fn path_array(key: &str, value: &Value) -> Result<Vec<PathBuf>, SettingsError> {
    Ok(string_array(key, value)?
        .into_iter()
        .map(PathBuf::from)
        .collect())
}

fn optional_bool(
    object: &serde_json::Map<String, Value>,
    key: &str,
    full: &str,
) -> Result<Option<bool>, SettingsError> {
    match object.get(key) {
        None => Ok(None),
        Some(value) => {
            Ok(Some(value.as_bool().ok_or_else(|| {
                SettingsError::wrong_type(full, "a boolean")
            })?))
        }
    }
}

fn parse_filesystem(value: &Value) -> Result<FilesystemConfig, SettingsError> {
    let object = value
        .as_object()
        .ok_or_else(|| SettingsError::wrong_type("sandbox.filesystem", "an object"))?;
    let mut config = FilesystemConfig::default();

    if let Some(disabled) = optional_bool(object, "disabled", "sandbox.filesystem.disabled")? {
        config.disabled = disabled;
    }
    if let Some(allow) = optional_bool(
        object,
        "allowGitConfig",
        "sandbox.filesystem.allowGitConfig",
    )? {
        config.allow_git_config = allow;
    }
    if let Some(value) = object.get("allowWrite") {
        config.allow_write = path_array("sandbox.filesystem.allowWrite", value)?;
    }
    if let Some(value) = object.get("denyWrite") {
        config.deny_write = path_array("sandbox.filesystem.denyWrite", value)?;
    }
    if let Some(value) = object.get("denyRead") {
        config.deny_read = path_array("sandbox.filesystem.denyRead", value)?;
    }
    if let Some(value) = object.get("allowRead") {
        config.allow_read = path_array("sandbox.filesystem.allowRead", value)?;
    }

    Ok(config)
}

fn parse_network(value: &Value) -> Result<NetworkConfig, SettingsError> {
    let object = value
        .as_object()
        .ok_or_else(|| SettingsError::wrong_type("sandbox.network", "an object"))?;
    let mut config = NetworkConfig::default();

    if let Some(value) = object.get("allowedDomains") {
        config.allowed_domains = string_array("sandbox.network.allowedDomains", value)?;
    }
    if let Some(value) = object.get("deniedDomains") {
        config.denied_domains = string_array("sandbox.network.deniedDomains", value)?;
    }
    if let Some(value) = object.get("allowUnixSockets") {
        config.allow_unix_sockets = path_array("sandbox.network.allowUnixSockets", value)?;
    }
    if let Some(value) = object.get("allowMachLookup") {
        config.allow_mach_lookup = string_array("sandbox.network.allowMachLookup", value)?;
    }
    if let Some(value) = optional_bool(
        object,
        "allowAllUnixSockets",
        "sandbox.network.allowAllUnixSockets",
    )? {
        config.allow_all_unix_sockets = value;
    }
    if let Some(value) = optional_bool(
        object,
        "allowLocalBinding",
        "sandbox.network.allowLocalBinding",
    )? {
        config.allow_local_binding = value;
    }

    Ok(config)
}

fn parse_misc(value: &Value) -> Result<MiscConfig, SettingsError> {
    let object = value
        .as_object()
        .ok_or_else(|| SettingsError::wrong_type("sandbox.misc", "an object"))?;
    let mut config = MiscConfig::default();

    if let Some(value) = optional_bool(object, "allowPty", "sandbox.misc.allowPty")? {
        config.allow_pty = value;
    }
    if let Some(value) = optional_bool(object, "allowAppleEvents", "sandbox.misc.allowAppleEvents")?
    {
        config.allow_apple_events = value;
    }
    if let Some(value) = optional_bool(
        object,
        "enableWeakerNetworkIsolation",
        "sandbox.misc.enableWeakerNetworkIsolation",
    )? {
        config.enable_weaker_network_isolation = value;
    }
    if let Some(value) = optional_bool(
        object,
        "enableWeakerNestedSandbox",
        "sandbox.misc.enableWeakerNestedSandbox",
    )? {
        config.enable_weaker_nested_sandbox = value;
    }

    Ok(config)
}

/// `credentials` is two maps of path/name → rule.
///
/// The rule is a string (`"deny"`) or an object
/// (`{"mask": "<path or value>"}`). A bare string that is not `deny`
/// is an error rather than a fallback to deny: a user who wrote
/// `"masked"` meant to mask something, and quietly denying it would
/// produce a permission error they would debug in the wrong place.
fn parse_credentials(
    value: &Value,
    session: &mut SessionSandboxConfig,
) -> Result<(), SettingsError> {
    let object = value
        .as_object()
        .ok_or_else(|| SettingsError::wrong_type("sandbox.credentials", "an object"))?;

    if let Some(files) = object.get("files") {
        let files = files
            .as_object()
            .ok_or_else(|| SettingsError::wrong_type("sandbox.credentials.files", "an object"))?;
        for (path, rule) in files {
            let key = format!("sandbox.credentials.files.{path}");
            let rule = match rule {
                Value::String(text) if text == "deny" => CredentialFileRule::Deny,
                Value::Object(map) => {
                    let fake =
                        map.get("mask")
                            .and_then(Value::as_str)
                            .ok_or_else(|| SettingsError {
                                key: key.clone(),
                                problem: "must be \"deny\" or {\"mask\": \"<path>\"}".into(),
                            })?;
                    CredentialFileRule::Mask {
                        fake: PathBuf::from(fake),
                    }
                }
                _ => {
                    return Err(SettingsError {
                        key,
                        problem: "must be \"deny\" or {\"mask\": \"<path>\"}".into(),
                    })
                }
            };
            session.credentials.files.push((PathBuf::from(path), rule));
        }
    }

    if let Some(vars) = object.get("envVars") {
        let vars = vars
            .as_object()
            .ok_or_else(|| SettingsError::wrong_type("sandbox.credentials.envVars", "an object"))?;
        for (name, rule) in vars {
            let key = format!("sandbox.credentials.envVars.{name}");
            let rule = match rule {
                Value::String(text) if text == "deny" => CredentialEnvRule::Deny,
                Value::Object(map) => {
                    let value =
                        map.get("mask")
                            .and_then(Value::as_str)
                            .ok_or_else(|| SettingsError {
                                key: key.clone(),
                                problem: "must be \"deny\" or {\"mask\": \"<value>\"}".into(),
                            })?;
                    CredentialEnvRule::Mask {
                        value: value.to_string(),
                    }
                }
                _ => {
                    return Err(SettingsError {
                        key,
                        problem: "must be \"deny\" or {\"mask\": \"<value>\"}".into(),
                    })
                }
            };
            session.credentials.env_vars.push((name.clone(), rule));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> Result<SandboxSettings, SettingsError> {
        SandboxSettings::from_settings_value(&value)
    }

    #[test]
    fn a_document_without_a_sandbox_block_is_the_default() {
        let settings = parse(json!({"theme": "dark"})).unwrap();
        assert_eq!(settings, SandboxSettings::default());
        assert!(!settings.enabled);
        assert_eq!(settings.mode, SandboxMode::Strict);
    }

    #[test]
    fn an_empty_sandbox_block_is_the_default() {
        assert_eq!(
            parse(json!({"sandbox": {}})).unwrap(),
            SandboxSettings::default()
        );
    }

    #[test]
    fn enabled_and_mode_round_trip() {
        let settings = parse(json!({"sandbox": {"enabled": true, "mode": "relaxed"}})).unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.mode, SandboxMode::Relaxed);
    }

    #[test]
    fn a_wrong_typed_enabled_is_an_error_not_a_default() {
        let error = parse(json!({"sandbox": {"enabled": "yes"}})).unwrap_err();
        assert_eq!(error.key, "sandbox.enabled");
        assert!(error.to_string().contains("must be a boolean"));
    }

    #[test]
    fn an_unknown_mode_is_rejected_by_name() {
        let error = parse(json!({"sandbox": {"mode": "open"}})).unwrap_err();
        assert_eq!(error.key, "sandbox.mode");
        assert!(error.to_string().contains("\"open\""));
    }

    #[test]
    fn a_non_object_sandbox_block_is_an_error() {
        assert_eq!(parse(json!({"sandbox": true})).unwrap_err().key, "sandbox");
    }

    #[test]
    fn unknown_keys_are_ignored_so_a_newer_file_still_loads() {
        let settings = parse(json!({
            "sandbox": {"enabled": true, "somethingFromTheFuture": {"nested": 1}}
        }))
        .unwrap();
        assert!(settings.enabled);
    }

    #[test]
    fn filesystem_lists_become_paths() {
        let settings = parse(json!({
            "sandbox": {
                "filesystem": {
                    "allowWrite": ["/work", "/tmp"],
                    "denyWrite": ["/work/vendor"],
                    "denyRead": ["/secret"],
                    "allowRead": ["/secret/public"],
                    "allowGitConfig": true,
                    "disabled": false
                }
            }
        }))
        .unwrap();
        let fs = settings.session.filesystem;

        assert_eq!(
            fs.allow_write,
            vec![PathBuf::from("/work"), PathBuf::from("/tmp")]
        );
        assert_eq!(fs.deny_write, vec![PathBuf::from("/work/vendor")]);
        assert_eq!(fs.deny_read, vec![PathBuf::from("/secret")]);
        assert_eq!(fs.allow_read, vec![PathBuf::from("/secret/public")]);
        assert!(fs.allow_git_config);
        assert!(!fs.disabled);
    }

    #[test]
    fn a_non_string_entry_in_a_path_list_is_an_error() {
        let error = parse(json!({
            "sandbox": {"filesystem": {"allowWrite": ["/work", 42]}}
        }))
        .unwrap_err();
        assert_eq!(error.key, "sandbox.filesystem.allowWrite");
    }

    #[test]
    fn network_lists_and_switches_parse() {
        let settings = parse(json!({
            "sandbox": {
                "network": {
                    "allowedDomains": ["api.example.com"],
                    "deniedDomains": ["evil.test"],
                    "allowUnixSockets": ["/run/docker.sock"],
                    "allowAllUnixSockets": false,
                    "allowLocalBinding": true,
                    "allowMachLookup": ["com.example.*"]
                }
            }
        }))
        .unwrap();
        let network = settings.session.network;

        assert_eq!(network.allowed_domains, vec!["api.example.com"]);
        assert_eq!(network.denied_domains, vec!["evil.test"]);
        assert_eq!(
            network.allow_unix_sockets,
            vec![PathBuf::from("/run/docker.sock")]
        );
        assert!(!network.allow_all_unix_sockets);
        assert!(network.allow_local_binding);
        assert_eq!(network.allow_mach_lookup, vec!["com.example.*"]);
    }

    #[test]
    fn misc_switches_parse() {
        let settings = parse(json!({
            "sandbox": {
                "misc": {
                    "allowPty": true,
                    "allowAppleEvents": true,
                    "enableWeakerNetworkIsolation": true,
                    "enableWeakerNestedSandbox": true
                }
            }
        }))
        .unwrap();
        let misc = settings.session.misc;

        assert!(misc.allow_pty);
        assert!(misc.allow_apple_events);
        assert!(misc.enable_weaker_network_isolation);
        assert!(misc.enable_weaker_nested_sandbox);
    }

    #[test]
    fn credential_files_parse_both_rule_shapes() {
        let settings = parse(json!({
            "sandbox": {
                "credentials": {
                    "files": {
                        "/home/u/.netrc": "deny",
                        "/home/u/.npmrc": {"mask": "/tmp/fake-npmrc"}
                    }
                }
            }
        }))
        .unwrap();
        let files = settings.session.credentials.files;

        assert!(files.contains(&(PathBuf::from("/home/u/.netrc"), CredentialFileRule::Deny)));
        assert!(files.contains(&(
            PathBuf::from("/home/u/.npmrc"),
            CredentialFileRule::Mask {
                fake: PathBuf::from("/tmp/fake-npmrc")
            }
        )));
    }

    #[test]
    fn credential_env_vars_parse_both_rule_shapes() {
        let settings = parse(json!({
            "sandbox": {
                "credentials": {
                    "envVars": {
                        "AWS_SECRET_ACCESS_KEY": "deny",
                        "GITHUB_TOKEN": {"mask": "redacted"}
                    }
                }
            }
        }))
        .unwrap();
        let vars = settings.session.credentials.env_vars;

        assert!(vars.contains(&("AWS_SECRET_ACCESS_KEY".to_string(), CredentialEnvRule::Deny)));
        assert!(vars.contains(&(
            "GITHUB_TOKEN".to_string(),
            CredentialEnvRule::Mask {
                value: "redacted".to_string()
            }
        )));
    }

    #[test]
    fn a_misspelled_credential_rule_is_an_error_not_a_deny() {
        let error = parse(json!({
            "sandbox": {"credentials": {"files": {"/x": "masked"}}}
        }))
        .unwrap_err();
        assert_eq!(error.key, "sandbox.credentials.files./x");
        assert!(error.to_string().contains("must be"));
    }

    #[test]
    fn a_mask_object_without_a_mask_key_is_an_error() {
        let error = parse(json!({
            "sandbox": {"credentials": {"envVars": {"T": {"redact": "x"}}}}
        }))
        .unwrap_err();
        assert_eq!(error.key, "sandbox.credentials.envVars.T");
    }

    #[test]
    fn excluded_commands_match_the_executable_exactly() {
        let settings = parse(json!({
            "sandbox": {"excludedCommands": ["git", "npm"]}
        }))
        .unwrap();

        assert!(settings.is_command_excluded("git status"));
        assert!(settings.is_command_excluded("  npm  install "));
        assert!(!settings.is_command_excluded("gitleaks detect"));
        assert!(!settings.is_command_excluded("cargo build"));
        assert!(!settings.is_command_excluded(""));
    }

    #[test]
    fn allow_unsandboxed_commands_defaults_to_the_restrictive_value() {
        assert!(!SandboxSettings::default().allow_unsandboxed_commands);
        let settings = parse(json!({
            "sandbox": {"allowUnsandboxedCommands": true}
        }))
        .unwrap();
        assert!(settings.allow_unsandboxed_commands);
    }

    #[test]
    fn bytes_parsing_reports_malformed_json_rather_than_defaulting() {
        let error = SandboxSettings::from_settings_bytes(b"{not json").unwrap_err();
        assert_eq!(error.key, "sandbox");
        assert!(error.to_string().contains("could not be read"));
    }

    #[test]
    fn bytes_parsing_accepts_a_real_document() {
        let settings =
            SandboxSettings::from_settings_bytes(br#"{"sandbox":{"enabled":true}}"#).unwrap();
        assert!(settings.enabled);
    }
}
