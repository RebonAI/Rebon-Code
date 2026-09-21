//! What a package declares about the plugins it puts on the kernel plugin plane.
//!
//! This is the ceiling on what a plugin may register and what it may reach —
//! the artefact a person reads before installing it. It lives here rather than
//! beside either reader because it has two: `rebon plugin install` validates it
//! at install time, and the plugin plane turns it into a `plugin/load` at boot.
//!
//! # One file name, one schema
//!
//! A package's declarations live in its `rebon-plugin.json`, under
//! `capabilities.kernelPlugins`, keyed by a name the composition refers to.
//! There is no second shape for the same file: a manifest is either a package
//! manifest or it is not read.
//!
//! The packages rebon vendors into its own payload are the one exception, and
//! not a second shape — they are dsh packages that have never heard of the
//! plugin plane, so rebon declares on their behalf in
//! `runtimes/node/compose-runtime/payload-manifests.json`. That table holds the same
//! [`KernelPluginManifest`] values; only where they are written differs.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The sentinel meaning "every tool this build exposes to the plane".
///
/// An agent loop runs whatever tool the model picks, so enumerating them in a
/// manifest would be a list that goes stale the moment rebon gains a tool.
/// Naming the set is the honest declaration; it is still a closed set, and
/// still the same one rebon offers the model.
pub const ALL_REBON_TOOLS: &str = "$rebon/tools";

/// Which package a manifest's `entry` is relative to.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KernelPluginRoot {
    /// The package the manifest itself belongs to — a third-party package's own
    /// directory, or the vendored payload tree for an entry in rebon's table.
    #[default]
    Package,
    /// The vendored JS payload, named explicitly.
    ///
    /// Only meaningful inside rebon's own table; a package naming it would be
    /// pointing its entry at rebon's files rather than its own.
    Payload,
    /// The composition runtime package, which holds rebon's own modules. Same
    /// restriction as [`Self::Payload`].
    Runtime,
}

impl KernelPluginRoot {
    /// Whether a third-party package may name this root.
    pub fn is_package_local(self) -> bool {
        matches!(self, Self::Package)
    }
}

/// One plugin on the plane, and everything it is allowed to do.
///
/// Every list is a ceiling checked twice: the plane refuses a ready report that
/// registers anything absent here, and refuses a call reaching for anything
/// absent here. An empty manifest is therefore a working plugin only if it
/// meant to be a library.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct KernelPluginManifest {
    /// Which package [`Self::entry`] is relative to.
    pub root: KernelPluginRoot,
    /// The module to load, relative to the root. Required in practice; optional
    /// in the type so a malformed entry is reported by the validator with the
    /// package's name attached rather than by serde with a field path.
    pub entry: Option<String>,
    /// Services the plugin may register and answer `service/call` on.
    pub services: Vec<String>,
    /// Event topics it may subscribe to.
    pub event_topics: Vec<String>,
    /// Event topics it may publish. Providing, but with no registration step —
    /// an emit is the registration and the use at once.
    pub published_topics: Vec<String>,
    /// Model providers it may register an adapter for.
    pub llm_providers: Vec<String>,
    /// Tools it may offer to the model.
    pub tools: Vec<String>,
    /// Slash commands it may register, by name.
    ///
    /// Names only, like [`Self::tools`], and for the same reason: what a
    /// command *is* — its description, its argument hint, which surfaces it
    /// works on — is how it describes itself at registration, while the name
    /// is the part a person reviews before installing. A command name is also
    /// the thing that can collide with a built-in, which makes it exactly the
    /// part that belongs in the reviewable ceiling.
    pub commands: Vec<String>,
    /// Tools it may *invoke*: either a list, or [`ALL_REBON_TOOLS`].
    ///
    /// The consuming direction, so it has no registered counterpart — a plugin
    /// calls these, it does not provide them.
    pub invokable_tools: Value,
    /// Kernel seats it may call.
    pub seats: Vec<String>,
    /// Settings keys the plugin owns under `plugins.<id>` in the
    /// `settings.json` chain.
    ///
    /// The consuming direction has a `seats` entry (`"settings"`); this is the
    /// *owning* one, and it is a ceiling in the same sense as every other list
    /// here: a plugin writes the keys it declared and nothing else, so a typo
    /// is a refusal its author sees rather than a setting nothing reads. What
    /// the keys mean is the plugin's business; what they are called, and
    /// roughly what shape they hold, is the part a person reviews before
    /// installing.
    pub settings: Vec<KernelPluginSettingKey>,
}

/// One declared settings key, as a manifest writes it.
///
/// A JSON Schema subset on purpose — name, type, default — because the point
/// is a declaration a person can read, not a validator. The type is kept as
/// written rather than parsed here: this crate is the vocabulary both the
/// installer and the plane read, and the meaning of a type name belongs to the
/// seat that enforces it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KernelPluginSettingKey {
    pub name: String,
    /// `boolean` / `number` / `string` / `array` / `object` / `any`. Absent
    /// means `any`: the key exists, and its shape is the plugin's business.
    #[serde(default, rename = "type")]
    pub ty: Option<String>,
    /// What the key reads as when no settings file sets it.
    #[serde(default)]
    pub default: Option<Value>,
}

impl KernelPluginManifest {
    /// The tools this plugin may invoke, with the sentinel expanded.
    ///
    /// Anything that is neither a list nor the sentinel resolves to nothing,
    /// which is the fail-closed reading: a manifest rebon cannot understand
    /// grants no reach.
    pub fn invokable(&self, all_tools: &[String]) -> Vec<String> {
        match &self.invokable_tools {
            Value::String(sentinel) if sentinel == ALL_REBON_TOOLS => all_tools.to_vec(),
            Value::Array(list) => list
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Checks what a package is allowed to say about itself.
    ///
    /// Two things a package may not do: leave out the module it wants loaded,
    /// and point that module at one of rebon's own packages.
    pub fn validate_for_package(&self, name: &str) -> Result<(), String> {
        if !self.root.is_package_local() {
            return Err(format!(
                "kernel plugin {name:?} declares root {:?}, which names one of rebon's own \
                 packages; a package's entry is relative to itself",
                self.root
            ));
        }
        match self.entry.as_deref().map(str::trim) {
            None | Some("") => Err(format!("kernel plugin {name:?} names no entry module")),
            Some(_) => Ok(()),
        }?;
        for key in &self.settings {
            if key.name.trim().is_empty() {
                return Err(format!(
                    "kernel plugin {name:?} declares a settings key with no name"
                ));
            }
            // `enabled` is the kernel's switch. A package that could declare it
            // would be a package that can switch itself on.
            if key.name.trim() == "enabled" {
                return Err(format!(
                    "kernel plugin {name:?} declares `enabled`, which is the kernel's switch \
                     and not one of the plugin's keys"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sentinel_expands_and_a_list_is_taken_as_written() {
        let all = vec!["Read".to_string(), "Write".to_string()];

        let sentinel = KernelPluginManifest {
            invokable_tools: Value::from(ALL_REBON_TOOLS),
            ..KernelPluginManifest::default()
        };
        assert_eq!(sentinel.invokable(&all), all);

        let listed = KernelPluginManifest {
            invokable_tools: serde_json::json!(["Read"]),
            ..KernelPluginManifest::default()
        };
        assert_eq!(listed.invokable(&all), vec!["Read".to_string()]);
    }

    #[test]
    fn an_unreadable_invokable_field_grants_nothing() {
        let all = vec!["Read".to_string()];
        let confused = KernelPluginManifest {
            invokable_tools: serde_json::json!({ "all": true }),
            ..KernelPluginManifest::default()
        };
        assert!(confused.invokable(&all).is_empty());
    }

    #[test]
    fn a_package_may_not_point_its_entry_at_rebons_own_packages() {
        let manifest = KernelPluginManifest {
            root: KernelPluginRoot::Payload,
            entry: Some("index.mjs".into()),
            ..KernelPluginManifest::default()
        };
        let error = manifest.validate_for_package("demo").unwrap_err();
        assert!(error.contains("rebon's own packages"), "{error}");
    }

    #[test]
    fn a_declaration_without_a_module_is_refused() {
        let manifest = KernelPluginManifest::default();
        assert!(manifest
            .validate_for_package("demo")
            .unwrap_err()
            .contains("no entry module"));
    }

    #[test]
    fn a_package_may_not_declare_the_kernels_switch_as_one_of_its_keys() {
        let manifest: KernelPluginManifest = serde_json::from_value(serde_json::json!({
            "entry": "index.mjs",
            "settings": [{ "name": "enabled", "type": "boolean" }]
        }))
        .expect("parses");
        let error = manifest.validate_for_package("demo").unwrap_err();
        assert!(error.contains("kernel's switch"), "{error}");
    }

    #[test]
    fn a_settings_declaration_parses_with_and_without_a_type() {
        let manifest: KernelPluginManifest = serde_json::from_value(serde_json::json!({
            "entry": "index.mjs",
            "seats": ["settings"],
            "settings": [
                { "name": "greeting", "type": "string", "default": "hi" },
                { "name": "anything" }
            ]
        }))
        .expect("parses");
        manifest.validate_for_package("demo").expect("valid");
        assert_eq!(manifest.settings[0].ty.as_deref(), Some("string"));
        assert_eq!(manifest.settings[0].default, Some(Value::from("hi")));
        assert_eq!(manifest.settings[1].ty, None);
    }

    #[test]
    fn the_vendored_table_shape_still_parses() {
        // Verbatim from runtimes/node/compose-runtime/payload-manifests.json.
        let entry: KernelPluginManifest = serde_json::from_value(serde_json::json!({
            "entry": "vendor/dsh/llm-deepseek.js",
            "llmProviders": ["deepseek-official"],
            "seats": ["credentials"]
        }))
        .expect("the table shape parses");
        assert_eq!(entry.root, KernelPluginRoot::Package);
        assert_eq!(entry.llm_providers, vec!["deepseek-official".to_string()]);
    }
}
