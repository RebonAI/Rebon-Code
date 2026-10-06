//! A mod folder as a composition entry.
//!
//! Three ways a mod reaches the plane, all ending here:
//!
//! * `kernelPlugins.modules` points a name at the folder (or at its
//!   `.claude-plugin/plugin.json`), and `plugins` lists the name;
//! * the folder sits under `<config_home>/mods/`, or a folder `REBON_MOD_DIRS`
//!   or `CLAUDE_CODE_PLUGIN_DIRS` names — loaded under the manifest's own
//!   name, with no configuration written, which is what makes dropping a
//!   mod folder there enough;
//! * `rebon plugin install <folder>` recorded it, with the manifest this
//!   crate synthesised beside it, and an installed package is found by name
//!   like any other.
//!
//! The entry is the plane's ordinary [`ComposeEntry`]: the scanned ceiling
//! as its declarations, the hooks module as its entry, and the marker the
//! `mods-runtime` loader reads under `config`.

use std::path::{Path, PathBuf};

use rebon_plugin_package::{
    is_claude_mod_dir, kernel_manifest_for, mod_config, read_claude_mod, user_config_options,
    ClaudeMod,
};
use rebon_types::{CLAUDE_PLUGIN_DIRS_ENV, CLAUDE_PLUGIN_MANIFEST, MOD_DIRS_ENV};
use serde_json::Value;

use crate::plugin_manifests::{entry_for, plain_path};
use crate::plugin_plane::ComposeEntry;

/// The folder a `modules` path names, when it names a mod.
///
/// A path at the folder, at its manifest, or at its hooks module all mean
/// the folder: a person copies whichever of the three they had at hand.
pub fn mod_root_of(path: &Path) -> Option<PathBuf> {
    if path.is_dir() {
        return is_claude_mod_dir(path).then(|| path.to_path_buf());
    }
    let manifest = Path::new(CLAUDE_PLUGIN_MANIFEST);
    if path.ends_with(manifest) {
        let root = path.parent()?.parent()?;
        return is_claude_mod_dir(root).then(|| root.to_path_buf());
    }
    // `hooks/<module>`: the folder above `hooks`.
    let hooks_dir = path.parent()?;
    if hooks_dir.file_name().is_some_and(|name| name == "hooks") {
        let root = hooks_dir.parent()?;
        return is_claude_mod_dir(root).then(|| root.to_path_buf());
    }
    None
}

/// The options stored for a mod: `plugins.<name>` in the user's
/// `settings.json` (the namespace its `userConfig` keys are declared under),
/// or Claude Code's own `pluginConfigs.<name>` spelling.
pub fn stored_options(config_dir: &Path, name: &str) -> Option<Value> {
    let raw = std::fs::read(config_dir.join("settings.json")).ok()?;
    let settings: Value = serde_json::from_slice(&raw).ok()?;
    settings
        .get("plugins")
        .and_then(|plugins| plugins.get(name))
        .filter(|value| value.is_object())
        .cloned()
        .or_else(|| {
            settings
                .get("pluginConfigs")
                .and_then(|configs| configs.get(name))
                .filter(|value| value.is_object())
                .cloned()
        })
}

/// The load request for a mod folder, under `id`.
///
/// `extra` is the entry's `config` block from `kernelPlugins`, when the
/// composition wrote one; its keys ride under the marker as options too, so
/// a composition can set what a settings file would.
pub fn mod_entry(
    id: &str,
    root: &Path,
    config_dir: &Path,
    extra: &Value,
    all_tools: &[String],
) -> Result<ComposeEntry, String> {
    let mod_ = read_claude_mod(root)?;
    Ok(entry_for_mod(id, &mod_, config_dir, extra, all_tools))
}

pub fn entry_for_mod(
    id: &str,
    mod_: &ClaudeMod,
    config_dir: &Path,
    extra: &Value,
    all_tools: &[String],
) -> ComposeEntry {
    let manifest = kernel_manifest_for(mod_);
    let mut stored = stored_options(config_dir, &mod_.manifest.name).unwrap_or(Value::Null);
    if let (Some(into), Some(from)) = (stored.as_object_mut(), extra.as_object()) {
        for (key, value) in from {
            into.insert(key.clone(), value.clone());
        }
    } else if extra.is_object() {
        stored = extra.clone();
    }
    let options = user_config_options(&mod_.manifest, Some(&stored));
    for warning in &mod_.scan.warnings {
        tracing::warn!(mod_ = %mod_.manifest.name, %warning, "mod scan");
    }
    entry_for(
        &manifest,
        id,
        mod_.root.clone(),
        mod_.hooks_module.clone(),
        mod_config(mod_, options),
        all_tools,
    )
}

/// Every mod folder this machine loads without being configured for it.
pub fn discovered_mod_dirs(config_dir: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    rebon_plugin_package::discover_mod_dirs(config_dir, home.as_deref(), |name| {
        std::env::var(name).ok()
    })
}

/// The entries for the discovered folders whose names nothing else took.
pub fn discovered_entries(
    config_dir: &Path,
    taken: &[String],
    all_tools: &[String],
    skipped: &mut Vec<String>,
) -> Vec<ComposeEntry> {
    let mut out = Vec::new();
    let mut names: Vec<String> = taken.to_vec();
    for root in discovered_mod_dirs(config_dir) {
        let mod_ = match read_claude_mod(&root) {
            Ok(mod_) => mod_,
            Err(reason) => {
                skipped.push(format!("{}: {reason}", plain_path(&root)));
                continue;
            }
        };
        let name = mod_.manifest.name.clone();
        if names.contains(&name) {
            skipped.push(format!(
                "{}: a plugin named {name} is already in the composition; this folder is not loaded",
                plain_path(&root)
            ));
            continue;
        }
        names.push(name.clone());
        out.push(entry_for_mod(
            &name,
            &mod_,
            config_dir,
            &Value::Null,
            all_tools,
        ));
    }
    out
}

/// The variables a listing names so a person knows where else to look.
pub fn mod_dir_variables() -> [&'static str; 2] {
    [MOD_DIRS_ENV, CLAUDE_PLUGIN_DIRS_ENV]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_types::{CLAUDE_HOOKS_FILE, MOD_MARKER};
    use serde_json::json;

    fn write_mod(root: &Path, name: &str) {
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(root.join("hooks")).unwrap();
        std::fs::write(
            root.join(CLAUDE_PLUGIN_MANIFEST),
            json!({ "name": name, "version": "0.1.0", "userConfig": { "prefix": { "type": "string", "default": "n=" } } }).to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join(CLAUDE_HOOKS_FILE),
            r#"{ "modules": ["./register.ts"] }"#,
        )
        .unwrap();
        std::fs::write(
            root.join("hooks/register.ts"),
            "export const register = (on) => { on('session.start', async ($, e, next) => { await $.command.register({ name: 'hi', description: 'Says hi' }); return next(e); }); };",
        )
        .unwrap();
    }

    #[test]
    fn the_three_spellings_of_a_mod_path_name_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tally");
        write_mod(&root, "tally");
        assert_eq!(mod_root_of(&root), Some(root.clone()));
        assert_eq!(
            mod_root_of(&root.join(CLAUDE_PLUGIN_MANIFEST)),
            Some(root.clone())
        );
        assert_eq!(
            mod_root_of(&root.join("hooks/register.ts")),
            Some(root.clone())
        );
        assert_eq!(mod_root_of(&dir.path().join("nothing")), None);
        assert_eq!(mod_root_of(dir.path()), None);
    }

    #[test]
    fn a_mod_entry_carries_the_ceiling_the_marker_and_the_stored_options() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tally");
        write_mod(&root, "tally");
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("settings.json"),
            json!({ "plugins": { "tally": { "prefix": "#" } } }).to_string(),
        )
        .unwrap();
        let entry = mod_entry(
            "tally",
            &root,
            &config,
            &json!({ "extra": 1 }),
            &["Read".to_owned()],
        )
        .unwrap();
        assert_eq!(entry.id, "tally");
        assert_eq!(entry.entry, "hooks/register.ts");
        assert_eq!(entry.services, vec!["mod"]);
        assert_eq!(entry.commands, vec!["hi"]);
        assert_eq!(entry.seats, vec!["mods", "settings", "logger"]);
        assert!(entry.invokable_tools.is_empty());
        assert_eq!(entry.settings.len(), 1);
        assert_eq!(
            entry.config[MOD_MARKER]["options"],
            json!({ "prefix": "#" })
        );
        assert_eq!(
            entry.config[MOD_MARKER]["commands"][0]["description"],
            json!("Says hi")
        );
        assert!(entry.root.ends_with("tally"));
        assert!(!entry.root.contains('\\'));
    }

    #[test]
    fn claude_codes_own_plugin_configs_key_is_read_too() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            json!({ "pluginConfigs": { "tally": { "prefix": "@" } } }).to_string(),
        )
        .unwrap();
        assert_eq!(
            stored_options(dir.path(), "tally"),
            Some(json!({ "prefix": "@" }))
        );
        assert_eq!(stored_options(dir.path(), "other"), None);
    }

    #[test]
    fn discovered_entries_skip_taken_names_and_unreadable_folders() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        write_mod(&config.join("mods").join("one"), "one");
        write_mod(&config.join("mods").join("two"), "two");
        std::fs::create_dir_all(config.join("mods").join("broken").join(".claude-plugin")).unwrap();
        std::fs::write(
            config
                .join("mods")
                .join("broken")
                .join(CLAUDE_PLUGIN_MANIFEST),
            "{",
        )
        .unwrap();
        let mut skipped = Vec::new();
        let entries = discovered_entries(&config, &["two".to_owned()], &[], &mut skipped);
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["one"]
        );
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped.iter().any(|s| s.contains("broken")));
        assert!(skipped
            .iter()
            .any(|s| s.contains("already in the composition")));
    }

    /// A machine that never wrote `config.json`, or wrote one without
    /// `kernelPlugins`, still loads the mods under its config home: the
    /// folder is the whole of the opt-in.
    #[test]
    fn mods_compose_without_any_kernel_plugins_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        write_mod(&config.join("mods").join("one"), "one");
        let roots = crate::plugin_composition::CompositionRoots {
            payload: dir.path().join("payload"),
            runtime: dir.path().join("runtime"),
        };
        let ids = |config: &Path| -> Vec<String> {
            crate::plugin_composition::plane_composition(config, &roots, &[])
                .unwrap()
                .entries
                .into_iter()
                .map(|entry| entry.id)
                .collect()
        };
        assert_eq!(ids(&config), vec!["one"]);
        std::fs::write(
            config.join("config.json"),
            json!({ "language": "en" }).to_string(),
        )
        .unwrap();
        assert_eq!(ids(&config), vec!["one"]);
    }
}
