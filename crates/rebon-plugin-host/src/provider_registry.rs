//! Which model providers this build can connect to, and how it connects.
//!
//! Two kinds of entry: the three built-in wire formats, and whatever a plugin
//! package contributes. Everything about a *built-in* would sit happily in
//! `rebon-provider` next to the catalog and the runtime cache. The second kind
//! is why this module is here instead: an external provider is a plugin loaded
//! on the Node plane, so building its client means starting a plane, and a
//! registry in `rebon-provider` that could do that would need to name
//! `rebon-plugin-host` — which names `rebon-provider` back for the adapter and
//! the client type. The module sits on the plane side of that edge rather
//! than inventing a callback to carry it across (the house rule: the dependency
//! direction decides, no trait adapter layers).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rebon_api::{
    anthropic_client, openai_compatible_client, AnthropicClientConfig, ModelClient,
    OpenAiCompatibleClientConfig, OpenAiResponsesClientConfig, OpenAiResponsesProvider,
    ServiceTierHandle, UniversalModelClient,
};
use rebon_config::{ProviderFormat, ProviderSelection, ResolvedProvider};
use rebon_types::ModelProfileMap;

use rebon_provider::model_provider_plugin::{
    ModelProviderCapabilityManifest, PluginModelProviderContribution,
};

pub const BUILTIN_OPENAI_PROVIDER_ID: &str = "openai";
pub const BUILTIN_OPENAI_RESPONSES_PROVIDER_ID: &str = "openai-responses";
pub const BUILTIN_ANTHROPIC_PROVIDER_ID: &str = "anthropic";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub request_scoped_transient_context: bool,
    pub forced_tool_choice: bool,
    pub web_search: bool,
    pub computer_use: bool,
    pub context_management: bool,
    pub stateful_responses: bool,
    pub remote_compaction: bool,
    pub anchored_minimal: bool,
    pub reasoning_text: bool,
    pub custom_tool_call: bool,
}

impl From<&ModelProviderCapabilityManifest> for ProviderCapabilities {
    fn from(value: &ModelProviderCapabilityManifest) -> Self {
        Self {
            request_scoped_transient_context: value.request_scoped_transient_context,
            forced_tool_choice: value.forced_tool_choice,
            web_search: value.web_search,
            computer_use: value.computer_use,
            context_management: value.context_management,
            stateful_responses: value.stateful_responses,
            remote_compaction: value.remote_compaction,
            anchored_minimal: value.anchored_minimal,
            reasoning_text: value.reasoning_text,
            custom_tool_call: value.custom_tool_call,
        }
    }
}

#[derive(Clone)]
pub struct ProviderRegistry {
    entries: BTreeMap<String, ProviderRegistryEntry>,
}

#[derive(Clone)]
pub struct ProviderRegistryEntry {
    pub id: String,
    pub display_name: String,
    pub capabilities: ProviderCapabilities,
    pub kind: ProviderRegistryEntryKind,
}

#[derive(Clone)]
pub enum ProviderRegistryEntryKind {
    BuiltIn(ProviderFormat),
    External(PluginModelProviderContribution),
}

#[derive(Debug, Clone)]
pub struct ProviderBuildContext {
    pub config_dir: PathBuf,
    pub retry_notifier: Option<rebon_api::RetryNotifier>,
    pub service_tier: Option<ServiceTierHandle>,
}

impl ProviderRegistry {
    pub fn with_builtins() -> Self {
        let mut registry = Self {
            entries: BTreeMap::new(),
        };
        registry.insert_builtin(BUILTIN_OPENAI_PROVIDER_ID, "OpenAI", ProviderFormat::Openai);
        registry.insert_builtin(
            BUILTIN_OPENAI_RESPONSES_PROVIDER_ID,
            "OpenAI Responses",
            ProviderFormat::OpenaiResponses,
        );
        registry.insert_builtin(
            BUILTIN_ANTHROPIC_PROVIDER_ID,
            "Anthropic",
            ProviderFormat::Anthropic,
        );
        registry
    }

    fn insert_builtin(&mut self, id: &str, display_name: &str, format: ProviderFormat) {
        self.entries.insert(
            id.to_string(),
            ProviderRegistryEntry {
                id: id.to_string(),
                display_name: display_name.to_string(),
                capabilities: builtin_capabilities(format),
                kind: ProviderRegistryEntryKind::BuiltIn(format),
            },
        );
    }

    pub fn register_plugin_provider(
        &mut self,
        contribution: PluginModelProviderContribution,
    ) -> Result<(), String> {
        let id = contribution.id.trim();
        if is_reserved_provider_id(id) {
            return Err(format!(
                "plugin model provider `{id}` from {} is ignored because `{id}` is reserved for a built-in provider",
                contribution.source
            ));
        }
        if self.entries.contains_key(id) {
            return Err(format!(
                "plugin model provider `{id}` from {} is ignored because another provider already registered that id",
                contribution.source
            ));
        }
        self.entries.insert(
            id.to_string(),
            ProviderRegistryEntry {
                id: id.to_string(),
                display_name: contribution
                    .display_name
                    .clone()
                    .unwrap_or_else(|| id.to_string()),
                capabilities: ProviderCapabilities::from(&contribution.capabilities),
                kind: ProviderRegistryEntryKind::External(contribution),
            },
        );
        Ok(())
    }

    #[cfg(test)]
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    pub fn external_provider_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|(id, entry)| match entry.kind {
                ProviderRegistryEntryKind::External(_) => Some(id.clone()),
                ProviderRegistryEntryKind::BuiltIn(_) => None,
            })
            .collect()
    }

    pub fn default_model_for(&self, selection: &ProviderSelection) -> Option<&str> {
        match self.entries.get(selection.id()).map(|entry| &entry.kind) {
            Some(ProviderRegistryEntryKind::External(contribution)) => {
                contribution.default_model.as_deref()
            }
            Some(ProviderRegistryEntryKind::BuiltIn(ProviderFormat::OpenaiResponses)) => {
                Some(rebon_config::OPENAI_OAUTH_PROVIDER_MODEL)
            }
            _ => None,
        }
    }

    pub fn model_profiles_for(&self, selection: &ProviderSelection) -> Option<ModelProfileMap> {
        match self.entries.get(selection.id()).map(|entry| &entry.kind) {
            Some(ProviderRegistryEntryKind::External(contribution)) => {
                Some(contribution.profiles.clone())
            }
            _ => None,
        }
    }

    pub fn model_context_windows_for(
        &self,
        selection: &ProviderSelection,
    ) -> BTreeMap<String, u32> {
        match self.entries.get(selection.id()).map(|entry| &entry.kind) {
            Some(ProviderRegistryEntryKind::External(contribution)) => contribution
                .models
                .iter()
                .filter_map(|(id, details)| {
                    details.context_window.map(|window| (id.clone(), window))
                })
                .collect(),
            _ => BTreeMap::new(),
        }
    }

    pub fn capabilities_for(&self, selection: &ProviderSelection) -> ProviderCapabilities {
        self.entries
            .get(selection.id())
            .map(|entry| entry.capabilities.clone())
            .unwrap_or_default()
    }

    pub fn lookup_diagnostic(&self, selection: &ProviderSelection) -> String {
        if self.entries.contains_key(selection.id()) {
            return format!("provider `{}` is registered", selection.id());
        }
        let available = self.entries.keys().cloned().collect::<Vec<_>>().join(", ");
        match selection {
            ProviderSelection::BuiltIn(format) => format!(
                "built-in provider `{}` is not registered (available: [{}])",
                format.as_str(), available
            ),
            ProviderSelection::External(id) => format!(
                "plugin provider `{id}` is selected but no enabled compatible plugin contribution provides it (available: [{available}]); enable/install the plugin or choose a built-in provider"
            ),
        }
    }

    /// Connect to one provider.
    ///
    /// `plugins` is the process registry, needed only by the external arm: a
    /// package's provider is a plugin *loaded on the plane*, and starting a
    /// plane means forking the kernel. That one edge is also why this module
    /// lives in `rebon-plugin-host` rather than `rebon-provider` — see the
    /// module header.
    pub async fn build_client(
        &self,
        plugins: &Arc<rebon_kernel::PluginRegistry>,
        resolved: ResolvedProvider,
        context: ProviderBuildContext,
    ) -> anyhow::Result<Arc<dyn ModelClient>> {
        let id = resolved.provider_selection.id().to_string();
        let entry = self
            .entries
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!(self.lookup_diagnostic(&resolved.provider_selection)))?;
        match &entry.kind {
            ProviderRegistryEntryKind::BuiltIn(format) => {
                build_builtin_client(*format, resolved, context)
            }
            ProviderRegistryEntryKind::External(contribution) => {
                let connection = provider_connection_config(&resolved);
                let client = crate::kernel_node_host::bind_plane_model_provider(
                    plugins,
                    contribution,
                    connection,
                )
                .await
                .map_err(|err| {
                    anyhow::anyhow!(
                        "plugin model provider `{}` from {} is selected but unavailable: {err}",
                        contribution.id,
                        contribution.source
                    )
                })?;
                Ok(Arc::new(client))
            }
        }
    }
}

fn builtin_capabilities(format: ProviderFormat) -> ProviderCapabilities {
    match format {
        // Chat Completions deliberately has no `computer_use`: the gate below
        // admits only the Responses-format OAuth route, so advertising it here
        // would be a capability that never resolves to anything.
        ProviderFormat::Openai => ProviderCapabilities {
            request_scoped_transient_context: true,
            forced_tool_choice: true,
            ..Default::default()
        },
        ProviderFormat::OpenaiResponses => ProviderCapabilities {
            request_scoped_transient_context: true,
            forced_tool_choice: true,
            web_search: true,
            computer_use: true,
            ..Default::default()
        },
        ProviderFormat::Anthropic => ProviderCapabilities {
            request_scoped_transient_context: true,
            forced_tool_choice: false,
            web_search: true,
            context_management: true,
            ..Default::default()
        },
    }
}

/// Return whether this resolved runtime is the synthetic OpenAI Codex OAuth
/// provider allowed to receive the local Computer Use tool.
///
/// Resolution replaces the OAuth sentinel with the live access token, so
/// `has_codex_login()` is the retained proof that the configured `apiKey` was
/// the ChatGPT login's sentinel — not merely some account login's. The
/// canonical name, Responses format, built-in selection, and canonical Codex
/// base URL prevent custom compatible and external providers from inheriting
/// the capability.
pub fn computer_use_enabled_for_provider(
    resolved: &ResolvedProvider,
    capabilities: &ProviderCapabilities,
) -> bool {
    capabilities.computer_use
        && resolved.name == rebon_config::OPENAI_OAUTH_PROVIDER_NAME
        && resolved.format == ProviderFormat::OpenaiResponses
        && matches!(
            resolved.provider_selection,
            ProviderSelection::BuiltIn(ProviderFormat::OpenaiResponses)
        )
        && resolved.has_codex_login()
        && resolved
            .base_url
            .trim_end_matches('/')
            .eq_ignore_ascii_case(
                rebon_config::OPENAI_OAUTH_PROVIDER_BASE_URL.trim_end_matches('/'),
            )
}

pub fn is_reserved_provider_id(id: &str) -> bool {
    matches!(
        id,
        BUILTIN_OPENAI_PROVIDER_ID
            | BUILTIN_OPENAI_RESPONSES_PROVIDER_ID
            | BUILTIN_ANTHROPIC_PROVIDER_ID
    )
}

fn build_builtin_client(
    format: ProviderFormat,
    resolved: ResolvedProvider,
    context: ProviderBuildContext,
) -> anyhow::Result<Arc<dyn ModelClient>> {
    match format {
        ProviderFormat::Openai => {
            let mut config = OpenAiCompatibleClientConfig::with_api_key(resolved.api_key);
            if !resolved.base_url.is_empty() {
                config.base_url = resolved.base_url;
            }
            config.extra_headers = resolved.extra_headers;
            config.request_options = resolved.request_options;
            config.model_request_options = resolved.model_request_options;
            config.service_tier = context.service_tier;
            config.request_scoped_transient_context = resolved.request_scoped_transient_context;
            // The entry's vendor (pinned, or recognised from the host)
            // decides the wire dialect per request — thinking switch,
            // reasoning replay, output-cap field, rejected fields — so the
            // client is told who it is talking to rather than handed one
            // DeepSeek-shaped compat flag. See `rebon_api::vendor`.
            config.vendor = resolved.vendor;
            // An account login's bearer can be short-lived (a Copilot
            // session lasts half an hour); the client renews it on a 401.
            config.refresher = resolved.oauth.as_ref().map(|oauth| {
                Arc::new(
                    rebon_provider::oauth_refresher::RebonOAuthRefresher::for_login(
                        context.config_dir.clone(),
                        oauth,
                    ),
                ) as Arc<dyn rebon_api::TokenRefresher>
            });
            Ok(Arc::new(openai_compatible_client(config)))
        }
        ProviderFormat::OpenaiResponses => {
            let mut config = OpenAiResponsesClientConfig::with_api_key(resolved.api_key.clone());
            if !resolved.base_url.is_empty() {
                config.base_url = resolved.base_url;
            }
            config.use_websocket = resolved.use_websocket;
            config.extra_headers = resolved.extra_headers;
            config.service_tier = context.service_tier;
            let provider = OpenAiResponsesProvider::new(config);
            let provider = if let Some(rn) = context.retry_notifier {
                provider.with_retry_notifier(rn)
            } else {
                provider
            };
            let provider = if let Some(oauth) = resolved.oauth {
                let refresher = rebon_provider::oauth_refresher::RebonOAuthRefresher::for_login(
                    context.config_dir,
                    &oauth,
                );
                provider.with_refresher(Arc::new(refresher))
            } else {
                provider
            };
            Ok(Arc::new(UniversalModelClient::new(Arc::new(provider))))
        }
        ProviderFormat::Anthropic => {
            let mut config = AnthropicClientConfig::with_api_key(resolved.api_key);
            if !resolved.base_url.is_empty() {
                config.base_url = resolved.base_url;
            }
            config.request_scoped_transient_context = resolved.request_scoped_transient_context;
            Ok(Arc::new(anthropic_client(config)))
        }
    }
}

/// Project the user's resolved provider entry (settings-page apiKey/baseUrl,
/// `options.headers`, request body options) into the protocol DTO handed to an
/// external plugin at initialize time. `None` when the entry configured
/// nothing, so a plugin can distinguish "no config" from "empty config".
fn provider_connection_config(
    resolved: &ResolvedProvider,
) -> Option<rebon_api::model_provider_protocol::ProviderConnectionConfigV1> {
    let connection = rebon_api::model_provider_protocol::ProviderConnectionConfigV1 {
        base_url: resolved.base_url.trim().to_string(),
        api_key: resolved.api_key.trim().to_string(),
        headers: resolved
            .extra_headers
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        request_options: request_options_value(&resolved.request_options),
        model_request_options: resolved
            .model_request_options
            .iter()
            .filter_map(|(model, options)| {
                request_options_value(options).map(|value| (model.clone(), value))
            })
            .collect(),
    };
    (!connection.is_empty()).then_some(connection)
}

fn request_options_value(options: &rebon_api::OpenAiRequestOptions) -> Option<serde_json::Value> {
    if options.is_empty() {
        return None;
    }
    let mut map = serde_json::Map::new();
    if !options.body.is_empty() {
        map.insert(
            "body".into(),
            serde_json::Value::Object(options.body.clone()),
        );
    }
    if !options.extra_body.is_empty() {
        map.insert(
            "extraBody".into(),
            serde_json::Value::Object(options.extra_body.clone()),
        );
    }
    if !options.omit_body_fields.is_empty() {
        map.insert(
            "omitBodyFields".into(),
            serde_json::Value::Array(
                options
                    .omit_body_fields
                    .iter()
                    .map(|field| serde_json::Value::String(field.clone()))
                    .collect(),
            ),
        );
    }
    Some(serde_json::Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_provider::model_provider_plugin::{
        MaterializedModelProviderTransport, MaterializedPluginModelProviderTransport,
        ModelProviderModelManifest,
    };

    fn dummy_contribution(id: &str) -> PluginModelProviderContribution {
        PluginModelProviderContribution {
            id: id.into(),
            plugin_name: "plugin".into(),
            source: "plugin:plugin@test".into(),
            display_name: None,
            transport: MaterializedModelProviderTransport::Plugin(
                MaterializedPluginModelProviderTransport {
                    root: std::path::PathBuf::from("/plugins/plugin"),
                    entry: "provider.mjs".into(),
                },
            ),
            capabilities: ModelProviderCapabilityManifest {
                forced_tool_choice: true,
                ..Default::default()
            },
            default_model: Some("fake-default".into()),
            models: BTreeMap::from([(
                "fake-default".into(),
                ModelProviderModelManifest {
                    context_window: Some(123),
                    display_name: None,
                },
            )]),
            profiles: ModelProfileMap::from_entries([("small", "fake-small")]),
        }
    }

    #[test]
    fn builtins_are_registered_and_reserved() {
        let registry = ProviderRegistry::with_builtins();
        assert!(registry.contains("openai"));
        assert!(registry.contains("openai-responses"));
        assert!(registry.contains("anthropic"));
        assert!(is_reserved_provider_id("openai"));
    }

    fn resolve_test_provider(
        name: &str,
        format: &str,
        base_url: &str,
        api_key: &str,
        external_provider_ids: &[&str],
    ) -> ResolvedProvider {
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let config = serde_json::json!({
            "activeCustomProvider": name,
            "customProviders": [{
                "name": name,
                "format": format,
                "baseUrl": base_url,
                "apiKey": api_key,
                "model": "gpt-test"
            }]
        });
        std::fs::write(
            config_dir.path().join("config.json"),
            serde_json::to_vec(&config).expect("serialize config"),
        )
        .expect("write config");
        std::fs::write(
            config_dir.path().join(".credentials.json"),
            br#"{"openaiOAuth":{"accessToken":"oauth-access-token","refreshToken":"oauth-refresh-token","expiresAt":4102444800000}}"#,
        )
        .expect("write credentials");

        rebon_config::resolve_from_dir_with_external_provider_ids(
            config_dir.path(),
            None,
            external_provider_ids.iter().copied(),
        )
        .expect("resolve provider")
        .expect("active provider")
    }

    #[test]
    fn computer_use_accepts_real_openai_oauth_responses_config() {
        let resolved = resolve_test_provider(
            rebon_config::OPENAI_OAUTH_PROVIDER_NAME,
            ProviderFormat::OpenaiResponses.as_str(),
            rebon_config::OPENAI_OAUTH_PROVIDER_BASE_URL,
            rebon_config::OPENAI_OAUTH_TOKEN_SENTINEL,
            &[],
        );
        assert_eq!(resolved.name, "openai");
        assert_eq!(resolved.format, ProviderFormat::OpenaiResponses);
        assert_eq!(
            resolved.provider_selection,
            ProviderSelection::BuiltIn(ProviderFormat::OpenaiResponses)
        );
        assert_eq!(resolved.api_key, "oauth-access-token");
        assert!(resolved.oauth.is_some());

        let registry = ProviderRegistry::with_builtins();
        assert!(computer_use_enabled_for_provider(
            &resolved,
            &registry.capabilities_for(&resolved.provider_selection)
        ));
    }

    #[test]
    fn computer_use_rejects_same_named_custom_openai_providers() {
        let registry = ProviderRegistry::with_builtins();

        let chat_completions = resolve_test_provider(
            "openai",
            ProviderFormat::Openai.as_str(),
            "https://openai-compatible.example/v1",
            "custom-api-key",
            &[],
        );
        assert_eq!(
            chat_completions.provider_selection,
            ProviderSelection::BuiltIn(ProviderFormat::Openai)
        );
        assert!(!computer_use_enabled_for_provider(
            &chat_completions,
            &registry.capabilities_for(&chat_completions.provider_selection)
        ));

        let responses_with_literal_key = resolve_test_provider(
            "openai",
            ProviderFormat::OpenaiResponses.as_str(),
            rebon_config::OPENAI_OAUTH_PROVIDER_BASE_URL,
            "custom-api-key",
            &[],
        );
        assert!(responses_with_literal_key.oauth.is_none());
        assert!(!computer_use_enabled_for_provider(
            &responses_with_literal_key,
            &registry.capabilities_for(&responses_with_literal_key.provider_selection)
        ));

        let oauth_with_custom_base_url = resolve_test_provider(
            "openai",
            ProviderFormat::OpenaiResponses.as_str(),
            "https://openai-compatible.example/v1/responses",
            rebon_config::OPENAI_OAUTH_TOKEN_SENTINEL,
            &[],
        );
        assert!(oauth_with_custom_base_url.oauth.is_some());
        assert!(!computer_use_enabled_for_provider(
            &oauth_with_custom_base_url,
            &registry.capabilities_for(&oauth_with_custom_base_url.provider_selection)
        ));
    }

    /// Regression: an account login other than ChatGPT, dressed as the
    /// synthetic `openai` entry on the Codex host, is still not the Codex
    /// login.
    #[test]
    fn computer_use_rejects_another_account_login_on_the_codex_host() {
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let config = serde_json::json!({
            "activeCustomProvider": "openai",
            "customProviders": [{
                "name": "openai",
                "format": "openai-responses",
                "baseUrl": rebon_config::OPENAI_OAUTH_PROVIDER_BASE_URL,
                "apiKey": "$OAUTH:copilot",
                "model": "gpt-test"
            }]
        });
        std::fs::write(
            config_dir.path().join("config.json"),
            serde_json::to_vec(&config).expect("serialize config"),
        )
        .expect("write config");
        std::fs::write(
            config_dir.path().join(".credentials.json"),
            br#"{"oauthAccounts":{"copilot":{"accessToken":"gho","sessionToken":"s","sessionExpiresAt":4102444800000}}}"#,
        )
        .expect("write credentials");
        let resolved = rebon_config::resolve_from_dir(config_dir.path())
            .expect("resolve provider")
            .expect("active provider");
        assert!(resolved.oauth.is_some());
        assert!(!resolved.has_codex_login());

        let registry = ProviderRegistry::with_builtins();
        assert!(!computer_use_enabled_for_provider(
            &resolved,
            &registry.capabilities_for(&resolved.provider_selection)
        ));
    }

    #[test]
    fn computer_use_rejects_external_providers_even_when_they_advertise_capability() {
        let resolved = resolve_test_provider(
            "external-openai",
            ProviderFormat::OpenaiResponses.as_str(),
            rebon_config::OPENAI_OAUTH_PROVIDER_BASE_URL,
            rebon_config::OPENAI_OAUTH_TOKEN_SENTINEL,
            &["external-openai"],
        );
        assert_eq!(
            resolved.provider_selection,
            ProviderSelection::External("external-openai".into())
        );
        let advertised = ProviderCapabilities {
            computer_use: true,
            ..Default::default()
        };
        assert!(!computer_use_enabled_for_provider(&resolved, &advertised));
    }

    #[test]
    fn builds_connection_config_from_resolved_provider() {
        let resolved = resolve_test_provider(
            "external-x",
            ProviderFormat::Openai.as_str(),
            "https://api.example.test",
            "sk-abc",
            &["external-x"],
        );
        assert_eq!(
            resolved.provider_selection,
            ProviderSelection::External("external-x".into())
        );
        let connection = provider_connection_config(&resolved).expect("connection config");
        assert_eq!(connection.base_url, "https://api.example.test");
        assert_eq!(connection.api_key, "sk-abc");
        assert!(connection.headers.is_empty());
        assert!(connection.request_options.is_none());
    }

    #[test]
    fn rejects_plugin_shadowing_builtin() {
        let mut registry = ProviderRegistry::with_builtins();
        let err = registry
            .register_plugin_provider(dummy_contribution("openai"))
            .unwrap_err();
        assert!(err.contains("reserved"));
        assert!(registry.contains("openai"));
    }

    #[test]
    fn registers_plugin_metadata_for_lookup() {
        let mut registry = ProviderRegistry::with_builtins();
        registry
            .register_plugin_provider(dummy_contribution("fake-provider"))
            .unwrap();
        let selection = ProviderSelection::External("fake-provider".into());
        assert_eq!(registry.default_model_for(&selection), Some("fake-default"));
        assert_eq!(
            registry.model_context_windows_for(&selection)["fake-default"],
            123
        );
        assert_eq!(
            registry
                .model_profiles_for(&selection)
                .unwrap()
                .get("small"),
            Some("fake-small")
        );
    }
}
