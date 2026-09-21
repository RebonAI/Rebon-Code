use std::path::{Path, PathBuf};

use super::{
    read_config_roundtrip, settings_layers_in_files, GENERATED_IMAGES_DIR_CONFIG_KEY,
    GENERATED_IMAGES_DIR_SNAKE_CONFIG_KEY,
};

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve a configured path against a working directory.
///
/// An absolute path is taken as written; anything else is joined onto `cwd`.
/// Four call sites had grown their own copy of this under four names, which is
/// four places to disagree about what a relative path in a config file means.
pub fn resolve_against_cwd(cwd: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// Return the rebon config directory the running process should use.
///
/// Precedence:
/// 1. `$REBON_CONFIG_DIR` env var (used as-is)
/// 2. `$REBON_CONFIG_HOME` env var (the app's alias, used as-is)
/// 3. `~/.rebon`
///
/// If no home directory can be determined this falls back to `.rebon`
/// relative to the cwd.
pub fn config_home_dir() -> PathBuf {
    rebon_session::config_home::default_config_home_dir()
}

/// Resolve the configured generated-image output base directory.
///
/// Reads top-level `generatedImagesDir` from `config.json`, with
/// `generated_images_dir` accepted as an alias. Supported values:
///
/// - absolute path: used as-is
/// - relative path: resolved relative to `cwd`
/// - placeholders: `{cwd}`, `{home}`, `{config}`
///
/// If the key is absent, empty, non-string, or config parsing fails,
/// this falls back to `<config_dir>/generated_images`.
pub fn generated_images_output_base(cwd: &Path) -> PathBuf {
    generated_images_output_base_in_dir(&config_home_dir(), cwd)
}

pub fn generated_images_output_base_in_dir(config_dir: &Path, cwd: &Path) -> PathBuf {
    let default = config_dir.join("generated_images");
    let config = match read_config_roundtrip(config_dir) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to read config for generated image output dir; using default"
            );
            return default;
        }
    };

    let Some(raw) = config
        .extra
        .get(GENERATED_IMAGES_DIR_CONFIG_KEY)
        .or_else(|| config.extra.get(GENERATED_IMAGES_DIR_SNAKE_CONFIG_KEY))
        .and_then(|v| v.as_str())
    else {
        return default;
    };

    let raw = raw.trim();
    if raw.is_empty() {
        return default;
    }

    resolve_configured_path(raw, config_dir, cwd)
}

fn resolve_configured_path(raw: &str, config_dir: &Path, cwd: &Path) -> PathBuf {
    let mut expanded = raw.to_string();
    if expanded == "~" || expanded.starts_with("~/") || expanded.starts_with("~\\") {
        if let Some(home) = home_dir() {
            let rest = expanded
                .strip_prefix("~/")
                .or_else(|| expanded.strip_prefix("~\\"))
                .unwrap_or("");
            expanded = if rest.is_empty() {
                home.to_string_lossy().into_owned()
            } else {
                home.join(rest).to_string_lossy().into_owned()
            };
        }
    }

    expanded = expanded.replace("{cwd}", &cwd.to_string_lossy());
    if let Some(home) = home_dir() {
        expanded = expanded.replace("{home}", &home.to_string_lossy());
    }
    expanded = expanded.replace("{config}", &config_dir.to_string_lossy());

    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// Cross-platform home directory lookup used by [`config_home_dir`].
///
/// Delegates so the platform preference is decided once: this copy read only
/// the platform's own variable, so under Git Bash on Windows it could resolve
/// a different home than the callers that fall back to the other one.
pub fn home_dir() -> Option<PathBuf> {
    rebon_session::platform_home_dir()
}

/// Path of the primary `config.json` for the given config dir.
pub fn config_json_path(config_dir: &Path) -> PathBuf {
    config_dir.join("config.json")
}

/// The `settings.json` chain, in the order later files override earlier ones.
///
/// Here rather than beside any one reader because three of them disagree the
/// moment there are two copies: the sandbox plugin compiles a session from
/// these files, `/doctor` reports on them, and the harness reads one key out
/// of them to decide whether a disabled sandbox plugin is a problem. A
/// project file the plugin honours and the harness does not is a workspace
/// that is confined but reports itself unconfined, or the reverse.
pub fn settings_files(config_dir: &Path, cwd: &Path) -> Vec<(&'static str, PathBuf)> {
    settings_files_for_mode(config_dir, cwd, cowork_mode())
}

/// [`settings_files`] with the cowork switch passed in, so a test can pin
/// both layouts without touching the process environment.
pub fn settings_files_for_mode(
    config_dir: &Path,
    cwd: &Path,
    use_cowork: bool,
) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("user", user_settings_file_for_mode(config_dir, use_cowork)),
        ("project", cwd.join(".rebon").join("settings.json")),
        ("local", cwd.join(".rebon").join("settings.local.json")),
    ]
}

/// The user layer of the settings chain: `settings.json`, or
/// `cowork_settings.json` when `REBON_USE_COWORK_PLUGINS` is on.
///
/// Every reader *and writer* of a user-level setting goes through this one
/// name. The plugin switches used to hard-code `settings.json` while the
/// sandbox settings came through [`settings_files`], so in cowork mode
/// `sandbox.enabled` and `plugins.sandbox.enabled` were read from two
/// different files — and once a disabled sandbox plugin became a refusal
/// rather than a passthrough, that split turned into every shell command
/// being refused by a switch the user could not see.
pub fn user_settings_file(config_dir: &Path) -> PathBuf {
    user_settings_file_for_mode(config_dir, cowork_mode())
}

/// [`user_settings_file`] with the cowork switch passed in.
pub fn user_settings_file_for_mode(config_dir: &Path, use_cowork: bool) -> PathBuf {
    config_dir.join(if use_cowork {
        "cowork_settings.json"
    } else {
        "settings.json"
    })
}

fn cowork_mode() -> bool {
    cowork_mode_from(std::env::var("REBON_USE_COWORK_PLUGINS").ok().as_deref())
}

/// Whether a `REBON_USE_COWORK_PLUGINS` value switches the user layer to
/// `cowork_settings.json`.
///
/// The one spelling of that rule, exposed so a reader that resolves its
/// environment through an injected lookup — the memory plugin's auto-memory
/// switch, whose tests pin the environment without touching the process —
/// answers the same way [`settings_files`] does.
pub fn cowork_mode_from(value: Option<&str>) -> bool {
    value.is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// Whether `sandbox.enabled` comes out true across [`settings_files`].
///
/// A plain key read rather than the sandbox plugin's own settings parser,
/// because the one caller is the code that decides what to do when the
/// sandbox plugin is *not loaded* — a question that cannot be answered by
/// asking it. The walk is [`settings_layers_in_files`], the same one the
/// plugin switches and a plugin's own keys are read through, so later
/// files win and unreadable or malformed files are skipped for the same
/// reason here as there; the default is `false`. `/doctor` does its own
/// walk because it reports what it had to skip rather than skipping
/// silently.
pub fn sandbox_enabled_in_settings(config_dir: &Path, cwd: &Path) -> bool {
    let mut enabled = false;
    for settings in settings_layers_in_files(
        settings_files(config_dir, cwd)
            .into_iter()
            .map(|(_, path)| path),
    ) {
        if let Some(found) = settings
            .get("sandbox")
            .and_then(|sandbox| sandbox.get("enabled"))
            .and_then(serde_json::Value::as_bool)
        {
            enabled = found;
        }
    }
    enabled
}

/// Optional fast-edit agent model config, same shape as the
/// `agents` / `categories` section in config.json.
pub fn agents_json_path(config_dir: &Path) -> PathBuf {
    config_dir.join("agents.json")
}

/// Path of the `.credentials.json` secure storage file for the
/// given config dir.
pub fn credentials_json_path(config_dir: &Path) -> PathBuf {
    config_dir.join(".credentials.json")
}
