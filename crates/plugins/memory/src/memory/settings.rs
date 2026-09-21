//! Auto-memory enable/disable settings.
//!
//! This module intentionally gates only the auto `MEMORY.md` feature. It does
//! not control canonical instruction discovery (`REBON.md`, `.rebon/rules`,
//! etc.), which has separate semantics.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rebon_types::env::parse_env_flag as parse_env_bool;

/// Return whether auto-memory should be enabled for `cwd`.
///
/// Precedence:
/// 1. `REBON_DISABLE_AUTO_MEMORY` when defined (`truthy` disables, `falsy`
/// enables).
/// 2. `REBON_SIMPLE` truthy disables by default.
/// 3. Top-level `autoMemoryEnabled` setting, local > project > user, where
///    the user layer is whatever `rebon-config` says it is (`settings.json`,
///    or `cowork_settings.json` under `REBON_USE_COWORK_PLUGINS`).
/// 4. Default enabled.
pub fn is_auto_memory_enabled(cwd: impl AsRef<Path>) -> bool {
    is_auto_memory_enabled_with_env(cwd.as_ref(), |name| std::env::var(name).ok())
}

fn is_auto_memory_enabled_with_env(cwd: &Path, env: impl Fn(&str) -> Option<String>) -> bool {
    if let Some(value) = env("REBON_DISABLE_AUTO_MEMORY") {
        return !parse_env_bool(&value).unwrap_or(false);
    }

    if env("REBON_SIMPLE")
        .and_then(|value| parse_env_bool(&value))
        .unwrap_or(false)
    {
        return false;
    }

    read_auto_memory_enabled_setting(cwd, &env).unwrap_or(true)
}

fn read_auto_memory_enabled_setting(
    cwd: &Path,
    env: &impl Fn(&str) -> Option<String>,
) -> Option<bool> {
    // The chain is `rebon-config`'s to spell: it is the only thing that
    // knows the user layer is `cowork_settings.json` in cowork mode. This
    // reader used to write the three paths out itself with `settings.json`
    // hard-coded, so under `REBON_USE_COWORK_PLUGINS` it read a file nothing
    // else in that mode reads. The chain lists later-overrides-earlier, and
    // the first layer that names the key wins here, hence the walk from the
    // end.
    let config_home = config_home_for_settings_with_env(env);
    let use_cowork = rebon_config::cowork_mode_from(env("REBON_USE_COWORK_PLUGINS").as_deref());
    rebon_config::settings_files_for_mode(&config_home, cwd, use_cowork)
        .into_iter()
        .rev()
        .find_map(|(_, path)| read_auto_memory_enabled_from_file(&path))
}

fn read_auto_memory_enabled_from_file(path: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get("autoMemoryEnabled")?.as_bool()
}

fn config_home_for_settings_with_env(env: &impl Fn(&str) -> Option<String>) -> PathBuf {
    rebon_session::config_home_with_env(|name| env(name).map(OsString::from)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn enabled_with(cwd: &Path, entries: &[(&str, &str)]) -> bool {
        let env = env_map(entries);
        is_auto_memory_enabled_with_env(cwd, |name| env.get(name).cloned())
    }

    #[test]
    fn auto_memory_enabled_defaults_true() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(enabled_with(tmp.path(), &[]));
    }

    #[test]
    fn auto_memory_enabled_env_truthy_disables() {
        let tmp = tempfile::tempdir().unwrap();
        for value in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(
                !enabled_with(tmp.path(), &[("REBON_DISABLE_AUTO_MEMORY", value)]),
                "value {value:?} should disable auto-memory"
            );
        }
    }

    #[test]
    fn auto_memory_enabled_env_defined_falsy_enables_even_in_simple_mode() {
        let tmp = tempfile::tempdir().unwrap();
        for value in ["0", "false", "FALSE", " no ", "off"] {
            assert!(
                enabled_with(
                    tmp.path(),
                    &[("REBON_DISABLE_AUTO_MEMORY", value), ("REBON_SIMPLE", "1")]
                ),
                "value {value:?} should enable auto-memory even in simple mode"
            );
        }
    }

    #[test]
    fn auto_memory_enabled_simple_mode_disables_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!enabled_with(tmp.path(), &[("REBON_SIMPLE", "true")]));
    }

    #[test]
    fn auto_memory_enabled_reads_user_project_local_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("project");
        let config_home = tmp.path().join("config-home");
        std::fs::create_dir_all(cwd.join(".rebon")).unwrap();
        std::fs::create_dir_all(&config_home).unwrap();

        std::fs::write(
            config_home.join("settings.json"),
            r#"{"autoMemoryEnabled":false}"#,
        )
        .unwrap();
        assert!(!enabled_with(
            &cwd,
            &[("REBON_CONFIG_DIR", config_home.to_string_lossy().as_ref())]
        ));

        std::fs::write(
            cwd.join(".rebon/settings.json"),
            r#"{"autoMemoryEnabled":true}"#,
        )
        .unwrap();
        assert!(enabled_with(
            &cwd,
            &[("REBON_CONFIG_DIR", config_home.to_string_lossy().as_ref())]
        ));

        std::fs::write(
            cwd.join(".rebon/settings.local.json"),
            r#"{"autoMemoryEnabled":false}"#,
        )
        .unwrap();
        assert!(!enabled_with(
            &cwd,
            &[("REBON_CONFIG_DIR", config_home.to_string_lossy().as_ref())]
        ));
    }

    /// In cowork mode the user layer is `cowork_settings.json`, and this
    /// reader has to follow `rebon-config` there rather than keep reading a
    /// `settings.json` nothing else in that mode reads.
    #[test]
    fn auto_memory_enabled_reads_the_cowork_user_file_in_cowork_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("project");
        let config_home = tmp.path().join("config-home");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            config_home.join("cowork_settings.json"),
            r#"{"autoMemoryEnabled":false}"#,
        )
        .unwrap();
        std::fs::write(
            config_home.join("settings.json"),
            r#"{"autoMemoryEnabled":true}"#,
        )
        .unwrap();
        let home = config_home.to_string_lossy();

        assert!(
            !enabled_with(
                &cwd,
                &[
                    ("REBON_CONFIG_DIR", home.as_ref()),
                    ("REBON_USE_COWORK_PLUGINS", "1"),
                ]
            ),
            "cowork mode must read the switch out of cowork_settings.json"
        );
        assert!(
            enabled_with(&cwd, &[("REBON_CONFIG_DIR", home.as_ref())]),
            "outside cowork mode the plain user file still answers"
        );
    }

    #[test]
    fn config_home_for_settings_honors_rebon_config_dir() {
        let configured = PathBuf::from("/tmp/rebon-config-for-settings");
        let env = env_map(&[
            ("REBON_CONFIG_DIR", configured.to_string_lossy().as_ref()),
            ("HOME", "/tmp/home"),
            ("USERPROFILE", "C:\\Users\\example"),
        ]);

        assert_eq!(
            config_home_for_settings_with_env(&|name| env.get(name).cloned()),
            configured
        );
    }

    #[test]
    fn config_home_for_settings_ignores_whitespace_rebon_config_dir() {
        let env = env_map(&[("REBON_CONFIG_DIR", "  \t "), ("HOME", "/tmp/home")]);

        assert_eq!(
            config_home_for_settings_with_env(&|name| env.get(name).cloned()),
            PathBuf::from("/tmp/home").join(".rebon")
        );
    }
}
