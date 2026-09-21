//! The one resolution of "where does Rebon keep its data".
//!
//! Resolution order:
//! 1. `REBON_CONFIG_DIR`, exactly as provided, when it trims to non-empty.
//! 2. `REBON_CONFIG_HOME`, likewise — the alias the desktop app has always
//!    written.
//! 3. The platform home directory plus `.rebon`.
//!
//! One resolution, because splitting it is what loses data silently: while
//! different surfaces read different variables, pointing `REBON_CONFIG_DIR`
//! at another disk moved one of them and left the rest behind. The
//! platform-home preference (which differs between Windows and Unix, and
//! matters under Git Bash where `HOME` and `USERPROFILE` disagree) is decided
//! in exactly one place for the same reason.

use std::ffi::OsString;
use std::path::PathBuf;

/// Every environment variable that names the config home, in precedence order.
pub const CONFIG_HOME_VARS: [&str; 2] = ["REBON_CONFIG_DIR", "REBON_CONFIG_HOME"];

/// Directory name under the platform home when nothing is configured.
pub const DEFAULT_CONFIG_DIR_NAME: &str = ".rebon";

/// Resolve the config home from the process environment, falling back to
/// `./.rebon` when there is no home directory to anchor to.
///
/// The fallback still produces a usable path: callers hit ENOENT on lookup,
/// which reads as "not found" exactly like any other missing file.
pub fn default_config_home_dir() -> PathBuf {
    config_home_with_env(|name| std::env::var_os(name)).unwrap_or_else(|| PathBuf::from(".rebon"))
}

/// Resolve the config home from an injected environment reader.
///
/// `None` means no override was set *and* no platform home could be found.
pub fn config_home_with_env(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    config_home_override_with_env(&env)
        .or_else(|| platform_home_with_env(env).map(|home| home.join(DEFAULT_CONFIG_DIR_NAME)))
}

/// The explicitly configured config home, or `None` when no override is set.
///
/// Separate from [`config_home_with_env`], which adds the platform-home
/// fallback on top. Blank and whitespace-only values do not count as set —
/// an empty variable is how a shell spells "unset" by accident, and honoring
/// it would put the config home at the filesystem root.
pub fn config_home_override_with_env(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    CONFIG_HOME_VARS
        .into_iter()
        .filter_map(env)
        .find(|value| !value.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
}

/// The platform home directory of the running process, or `None` when neither
/// variable names one.
///
/// This is the `~` every Rebon path expands: [`default_config_home_dir`]
/// anchors on it, `~/` in a config value means it, and a memory notification
/// abbreviates against it. Read it here rather than from `HOME` directly so
/// Git Bash on Windows, where `HOME` and `USERPROFILE` disagree, resolves the
/// same directory everywhere.
pub fn platform_home_dir() -> Option<PathBuf> {
    platform_home_with_env(|name| std::env::var_os(name))
}

/// Resolve the platform home directory from an injected environment reader.
///
/// Windows prefers non-empty `USERPROFILE` with fallback to non-empty `HOME`.
/// Non-Windows prefers non-empty `HOME` with fallback to non-empty
/// `USERPROFILE`. Under Git Bash on Windows both are set and they name
/// different paths (`/c/Users/x` vs `C:\Users\x`), so a caller that picked the
/// other order landed in a different directory than the rest of the process.
pub fn platform_home_with_env(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = ["USERPROFILE", "HOME"];
    #[cfg(not(windows))]
    let candidates = ["HOME", "USERPROFILE"];

    candidates
        .into_iter()
        .filter_map(env)
        .find(|value| !value.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(entries: &[(&str, &str)]) -> HashMap<String, OsString> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), OsString::from(value)))
            .collect()
    }

    fn config_home_with(entries: &[(&str, &str)]) -> Option<PathBuf> {
        let env = env_map(entries);
        config_home_with_env(|name| env.get(name).cloned())
    }

    #[test]
    fn config_dir_wins_over_every_other_source() {
        assert_eq!(
            config_home_with(&[
                ("REBON_CONFIG_DIR", "D:\\rebon"),
                ("REBON_CONFIG_HOME", "E:\\alias"),
                ("HOME", "/tmp/home"),
                ("USERPROFILE", "C:\\Users\\example"),
            ]),
            Some(PathBuf::from("D:\\rebon"))
        );
    }

    /// The alias the desktop app writes has to move the data too. It used to
    /// move only the config, leaving transcripts in `~/.rebon`.
    #[test]
    fn config_home_alias_is_honored_when_the_dir_var_is_absent() {
        assert_eq!(
            config_home_with(&[("REBON_CONFIG_HOME", "E:\\alias"), ("HOME", "/tmp/home")]),
            Some(PathBuf::from("E:\\alias"))
        );
    }

    #[test]
    fn blank_overrides_fall_through_to_the_platform_home() {
        for blank in ["", "  \t  "] {
            assert_eq!(
                config_home_with(&[
                    ("REBON_CONFIG_DIR", blank),
                    ("REBON_CONFIG_HOME", blank),
                    ("HOME", "/tmp/home"),
                ]),
                Some(PathBuf::from("/tmp/home").join(".rebon")),
                "blank override {blank:?} must not win"
            );
        }
    }

    #[test]
    fn no_override_and_no_home_resolves_to_nothing() {
        assert_eq!(config_home_with(&[]), None);
    }

    /// The question a migration actually turns on: once the data has moved,
    /// nothing may still resolve to the profile directory. The override is
    /// consulted before the platform home is even looked up, so a stale
    /// `~/.rebon` full of the old data cannot pull anything back.
    #[test]
    fn a_configured_home_never_falls_back_to_the_profile_directory() {
        let moved = PathBuf::from("D:\\rebon");
        for env in [
            vec![("REBON_CONFIG_DIR", "D:\\rebon")],
            vec![("REBON_CONFIG_HOME", "D:\\rebon")],
        ] {
            let mut with_profile = env.clone();
            with_profile.push(("HOME", "/tmp/home"));
            with_profile.push(("USERPROFILE", "C:\\Users\\example"));
            assert_eq!(
                config_home_with(&with_profile),
                Some(moved.clone()),
                "a set override must win over any home directory: {env:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefers_userprofile_over_a_git_bash_home() {
        let env = env_map(&[("HOME", "/c/Users/me"), ("USERPROFILE", "C:\\Users\\me")]);
        assert_eq!(
            platform_home_with_env(|name| env.get(name).cloned()),
            Some(PathBuf::from("C:\\Users\\me"))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_prefers_home() {
        let env = env_map(&[("HOME", "/home/me"), ("USERPROFILE", "/wrong")]);
        assert_eq!(
            platform_home_with_env(|name| env.get(name).cloned()),
            Some(PathBuf::from("/home/me"))
        );
    }
}
