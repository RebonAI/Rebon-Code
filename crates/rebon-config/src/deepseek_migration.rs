//! Move DeepSeek off the retired `deepseek-responses` plugin, once, at startup.
//!
//! DeepSeek's Responses API used to be served by an installed plugin, which
//! took over any provider entry named `deepseek` by name alone; nothing in the
//! entry said so. It is built in now (`openai-responses` with vendor
//! `deepseek`), and `deepseek` is reserved so a copy of the plugin left on
//! disk cannot claim the entry again.
//!
//! So when the plugin is still installed, the `deepseek` entry is rewritten to
//! the built-in format, given what the plugin's manifest used to supply (a
//! default model, the model list, the `small` profile), and the plugin is
//! uninstalled: its install record, its files and its container grant. The
//! entry is rewritten first; if that fails the plugin stays, so the user keeps
//! a working provider and the next start tries again.

use std::path::Path;

use rebon_plugin_package::container::{package_container_id, ContainerGrants};
use rebon_plugin_package::{PluginScope, PluginStore};

use crate::{read_config_roundtrip, write_config_roundtrip, ProviderFormat};

/// The package that served DeepSeek before it moved in-tree.
pub const RETIRED_DEEPSEEK_PLUGIN: &str = "deepseek-responses";

/// The provider id that plugin registered, and the entry name it took over.
pub const DEEPSEEK_PROVIDER_ID: &str = "deepseek";

/// What the plugin's manifest declared, for entries that relied on it.
const DEFAULT_MODEL: &str = "deepseek-v4-pro";
const SMALL_MODEL: &str = "deepseek-flash";
const MODELS: &[&str] = &["deepseek-v4-pro", "deepseek-flash"];

/// Provider ids that once belonged to a plugin and are built in now. No plugin
/// may register them, and an entry with that name resolves to a built-in.
pub fn is_retired_plugin_provider_id(id: &str) -> bool {
    id == DEEPSEEK_PROVIDER_ID
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeepSeekMigrationOutcome {
    /// The plugin is not installed: nothing to do.
    NotInstalled,
    /// The plugin was uninstalled; `rewrote_entry` says whether a `deepseek`
    /// provider entry was moved to the built-in format.
    Migrated { rewrote_entry: bool },
}

/// Run the migration against the user's config home. Never fatal: a failure
/// is logged and the next start tries again.
pub fn migrate_deepseek_plugin() {
    let config_dir = crate::config_home_dir();
    match migrate_deepseek_plugin_in(&config_dir) {
        Ok(DeepSeekMigrationOutcome::NotInstalled) => {}
        Ok(DeepSeekMigrationOutcome::Migrated { rewrote_entry }) => tracing::info!(
            rewrote_entry,
            "DeepSeek is built in now: moved off the deepseek-responses plugin and uninstalled it"
        ),
        Err(error) => tracing::warn!(
            error = %error,
            "could not move DeepSeek off the deepseek-responses plugin; will retry next start"
        ),
    }
}

pub fn migrate_deepseek_plugin_in(config_dir: &Path) -> anyhow::Result<DeepSeekMigrationOutcome> {
    // Only the user scope: the project scope needs a working directory, and
    // the plugin was installed per user.
    let store = PluginStore::new(config_dir.to_path_buf(), config_dir.to_path_buf());
    if !is_installed(&store)? {
        return Ok(DeepSeekMigrationOutcome::NotInstalled);
    }

    let rewrote_entry = rewrite_deepseek_entry(config_dir)?;

    store.uninstall(PluginScope::User, RETIRED_DEEPSEEK_PLUGIN)?;
    let package_dir = store
        .scope_dir(PluginScope::User)
        .join(RETIRED_DEEPSEEK_PLUGIN);
    if package_dir.exists() {
        if let Err(error) = std::fs::remove_dir_all(&package_dir) {
            // The record is gone, so nothing loads these files any more.
            tracing::warn!(error = %error, dir = %package_dir.display(), "could not delete the old plugin files");
        }
    }
    let mut grants = ContainerGrants::load(config_dir);
    grants.remove(&package_container_id(RETIRED_DEEPSEEK_PLUGIN));
    grants.save(config_dir)?;

    Ok(DeepSeekMigrationOutcome::Migrated { rewrote_entry })
}

fn is_installed(store: &PluginStore) -> anyhow::Result<bool> {
    Ok(store
        .load_state(PluginScope::User)?
        .plugins
        .iter()
        .any(|record| record.name == RETIRED_DEEPSEEK_PLUGIN))
}

/// Point the `deepseek` entry at the built-in client. Returns whether there
/// was one. The match is exact, as the plugin's takeover was.
fn rewrite_deepseek_entry(config_dir: &Path) -> anyhow::Result<bool> {
    let mut config = read_config_roundtrip(config_dir)?;
    let Some(entry) = config
        .custom_providers
        .iter_mut()
        .find(|provider| provider.name == DEEPSEEK_PROVIDER_ID)
    else {
        return Ok(false);
    };
    entry.format = Some(ProviderFormat::OpenaiResponses.as_str().to_string());
    // Pinned so a gateway `baseUrl` still gets DeepSeek's dialect. An empty
    // `baseUrl` stays empty: it means `DEEPSEEK_BASE_URL`, then the official
    // endpoint, as it did under the plugin.
    if entry.vendor.as_deref().map_or(true, str::is_empty) {
        entry.vendor = Some(DEEPSEEK_PROVIDER_ID.to_string());
    }
    if entry.model.trim().is_empty() {
        entry.model = DEFAULT_MODEL.to_string();
    }
    if entry.models.is_empty() {
        for model in MODELS {
            entry.models.push_id(model.to_string());
        }
    }
    if entry.model_profiles.get("small").is_none() {
        entry.model_profiles.insert("small", SMALL_MODEL);
    }
    write_config_roundtrip(config_dir, &config)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, value: serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn install_plugin(config_dir: &Path) {
        let store = PluginStore::new(config_dir.to_path_buf(), config_dir.to_path_buf());
        write(
            &store.state_path(PluginScope::User),
            json!({"plugins": [{
                "name": RETIRED_DEEPSEEK_PLUGIN, "version": "0.6.0", "enabled": true,
                "sourceKind": "local", "source": "x", "digest": "sha256:0",
            }]}),
        );
        let package = store
            .plugin_version_dir(PluginScope::User, RETIRED_DEEPSEEK_PLUGIN, "0.6.0")
            .join("provider.mjs");
        std::fs::create_dir_all(package.parent().unwrap()).unwrap();
        std::fs::write(package, "// old").unwrap();
        let mut grants = ContainerGrants::load(config_dir);
        grants.set(
            &package_container_id(RETIRED_DEEPSEEK_PLUGIN),
            serde_json::from_value(json!({"network": ["api.deepseek.com"]})).unwrap(),
        );
        grants.save(config_dir).unwrap();
    }

    #[test]
    fn nothing_happens_without_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("config.json"),
            json!({"customProviders": [{"name": "deepseek", "format": "openai", "baseUrl": "https://api.deepseek.com", "apiKey": "k"}]}),
        );
        assert_eq!(
            migrate_deepseek_plugin_in(dir.path()).unwrap(),
            DeepSeekMigrationOutcome::NotInstalled
        );
        let config = read_config_roundtrip(dir.path()).unwrap();
        assert_eq!(config.custom_providers[0].format.as_deref(), Some("openai"));
    }

    #[test]
    fn the_entry_moves_to_the_built_in_and_the_plugin_is_uninstalled() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("config.json"),
            json!({
                "activeCustomProvider": "deepseek",
                "customProviders": [{"name": "deepseek", "baseUrl": "", "apiKey": "$DEEPSEEK_API_KEY",
                                     "options": {"extraBody": {"prompt_cache_retention": "24h"}}}],
            }),
        );
        install_plugin(dir.path());

        assert_eq!(
            migrate_deepseek_plugin_in(dir.path()).unwrap(),
            DeepSeekMigrationOutcome::Migrated {
                rewrote_entry: true
            }
        );

        let config = read_config_roundtrip(dir.path()).unwrap();
        let entry = &config.custom_providers[0];
        assert_eq!(entry.format.as_deref(), Some("openai-responses"));
        assert_eq!(entry.vendor.as_deref(), Some("deepseek"));
        assert_eq!(
            entry.base_url, "",
            "an empty base URL keeps meaning DEEPSEEK_BASE_URL"
        );
        assert_eq!(entry.api_key, "$DEEPSEEK_API_KEY");
        assert_eq!(entry.model, "deepseek-v4-pro");
        assert_eq!(entry.model_profiles.get("small"), Some("deepseek-flash"));
        assert_eq!(
            serde_json::to_value(&entry.options).unwrap()["extraBody"]["prompt_cache_retention"],
            "24h",
            "the entry's own options are kept"
        );

        let store = PluginStore::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        assert!(!is_installed(&store).unwrap());
        assert!(!store
            .scope_dir(PluginScope::User)
            .join(RETIRED_DEEPSEEK_PLUGIN)
            .exists());
        let grant =
            ContainerGrants::load(dir.path()).get(&package_container_id(RETIRED_DEEPSEEK_PLUGIN));
        assert!(grant.network.is_empty(), "the container grant is gone");

        assert_eq!(
            migrate_deepseek_plugin_in(dir.path()).unwrap(),
            DeepSeekMigrationOutcome::NotInstalled,
            "running again is a no-op"
        );
    }

    #[test]
    fn an_entry_that_set_its_own_model_and_vendor_keeps_them() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("config.json"),
            json!({"customProviders": [{"name": "deepseek", "baseUrl": "https://relay.example/v1", "apiKey": "k",
                                        "vendor": "deepseek", "model": "deepseek-flash", "models": ["deepseek-flash"]}]}),
        );
        install_plugin(dir.path());
        migrate_deepseek_plugin_in(dir.path()).unwrap();
        let entry = &read_config_roundtrip(dir.path()).unwrap().custom_providers[0];
        assert_eq!(entry.model, "deepseek-flash");
        assert_eq!(entry.base_url, "https://relay.example/v1");
    }

    #[test]
    fn without_a_deepseek_entry_the_plugin_is_still_removed() {
        let dir = tempfile::tempdir().unwrap();
        install_plugin(dir.path());
        assert_eq!(
            migrate_deepseek_plugin_in(dir.path()).unwrap(),
            DeepSeekMigrationOutcome::Migrated {
                rewrote_entry: false
            }
        );
    }
}
