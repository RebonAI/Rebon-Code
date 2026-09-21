//! One list of every provider Rebon can see, whatever supplied it.
//!
//! The provider store and plugin manifests feed one list. Providers arrive
//! from two places that never met in any UI:
//!
//! - the **provider store** (`~/.rebon/providers/<id>.json`) — endpoint,
//!   credentials, models, profiles;
//! - a **plugin manifest** — transport, capabilities, and default
//!   model/profiles for a provider the plugin knows how to speak to.
//!
//! They are not competitors. A plugin provider carries *how to reach* a
//! service and nothing that identifies the caller; the store entry carries the
//! `baseUrl` and `apiKey`. `build_client` pairs them by name at connect time
//! (`provider_registry.rs`'s `External` arm feeds the resolved entry's
//! connection into the plugin's process), so a plugin provider with no
//! same-named store entry has no credentials and cannot run — and a store
//! entry that *is* paired stops speaking HTTP and starts speaking to a child
//! process, which nothing in `/provider list` or the settings window ever
//! said out loud.
//!
//! This module answers that: for each provider, where it came from, whether
//! the pairing happened, and whether it can actually be used.

use std::collections::BTreeMap;
use std::path::Path;

use rebon_types::ModelProfileMap;

use crate::model_provider_plugin::PluginModelProviderContribution;

/// Where a provider's definition came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderOrigin {
    /// Only the provider store has it. Runs on a built-in wire format.
    User,
    /// Only a plugin has it. Usable once a same-named store entry supplies
    /// the endpoint and key.
    Plugin { plugin: String },
    /// Both: the plugin's transport carries the store entry's credentials.
    UserWithPlugin { plugin: String },
}

impl ProviderOrigin {
    pub fn plugin_name(&self) -> Option<&str> {
        match self {
            Self::User => None,
            Self::Plugin { plugin } | Self::UserWithPlugin { plugin } => Some(plugin),
        }
    }

    /// Whether a front end must refuse to edit this entry.
    ///
    /// A plugin-only row has no file to write to — its definition lives in the
    /// plugin package.
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::Plugin { .. })
    }
}

/// One provider, as a user should see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCatalogEntry {
    /// Lookup key. Matches the store's file-name id for user entries and the
    /// manifest id for plugin ones — they share one namespace, which is what
    /// makes the pairing possible in the first place.
    pub id: String,
    pub display_name: String,
    pub origin: ProviderOrigin,
    pub is_active: bool,
    /// Built-in wire format, when the store entry names one. A paired entry
    /// still records it, but the plugin's transport is what actually runs.
    pub format: Option<String>,
    pub base_url: Option<String>,
    /// Already masked — a catalog is for display, and an unmasked key would
    /// eventually reach a log or a screenshot.
    pub api_key_masked: Option<String>,
    pub default_model: Option<String>,
    pub models: Vec<String>,
    /// Effective profile table: the store's declarations layered over the
    /// plugin manifest's, matching what resolution actually does.
    pub model_profiles: ModelProfileMap,
    /// `None` when usable; otherwise why not, phrased as something to do.
    pub unusable_reason: Option<String>,
}

impl ProviderCatalogEntry {
    pub fn is_usable(&self) -> bool {
        self.unusable_reason.is_none()
    }
}

/// Discover the model providers this install's plugins contribute.
///
/// Narrower than the full runtime-contribution resolution a session runs,
/// which materialises everything a session needs (MCP servers, skills,
/// agents, hooks) and belongs to the binary that assembles a session. Listing
/// providers needs one capability and has to work from the desktop app too,
/// so it walks the same discovery and calls the same
/// `materialize_model_provider_contribution` — the manifest-to-contribution
/// translation stays in one place, only the traversal is repeated.
///
/// Best-effort by design: a plugin that fails to materialise is skipped so the
/// catalog still lists everything else.
pub fn discover_plugin_model_providers(
    config_home: &Path,
    cwd: &Path,
) -> Vec<PluginModelProviderContribution> {
    use rebon_plugin_package::discovery::{discover, InstalledPlugin};
    use rebon_plugin_package::model_provider_manifest::materialize_model_provider_contribution;
    use rebon_plugin_package::store::PluginStore;

    let store = PluginStore::new(config_home.to_path_buf(), cwd.to_path_buf());
    let project_trusted = rebon_config::is_directory_trusted_in(config_home, cwd);
    let found = match discover(&store, &[], cwd, project_trusted) {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(error = %err, "provider catalog: plugin discovery failed");
            return Vec::new();
        }
    };

    // `{rebon}` placeholders in a manifest's command expand against this.
    // Called from `/provider list` and the settings window mid-session, so an
    // unreadable executable path degrades to the bare name rather than
    // failing the listing.
    let rebon_exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("rebon"));
    let mut out = Vec::new();
    for plugin in &found.plugins {
        let InstalledPlugin::Package { root, manifest } = &plugin.plugin else {
            continue;
        };
        for (provider_id, provider) in &manifest.capabilities.model_providers {
            match materialize_model_provider_contribution(
                &manifest.name,
                provider_id,
                provider,
                root,
                &plugin.source,
                &rebon_exe,
            ) {
                Ok(contribution) => out.push(contribution),
                Err(err) => {
                    tracing::warn!(
                        plugin = %manifest.name,
                        provider = %provider_id,
                        error = %err,
                        "provider catalog: skipping a plugin provider that could not be materialised"
                    );
                }
            }
        }
    }
    out
}

/// Build the catalog from the store plus whatever plugins contributed.
///
/// Plugin contributions are passed in rather than scanned for here: the same
/// list already travels through `HarnessOverrides`, and re-scanning would mean
/// this function could disagree with the registry that actually resolves.
pub fn provider_catalog(
    config_dir: &Path,
    plugin_providers: &[PluginModelProviderContribution],
) -> Vec<ProviderCatalogEntry> {
    let active = rebon_config::get_active_custom_provider_name_from(config_dir);
    let stored = rebon_config::list_custom_providers_from(config_dir);

    let plugins_by_id: BTreeMap<String, &PluginModelProviderContribution> = plugin_providers
        .iter()
        .map(|contribution| (contribution.id.trim().to_ascii_lowercase(), contribution))
        .collect();

    let mut entries: Vec<ProviderCatalogEntry> = Vec::new();
    let mut paired: Vec<String> = Vec::new();

    for provider in &stored {
        let id = provider.name.trim().to_ascii_lowercase();
        let plugin = plugins_by_id.get(&id).copied();
        if plugin.is_some() {
            paired.push(id.clone());
        }

        // What resolution will use: the store's declarations win, the
        // manifest's fill the gaps (`fill_missing_from`, same as
        // `resolve_runtime_model`).
        let mut model_profiles =
            rebon_config::custom_provider_profiles_in(config_dir, &provider.name)
                .map(|rows| {
                    let mut map = ModelProfileMap::new();
                    for row in rows {
                        if let Some(model) = row.model {
                            map.insert_with_reasoning_effort(
                                &row.role,
                                model,
                                row.reasoning_effort,
                            );
                        }
                    }
                    map
                })
                .unwrap_or_default();
        if let Some(plugin) = plugin {
            model_profiles.fill_missing_from(&plugin.profiles);
        }

        entries.push(ProviderCatalogEntry {
            id,
            display_name: provider.name.clone(),
            origin: match plugin {
                Some(plugin) => ProviderOrigin::UserWithPlugin {
                    plugin: plugin.plugin_name.clone(),
                },
                None => ProviderOrigin::User,
            },
            is_active: active
                .as_deref()
                .is_some_and(|active| active.eq_ignore_ascii_case(&provider.name)),
            format: Some(provider.format.clone()),
            base_url: Some(provider.base_url.clone()),
            api_key_masked: Some(rebon_config::mask_api_key(&provider.api_key)),
            default_model: (!provider.model.trim().is_empty()).then(|| provider.model.clone()),
            models: provider.models.clone(),
            model_profiles,
            unusable_reason: None,
        });
    }

    for (id, contribution) in &plugins_by_id {
        if paired.contains(id) {
            continue;
        }
        entries.push(ProviderCatalogEntry {
            id: id.clone(),
            display_name: contribution
                .display_name
                .clone()
                .unwrap_or_else(|| contribution.id.clone()),
            origin: ProviderOrigin::Plugin {
                plugin: contribution.plugin_name.clone(),
            },
            is_active: false,
            format: None,
            base_url: None,
            api_key_masked: None,
            default_model: contribution.default_model.clone(),
            models: contribution.models.keys().cloned().collect(),
            model_profiles: contribution.profiles.clone(),
            unusable_reason: Some(format!(
                "the plugin supplies the transport but not the endpoint or key — \
                 add a provider named `{id}` to supply them"
            )),
        });
    }

    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_provider_plugin::{
        MaterializedModelProviderTransport, MaterializedPluginModelProviderTransport,
        ModelProviderCapabilityManifest, ModelProviderModelManifest,
    };
    use tempfile::TempDir;

    fn contribution(id: &str, plugin: &str) -> PluginModelProviderContribution {
        PluginModelProviderContribution {
            id: id.to_string(),
            plugin_name: plugin.to_string(),
            source: format!("plugin:{plugin}"),
            display_name: Some(format!("{id} (plugin)")),
            transport: MaterializedModelProviderTransport::Plugin(
                MaterializedPluginModelProviderTransport {
                    root: std::path::PathBuf::from("/plugins/vendor"),
                    entry: "provider.mjs".into(),
                },
            ),
            capabilities: ModelProviderCapabilityManifest::default(),
            default_model: Some("vendor-pro".into()),
            models: BTreeMap::from([(
                "vendor-flash".to_string(),
                ModelProviderModelManifest::default(),
            )]),
            profiles: ModelProfileMap::from_entries([("small", "vendor-flash")]),
        }
    }

    fn config_with_providers(json: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(rebon_config::config_json_path(tmp.path()), json).unwrap();
        tmp
    }

    #[test]
    fn a_store_entry_with_no_plugin_is_a_plain_user_provider() {
        let tmp = config_with_providers(
            r#"{"activeCustomProvider":"vendor","customProviders":[
                {"name":"vendor","format":"openai","baseUrl":"https://x.example","apiKey":"sk-secret","model":"vendor-pro"}
            ]}"#,
        );

        let catalog = provider_catalog(tmp.path(), &[]);

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].origin, ProviderOrigin::User);
        assert!(catalog[0].is_active);
        assert!(catalog[0].is_usable());
        assert!(!catalog[0].origin.is_read_only());
        // Keys never leave this module in the clear.
        assert!(!catalog[0]
            .api_key_masked
            .as_deref()
            .unwrap()
            .contains("secret"));
    }

    #[test]
    fn a_plugin_with_a_same_named_entry_reports_the_pairing() {
        let tmp = config_with_providers(
            r#"{"activeCustomProvider":"Vendor","customProviders":[
                {"name":"Vendor","format":"openai","baseUrl":"https://x.example","apiKey":"sk","model":"vendor-pro"}
            ]}"#,
        );

        let catalog = provider_catalog(tmp.path(), &[contribution("vendor", "vendor-plugin")]);

        // One row, not two: the pairing is one provider, described twice.
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog[0].origin,
            ProviderOrigin::UserWithPlugin {
                plugin: "vendor-plugin".into()
            }
        );
        assert!(catalog[0].is_usable());
        // Case differences in the entry name must not break the pairing —
        // the runtime pairs case-insensitively too.
        assert!(catalog[0].is_active);
        // The manifest's profile fills a gap the user left.
        assert_eq!(catalog[0].model_profiles.get("small"), Some("vendor-flash"));
    }

    #[test]
    fn a_user_declaration_outranks_the_manifests_for_the_same_role() {
        let tmp = config_with_providers(
            r#"{"customProviders":[
                {"name":"vendor","format":"openai","baseUrl":"https://x.example","apiKey":"sk",
                 "model":"vendor-pro","modelProfiles":{"small":"user-choice"}}
            ]}"#,
        );

        let catalog = provider_catalog(tmp.path(), &[contribution("vendor", "vendor-plugin")]);

        assert_eq!(catalog[0].model_profiles.get("small"), Some("user-choice"));
    }

    #[test]
    fn a_plugin_with_no_entry_is_listed_but_not_usable() {
        let tmp = config_with_providers(r#"{"customProviders":[]}"#);

        let catalog = provider_catalog(tmp.path(), &[contribution("vendor", "vendor-plugin")]);

        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog[0].origin,
            ProviderOrigin::Plugin {
                plugin: "vendor-plugin".into()
            }
        );
        assert!(!catalog[0].is_usable());
        assert!(catalog[0].origin.is_read_only());
        // The reason has to say what to do, not just that it failed.
        let reason = catalog[0].unusable_reason.as_deref().unwrap();
        assert!(reason.contains("add a provider named `vendor`"), "{reason}");
        // Still worth showing what it would offer.
        assert_eq!(catalog[0].default_model.as_deref(), Some("vendor-pro"));
        assert_eq!(catalog[0].models, vec!["vendor-flash".to_string()]);
    }

    #[test]
    fn entries_are_listed_once_each_in_id_order() {
        let tmp = config_with_providers(
            r#"{"customProviders":[
                {"name":"zeta","format":"openai","baseUrl":"https://z.example","apiKey":"sk","model":"m"},
                {"name":"alpha","format":"openai","baseUrl":"https://a.example","apiKey":"sk","model":"m"}
            ]}"#,
        );

        let catalog = provider_catalog(
            tmp.path(),
            &[contribution("mid", "p1"), contribution("alpha", "p2")],
        );

        let ids: Vec<&str> = catalog.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "mid", "zeta"]);
    }
}
