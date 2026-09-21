use std::collections::BTreeMap;
use std::path::Path;

pub use crate::model_provider::{
    MaterializedModelProviderTransport, MaterializedPluginModelProviderTransport,
    ModelProviderCapabilityManifest, ModelProviderModelManifest, PluginModelProviderContribution,
};
use anyhow::{anyhow, bail};
use rebon_types::ModelProfileMap;
use serde::{Deserialize, Serialize};

use crate::security::{ensure_path_inside_root, validate_identifier, validate_relative_asset_path};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderManifest {
    pub transport: ModelProviderTransportManifest,
    #[serde(default)]
    pub capabilities: ModelProviderCapabilityManifest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelProviderModelManifest>,
    #[serde(default, skip_serializing_if = "ModelProfileMap::is_empty")]
    pub profiles: ModelProfileMap,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ModelProviderTransportManifest {
    /// A module on the plugin plane, loaded into the Node host every other
    /// plugin runs in.
    ///
    /// The provider is an `llm/stream` adapter: it registers under its
    /// provider id, is handed one turn at a time, and answers with the same
    /// `StreamEventV1` vocabulary the old child process spoke. What it stops
    /// being is a process of its own — no second protocol to keep in step, no
    /// per-provider spawn, and the same drain, cancel and failure accounting
    /// every other plugin already gets.
    Plugin {
        /// The module inside the package, relative to its root.
        entry: String,
    },
    /// The retired child-process protocol.
    ///
    /// Still parsed, deliberately: a package written against it is on disk
    /// somewhere, and refusing to *read* the record would take every other
    /// plugin in the same `installed.json` down with it. It is refused when it
    /// is validated or materialised instead, where the message can say what to
    /// change.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initialize_timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shutdown_timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_stdout_line_bytes: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_stderr_bytes: Option<usize>,
    },
}

impl ModelProviderManifest {
    pub fn validate(
        &self,
        provider_id: &str,
        root: Option<&Path>,
        external_commands: &[String],
    ) -> anyhow::Result<()> {
        validate_identifier("model provider id", provider_id)?;
        if matches!(provider_id, "openai" | "openai-responses" | "anthropic") {
            bail!("model provider id `{provider_id}` is reserved for Rebon built-ins");
        }
        if let Some(model) = self.default_model.as_deref() {
            validate_model_id("defaultModel", model)?;
        }
        for model in self.models.keys() {
            validate_model_id("model id", model)?;
        }
        let _ = external_commands;
        match &self.transport {
            ModelProviderTransportManifest::Plugin { entry } => {
                // The same rule every other packaged module follows: relative,
                // and not out of the package. A provider naming a module
                // outside its own root would be running code the package did
                // not ship and the user did not review.
                let relative = validate_relative_asset_path(entry).map_err(|error| {
                    anyhow!("invalid model provider `{provider_id}` entry `{entry}`: {error}")
                })?;
                if let Some(root) = root {
                    // Joined, not handed over relative: `ensure_path_inside_root`
                    // canonicalises what it is given, and a relative path
                    // canonicalises against the process's working directory —
                    // which is not the package.
                    ensure_path_inside_root(root, &root.join(&relative)).map_err(|error| {
                        anyhow!("invalid model provider `{provider_id}` entry `{entry}`: {error}")
                    })?;
                }
            }
            ModelProviderTransportManifest::Stdio { .. } => bail!(
                "model provider `{provider_id}` uses the retired `stdio` transport. Rebon runs \
                 provider plugins on the plugin plane now: replace the transport with \
                 {{\"type\": \"plugin\", \"entry\": \"<module>.mjs\"}} and export an `activate` \
                 that calls `api.llm(\"{provider_id}\", handler, info)`."
            ),
        }
        Ok(())
    }
}

pub fn materialize_model_provider_contribution(
    plugin_name: &str,
    provider_id: &str,
    manifest: &ModelProviderManifest,
    root: &Path,
    source: &str,
    rebon_exe: &Path,
) -> anyhow::Result<PluginModelProviderContribution> {
    // Kept in the signature and unused in the body: a plugin entry is a module
    // inside the package, so there is no command line left for `{rebon}` to
    // expand into. Callers pass it; removing it would churn every one of them
    // for a parameter the next transport may well want back.
    let _ = rebon_exe;
    let transport = match &manifest.transport {
        ModelProviderTransportManifest::Plugin { entry } => {
            MaterializedModelProviderTransport::Plugin(MaterializedPluginModelProviderTransport {
                root: root.to_path_buf(),
                entry: entry.clone(),
            })
        }
        // Unreachable through `validate`, which refuses it first. Kept as a
        // second refusal rather than an `unreachable!` because materialising is
        // a public entry point and a caller that skipped validation should get
        // the same sentence, not a panic.
        ModelProviderTransportManifest::Stdio { .. } => bail!(
            "model provider `{provider_id}` uses the retired `stdio` transport; declare a \
             `plugin` transport with an `entry` module instead"
        ),
    };

    Ok(PluginModelProviderContribution {
        id: provider_id.to_string(),
        plugin_name: plugin_name.to_string(),
        source: source.to_string(),
        display_name: manifest.display_name.clone(),
        transport,
        capabilities: manifest.capabilities.clone(),
        default_model: manifest.default_model.clone(),
        models: manifest.models.clone(),
        profiles: manifest.profiles.clone(),
    })
}

fn validate_model_id(kind: &str, value: &str) -> anyhow::Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{kind} must not be empty");
    }
    if trimmed.contains('\0') {
        bail!("{kind} `{trimmed}` contains a NUL byte");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin_manifest(entry: &str) -> ModelProviderManifest {
        ModelProviderManifest {
            transport: ModelProviderTransportManifest::Plugin {
                entry: entry.into(),
            },
            capabilities: ModelProviderCapabilityManifest {
                request_scoped_transient_context: true,
                computer_use: true,
                ..Default::default()
            },
            default_model: Some("fake-model".into()),
            models: BTreeMap::from([("fake-model".into(), ModelProviderModelManifest::default())]),
            profiles: ModelProfileMap::from_entries([("small", "fake-small")]),
            display_name: Some("Fake".into()),
        }
    }

    #[test]
    fn validates_and_materializes_a_plugin_provider() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("provider.mjs"),
            "export function activate() {}",
        )
        .unwrap();
        let manifest = plugin_manifest("provider.mjs");
        manifest
            .validate("fake-provider", Some(tmp.path()), &[])
            .unwrap();
        let contribution = materialize_model_provider_contribution(
            "fake-plugin",
            "fake-provider",
            &manifest,
            tmp.path(),
            "plugin:fake",
            Path::new("rebon"),
        )
        .unwrap();
        assert_eq!(contribution.default_model.as_deref(), Some("fake-model"));
        assert!(contribution.capabilities.computer_use);
        match contribution.transport {
            MaterializedModelProviderTransport::Plugin(plugin) => {
                assert_eq!(plugin.entry, "provider.mjs");
                assert_eq!(plugin.root, tmp.path());
            }
        }
    }

    /// A provider naming a module outside its package would run code the
    /// package never shipped.
    #[test]
    fn refuses_an_entry_outside_the_package() {
        let tmp = tempfile::tempdir().unwrap();
        let err = plugin_manifest("../elsewhere.mjs")
            .validate("fake-provider", Some(tmp.path()), &[])
            .unwrap_err();
        assert!(err.to_string().contains("entry"), "{err}");
    }

    /// The retired transport still parses — a record carrying one must not
    /// take the rest of `installed.json` down — and is refused where the
    /// message can say what to change.
    #[test]
    fn the_retired_stdio_transport_parses_and_is_refused_with_instructions() {
        let manifest: ModelProviderManifest = serde_json::from_value(serde_json::json!({
            "transport": { "type": "stdio", "command": "node", "args": ["p.mjs"] }
        }))
        .expect("an old manifest still reads");
        let err = manifest
            .validate("legacy-provider", None, &["node".to_string()])
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("retired"), "{message}");
        assert!(message.contains("\"entry\""), "{message}");
    }

    #[test]
    fn rejects_reserved_provider_id() {
        let manifest = ModelProviderManifest {
            transport: ModelProviderTransportManifest::Plugin {
                entry: "provider.mjs".into(),
            },
            capabilities: Default::default(),
            default_model: None,
            models: BTreeMap::new(),
            profiles: ModelProfileMap::default(),
            display_name: None,
        };
        let err = manifest.validate("openai", None, &[]).unwrap_err();
        assert!(err.to_string().contains("reserved"));
    }
}
