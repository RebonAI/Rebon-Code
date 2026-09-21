//! Which provider, model and reasoning effort a sub-agent turn runs on.
//!
//! Two above this crate ask the question — the sub-agent spawner for every
//! spawn and the workflow runner for a script's `agent()` calls — and the front
//! end answers it once per session by building a [`ConfigurableModelRouter`]
//! over its provider runtime. Those two may not depend on each other, so the
//! answer lives here, below both of them, next to the agent turn seam it
//! decides for.
//!
//! [`resolve_sub_agent_model_with_config`] is the precedence itself, written
//! once: an `agents.json` selection beats the spec's own model, a model
//! profile resolves through the provider's profile map, and `inherit` /
//! `default` mean the parent's model rather than a name to look up.

use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use rebon_api::{ModelClient, ReasoningEffort};
use rebon_types::{ModelProfileMap, SubAgentModelConfig, SubAgentModelSelection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSubAgentModel {
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// `inherit` and `default` are reserved sentinels meaning "use the
/// parent/provider default" — never real provider, model, or profile
/// names. Models generalize the documented `model: "inherit"` form to
/// the sibling fields, so every resolution path must treat both words
/// as unset instead of looking them up literally.
pub fn is_inherit_sentinel(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.eq_ignore_ascii_case("inherit") || trimmed.eq_ignore_ascii_case("default")
}

/// Resolve agent frontmatter/tool-call model names before sending a
/// request to the provider. Only `inherit` and `default` are built in;
/// everything else is either looked up from config or forwarded as the
/// caller's explicit model string. This keeps provider/account-specific
/// model choices adjustable from config instead of code.
#[cfg(test)]
fn resolve_sub_agent_model(
    requested: Option<&str>,
    default_model: &str,
    provider_name: &str,
) -> String {
    resolve_sub_agent_model_with_config(
        requested,
        None,
        None,
        None,
        default_model,
        provider_name,
        provider_name,
        &SubAgentModelConfig::default(),
        &ModelProfileMap::default(),
    )
    .model
}

pub fn resolve_sub_agent_model_with_config(
    requested: Option<&str>,
    requested_profile: Option<&str>,
    agent_type: Option<&str>,
    category: Option<&str>,
    default_model: &str,
    provider_name: &str,
    client_provider_name: &str,
    config: &SubAgentModelConfig,
    profiles: &ModelProfileMap,
) -> ResolvedSubAgentModel {
    let selection = config.selection_for(agent_type, category, requested);

    let requested_model = selection
        .and_then(|selection| selection.model.as_deref())
        .map(ModelResolutionRequest::Model)
        .or_else(|| {
            selection
                .and_then(|selection| selection.model_profile.as_deref())
                .filter(|raw| !is_inherit_sentinel(raw))
                .map(ModelResolutionRequest::Profile)
        })
        .or_else(|| {
            requested
                .map(str::trim)
                .filter(|raw| !raw.is_empty())
                .map(ModelResolutionRequest::Model)
        })
        .or_else(|| {
            requested_profile
                .map(str::trim)
                .filter(|raw| !raw.is_empty() && !is_inherit_sentinel(raw))
                .map(ModelResolutionRequest::Profile)
        });
    let mut profile_reasoning_effort = None;
    let model = match requested_model {
        Some(ModelResolutionRequest::Model(raw)) if is_inherit_sentinel(raw) => {
            default_model.to_string()
        }
        Some(ModelResolutionRequest::Model(raw)) => {
            normalize_provider_model_id(raw, provider_name, client_provider_name)
        }
        Some(ModelResolutionRequest::Profile(profile)) => {
            let resolved =
                profiles.resolve_profile_selection(profile, Some(default_model), default_model);
            profile_reasoning_effort = resolved
                .reasoning_effort
                .as_deref()
                .and_then(parse_reasoning_effort);
            resolved.model
        }
        None => default_model.to_string(),
    };

    ResolvedSubAgentModel {
        model,
        reasoning_effort: selection
            .and_then(|selection| selection.reasoning_effort)
            .or(profile_reasoning_effort),
    }
}

enum ModelResolutionRequest<'a> {
    Model(&'a str),
    Profile(&'a str),
}

fn normalize_provider_model_id(
    raw: &str,
    provider_name: &str,
    client_provider_name: &str,
) -> String {
    let trimmed = raw.trim();
    let Some((prefix, suffix)) = trimmed.split_once('/') else {
        return trimmed.to_string();
    };
    if suffix.trim().is_empty() {
        return trimmed.to_string();
    }
    if provider_alias_matches(provider_name, client_provider_name, prefix) {
        suffix.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn provider_alias_matches(
    provider_name: &str,
    client_provider_name: &str,
    requested: &str,
) -> bool {
    let provider = normalize_provider_alias(provider_name);
    let client = normalize_provider_alias(client_provider_name);
    let requested = normalize_provider_alias(requested);
    requested == provider
        || requested == client
        || matches!(requested.as_str(), "openai")
            && matches!(
                client.as_str(),
                "openai-responses" | "openai-compatible" | "openai"
            )
}

fn normalize_provider_alias(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace('_', "-")
}

/// Thin `Option` adapter over [`ReasoningEffort::from_str`] for the
/// `and_then` chains in this crate; unknown spellings read as "unset".
pub fn parse_reasoning_effort(raw: &str) -> Option<ReasoningEffort> {
    raw.parse().ok()
}

#[derive(Debug, Clone, Default)]
pub struct ModelRouteRequest {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub model_profile: Option<String>,
    pub agent_type: Option<String>,
    pub category: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Clone)]
pub struct ResolvedModelRuntime {
    pub provider_name: String,
    pub client: Arc<dyn ModelClient>,
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl std::fmt::Debug for ResolvedModelRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModelRuntime")
            .field("provider_name", &self.provider_name)
            .field("client_provider", &self.client.provider_name())
            .field("model", &self.model)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

#[derive(Clone)]
pub struct ProviderModelRuntime {
    pub provider_name: String,
    pub client: Arc<dyn ModelClient>,
    pub default_model: String,
    pub model_profiles: ModelProfileMap,
}

impl std::fmt::Debug for ProviderModelRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderModelRuntime")
            .field("provider_name", &self.provider_name)
            .field("client_provider", &self.client.provider_name())
            .field("default_model", &self.default_model)
            .field("model_profiles_empty", &self.model_profiles.is_empty())
            .finish()
    }
}

#[async_trait]
pub trait ProviderRuntimeResolver: Send + Sync {
    async fn resolve_provider(&self, provider: Option<&str>) -> Result<ProviderModelRuntime>;
}

#[async_trait]
pub trait AgentModelRouter: Send + Sync {
    async fn resolve(&self, request: ModelRouteRequest) -> Result<ResolvedModelRuntime>;

    // 自动选型需要让装配层重建目标模型中间件，而不是沿用旧模型预算。
    async fn resolve_automatic(&self, request: ModelRouteRequest) -> Result<ResolvedModelRuntime> {
        self.resolve(request).await
    }
}

#[derive(Clone)]
pub struct ConfigurableModelRouter {
    provider_resolver: Arc<dyn ProviderRuntimeResolver>,
    model_config: SubAgentModelConfig,
}

impl ConfigurableModelRouter {
    pub fn new(provider_resolver: Arc<dyn ProviderRuntimeResolver>) -> Self {
        Self {
            provider_resolver,
            model_config: SubAgentModelConfig::default(),
        }
    }

    pub fn with_model_config(mut self, config: SubAgentModelConfig) -> Self {
        self.model_config = config;
        self
    }
}

fn config_model_profile(selection: Option<&SubAgentModelSelection>) -> Option<&str> {
    selection.and_then(|selection| {
        selection
            .model
            .is_none()
            .then(|| selection.model_profile.as_deref())
            .flatten()
    })
}

fn model_for_resolution<'a>(
    selection: Option<&SubAgentModelSelection>,
    requested_model: Option<&'a str>,
    routed_model: Option<&'a str>,
) -> Option<&'a str> {
    if selection.is_some() {
        requested_model
    } else {
        routed_model
    }
}

impl std::fmt::Debug for ConfigurableModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigurableModelRouter")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl AgentModelRouter for ConfigurableModelRouter {
    async fn resolve(&self, request: ModelRouteRequest) -> Result<ResolvedModelRuntime> {
        let requested_selection = self.model_config.selection_for(
            request.agent_type.as_deref(),
            request.category.as_deref(),
            request.model.as_deref(),
        );
        let (requested_provider, requested_model) = apply_model_provider_hint(
            request.provider.as_deref().or_else(|| {
                requested_selection.and_then(|selection| selection.provider.as_deref())
            }),
            requested_selection
                .and_then(|selection| selection.model.as_deref())
                .or(request.model.as_deref()),
        );
        let provider_runtime = self
            .provider_resolver
            .resolve_provider(requested_provider.as_deref())
            .await?;
        if let Some(provider) = requested_provider.as_deref() {
            if !provider_matches(
                &provider_runtime.provider_name,
                provider_runtime.client.provider_name(),
                provider,
            ) {
                bail!(
                    "sub-agent requested provider `{provider}`, but resolver returned provider `{}` (client `{}`)",
                    provider_runtime.provider_name,
                    provider_runtime.client.provider_name()
                );
            }
        }
        let requested_profile = config_model_profile(requested_selection)
            .or(request.model_profile.as_deref())
            .filter(|profile| !is_inherit_sentinel(profile));
        let requested_for_resolution = model_for_resolution(
            requested_selection,
            request.model.as_deref(),
            requested_model.as_deref(),
        );
        validate_profile(
            requested_profile,
            &provider_runtime.model_profiles,
            &provider_runtime.provider_name,
        )?;
        let resolved = resolve_sub_agent_model_with_config(
            requested_for_resolution,
            requested_profile,
            request.agent_type.as_deref(),
            request.category.as_deref(),
            &provider_runtime.default_model,
            &provider_runtime.provider_name,
            provider_runtime.client.provider_name(),
            &self.model_config,
            &provider_runtime.model_profiles,
        );
        Ok(ResolvedModelRuntime {
            provider_name: provider_runtime.provider_name,
            client: provider_runtime.client,
            model: resolved.model,
            reasoning_effort: resolved.reasoning_effort.or(request.reasoning_effort),
        })
    }
}

#[derive(Clone)]
pub struct SingleProviderModelRouter {
    provider_name: String,
    client: Arc<dyn ModelClient>,
    default_model: String,
    model_config: SubAgentModelConfig,
    model_profiles: ModelProfileMap,
}

impl SingleProviderModelRouter {
    pub fn new(client: Arc<dyn ModelClient>, default_model: impl Into<String>) -> Self {
        let provider_name = client.provider_name().to_string();
        Self {
            provider_name,
            client,
            default_model: default_model.into(),
            model_config: SubAgentModelConfig::default(),
            model_profiles: ModelProfileMap::default(),
        }
    }

    pub fn with_provider_name(mut self, provider_name: impl Into<String>) -> Self {
        self.provider_name = provider_name.into();
        self
    }

    pub fn with_model_config(mut self, config: SubAgentModelConfig) -> Self {
        self.model_config = config;
        self
    }

    pub fn with_model_profiles(mut self, profiles: ModelProfileMap) -> Self {
        self.model_profiles = profiles;
        self
    }
}

impl std::fmt::Debug for SingleProviderModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleProviderModelRouter")
            .field("provider_name", &self.provider_name)
            .field("client_provider", &self.client.provider_name())
            .field("default_model", &self.default_model)
            .finish()
    }
}

#[async_trait]
impl AgentModelRouter for SingleProviderModelRouter {
    async fn resolve(&self, request: ModelRouteRequest) -> Result<ResolvedModelRuntime> {
        let requested_selection = self.model_config.selection_for(
            request.agent_type.as_deref(),
            request.category.as_deref(),
            request.model.as_deref(),
        );
        let (requested_provider, requested_model) = apply_model_provider_hint(
            request.provider.as_deref().or_else(|| {
                requested_selection.and_then(|selection| selection.provider.as_deref())
            }),
            requested_selection
                .and_then(|selection| selection.model.as_deref())
                .or(request.model.as_deref()),
        );
        if let Some(provider) = requested_provider.as_deref() {
            if !provider_matches(&self.provider_name, self.client.provider_name(), provider) {
                bail!(
                    "sub-agent requested provider `{provider}`, but this runtime only has provider `{}` (client `{}`)",
                    self.provider_name,
                    self.client.provider_name()
                );
            }
        }
        let requested_profile = config_model_profile(requested_selection)
            .or(request.model_profile.as_deref())
            .filter(|profile| !is_inherit_sentinel(profile));
        let requested_for_resolution = model_for_resolution(
            requested_selection,
            request.model.as_deref(),
            requested_model.as_deref(),
        );
        validate_profile(requested_profile, &self.model_profiles, &self.provider_name)?;
        let resolved = resolve_sub_agent_model_with_config(
            requested_for_resolution,
            requested_profile,
            request.agent_type.as_deref(),
            request.category.as_deref(),
            &self.default_model,
            &self.provider_name,
            self.client.provider_name(),
            &self.model_config,
            &self.model_profiles,
        );
        Ok(ResolvedModelRuntime {
            provider_name: self.provider_name.clone(),
            client: self.client.clone(),
            model: resolved.model,
            reasoning_effort: resolved.reasoning_effort.or(request.reasoning_effort),
        })
    }
}

pub fn apply_model_provider_hint(
    explicit_provider: Option<&str>,
    model: Option<&str>,
) -> (Option<String>, Option<String>) {
    let explicit_provider = explicit_provider
        .map(str::trim)
        .filter(|value| !value.is_empty() && !is_inherit_sentinel(value))
        .map(str::to_string);
    let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) else {
        return (explicit_provider, None);
    };
    let Some((prefix, suffix)) = model.split_once('/') else {
        return (explicit_provider, Some(model.to_string()));
    };
    let prefix = prefix.trim();
    let suffix = suffix.trim();
    if prefix.is_empty() || suffix.is_empty() {
        return (explicit_provider, Some(model.to_string()));
    }
    match explicit_provider {
        Some(provider) => {
            if provider.eq_ignore_ascii_case(prefix) {
                (Some(provider), Some(suffix.to_string()))
            } else {
                (Some(provider), Some(model.to_string()))
            }
        }
        None => (Some(prefix.to_string()), Some(suffix.to_string())),
    }
}

pub fn provider_matches(runtime_provider: &str, client_provider: &str, requested: &str) -> bool {
    let runtime = normalize_provider_name(runtime_provider);
    let client = normalize_provider_name(client_provider);
    let requested = normalize_provider_name(requested);
    if requested == runtime || requested == client {
        return true;
    }
    matches!(requested.as_str(), "openai")
        && matches!(
            client.as_str(),
            "openai-responses" | "openai-compatible" | "openai"
        )
}

fn normalize_provider_name(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace('_', "-")
}

pub fn validate_profile(
    profile: Option<&str>,
    profiles: &ModelProfileMap,
    provider_name: &str,
) -> Result<()> {
    let Some(profile) = profile.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    if is_inherit_sentinel(profile) || profiles.is_empty() || profiles.get(profile).is_some() {
        return Ok(());
    }
    let normalized = ModelProfileMap::normalize_profile_name(profile);
    let builtin_profile = matches!(
        normalized.as_str(),
        "general"
            | "fast"
            | "small"
            | "explore"
            | "librarian"
            | "builder"
            | "reviewer"
            | "reasoning"
    );
    // The built-in profile names are a closed set and every one of them
    // resolves to something: a declared entry, a declared neighbour, or the
    // session's own model. Only a name outside that set can be a typo worth
    // refusing — an undeclared `general` used to be rejected here, which is
    // what stood between `general-purpose` sub-agents and a provider that
    // simply had not spelled the profile out.
    if builtin_profile {
        return Ok(());
    }
    let available = profiles
        .iter()
        .map(|(profile, _)| profile.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(anyhow!(
        "unknown modelProfile `{profile}` for provider `{provider_name}` (available: [{}]); \
         omit modelProfile to use the provider default model",
        available
    ))
}

pub fn parse_reasoning_effort_string(raw: Option<&str>) -> Option<ReasoningEffort> {
    raw.and_then(parse_reasoning_effort)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct TestResolver {
        default_provider: String,
        runtimes: HashMap<String, ProviderModelRuntime>,
        seen: Mutex<Vec<Option<String>>>,
    }

    impl TestResolver {
        fn new(default_provider: impl Into<String>) -> Self {
            Self {
                default_provider: default_provider.into(),
                runtimes: HashMap::new(),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn into_shared(self) -> Arc<Self> {
            Arc::new(self)
        }

        fn insert(
            &mut self,
            provider_name: impl Into<String>,
            default_model: impl Into<String>,
            model_profiles: ModelProfileMap,
        ) {
            let provider_name = provider_name.into();
            self.runtimes.insert(
                provider_name.clone(),
                ProviderModelRuntime {
                    provider_name,
                    client: Arc::new(rebon_api::MockModelClient::new()),
                    default_model: default_model.into(),
                    model_profiles,
                },
            );
        }

        fn seen(&self) -> Vec<Option<String>> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ProviderRuntimeResolver for TestResolver {
        async fn resolve_provider(&self, provider: Option<&str>) -> Result<ProviderModelRuntime> {
            self.seen.lock().unwrap().push(provider.map(str::to_string));
            let key = provider.unwrap_or(&self.default_provider);
            self.runtimes
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow!("missing provider {key}"))
        }
    }

    #[test]
    fn validate_profile_accepts_fast_as_small_profile_alias() {
        let profiles =
            ModelProfileMap::from_entries([("general", "gpt-main"), ("small", "gpt-mini")]);

        validate_profile(Some("fast"), &profiles, "openai").unwrap();
    }

    #[test]
    fn validate_profile_accepts_sentinels_and_hints_on_unknown() {
        let profiles = ModelProfileMap::from_entries([("general", "gpt-main")]);

        validate_profile(Some("inherit"), &profiles, "openai").unwrap();
        validate_profile(Some("default"), &profiles, "openai").unwrap();

        let err = validate_profile(Some("verification"), &profiles, "openai")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown modelProfile `verification`"), "{err}");
        assert!(err.contains("omit modelProfile"), "{err}");
    }

    #[test]
    fn apply_model_provider_hint_treats_provider_sentinels_as_unset() {
        assert_eq!(
            apply_model_provider_hint(Some("inherit"), None),
            (None, None)
        );
        assert_eq!(
            apply_model_provider_hint(Some("Default"), Some("gpt-main")),
            (None, Some("gpt-main".to_string()))
        );
    }

    #[tokio::test]
    async fn config_agent_provider_routes_before_model_resolution() {
        let mut resolver = TestResolver::new("openai");
        resolver.insert("openai", "gpt-main", ModelProfileMap::default());
        resolver.insert("deepseek", "deepseek-main", ModelProfileMap::default());
        let resolver = resolver.into_shared();

        let mut config = SubAgentModelConfig::default();
        config.insert_agent(
            "Explore",
            SubAgentModelSelection::model("deepseek/deepseek-coder").with_provider("deepseek"),
        );

        let router = ConfigurableModelRouter::new(resolver.clone()).with_model_config(config);
        let resolved = router
            .resolve(ModelRouteRequest {
                agent_type: Some("Explore".to_string()),
                ..ModelRouteRequest::default()
            })
            .await
            .unwrap();

        assert_eq!(resolver.seen(), vec![Some("deepseek".to_string())]);
        assert_eq!(resolved.provider_name, "deepseek");
        assert_eq!(resolved.model, "deepseek-coder");
    }

    #[tokio::test]
    async fn config_agent_provider_uses_target_provider_profile_fallbacks() {
        let mut resolver = TestResolver::new("openai");
        resolver.insert("openai", "gpt-main", ModelProfileMap::default());
        let deepseek_profiles = ModelProfileMap::from_entries([
            ("general", "deepseek-general"),
            ("reasoning", "deepseek-reasoning"),
        ]);
        resolver.insert("deepseek", "deepseek-main", deepseek_profiles);
        let resolver = resolver.into_shared();

        let mut config = SubAgentModelConfig::default();
        config.insert_agent(
            "verification",
            SubAgentModelSelection::model_profile("reviewer").with_provider("deepseek"),
        );

        let router = ConfigurableModelRouter::new(resolver.clone()).with_model_config(config);
        let resolved = router
            .resolve(ModelRouteRequest {
                agent_type: Some("verification".to_string()),
                ..ModelRouteRequest::default()
            })
            .await
            .unwrap();

        assert_eq!(resolver.seen(), vec![Some("deepseek".to_string())]);
        assert_eq!(resolved.provider_name, "deepseek");
        assert_eq!(resolved.model, "deepseek-reasoning");
    }

    #[test]
    fn resolve_sub_agent_model_treats_profile_sentinels_as_unset() {
        let profiles = ModelProfileMap::from_entries([("general", "gpt-main")]);
        for sentinel in ["default", "inherit", "Default", "INHERIT"] {
            let resolved = resolve_sub_agent_model_with_config(
                None,
                Some(sentinel),
                None,
                None,
                "session-model",
                "openai",
                "openai",
                &SubAgentModelConfig::default(),
                &profiles,
            );
            assert_eq!(
                resolved.model, "session-model",
                "profile sentinel `{sentinel}` must resolve to the default model, \
                 not a profile lookup"
            );
        }
    }

    #[test]
    fn resolve_sub_agent_model_uses_config_instead_of_builtin_aliases() {
        assert_eq!(
            resolve_sub_agent_model(None, "gpt-5.4", "openai-responses"),
            "gpt-5.4"
        );
        assert_eq!(
            resolve_sub_agent_model(Some("inherit"), "gpt-5.4", "openai-responses"),
            "gpt-5.4"
        );
        assert_eq!(
            resolve_sub_agent_model(Some("default"), "gpt-5.4", "openai-responses"),
            "gpt-5.4"
        );
        assert_eq!(
            resolve_sub_agent_model(Some("haiku"), "gpt-5.4", "openai-responses"),
            "haiku"
        );

        let mut config = SubAgentModelConfig::default();
        config.insert_agent(
            "explore",
            SubAgentModelSelection::model("openai/gpt-5.3-codex")
                .with_reasoning_effort(ReasoningEffort::Medium),
        );
        config.insert_alias("haiku", SubAgentModelSelection::model("gpt-5.4-mini"));
        config.insert_agent(
            "reviewer",
            SubAgentModelSelection::model_profile("reviewer")
                .with_provider("deepseek")
                .with_reasoning_effort(ReasoningEffort::High),
        );

        let profiles = ModelProfileMap::from_entries([
            ("general", "gpt-5.4"),
            ("explore", "gpt-5.4-mini"),
            ("reviewer", "gpt-5.4-review"),
        ]);
        let resolved = resolve_sub_agent_model_with_config(
            None,
            None,
            Some("Explore"),
            None,
            "gpt-5.4",
            "openai-responses",
            "openai-responses",
            &config,
            &profiles,
        );
        assert_eq!(resolved.model, "gpt-5.3-codex");
        assert_eq!(resolved.reasoning_effort, Some(ReasoningEffort::Medium));

        let profile_resolved = resolve_sub_agent_model_with_config(
            None,
            None,
            Some("reviewer"),
            None,
            "gpt-5.4",
            "openai-responses",
            "openai-responses",
            &config,
            &profiles,
        );
        assert_eq!(profile_resolved.model, "gpt-5.4-review");
        assert_eq!(
            config.agent("reviewer").unwrap().provider.as_deref(),
            Some("deepseek")
        );
        assert_eq!(
            profile_resolved.reasoning_effort,
            Some(ReasoningEffort::High)
        );

        let requested_profile_resolved = resolve_sub_agent_model_with_config(
            None,
            Some("explore"),
            None,
            None,
            "gpt-5.4",
            "openai-responses",
            "openai-responses",
            &SubAgentModelConfig::default(),
            &profiles,
        );
        assert_eq!(requested_profile_resolved.model, "gpt-5.4-mini");
        assert_eq!(requested_profile_resolved.reasoning_effort, None);

        let mut effort_profiles = ModelProfileMap::default();
        effort_profiles.insert_with_reasoning_effort("explore", "gpt-mini", Some("low"));
        let requested_profile_with_effort = resolve_sub_agent_model_with_config(
            None,
            Some("explore"),
            None,
            None,
            "gpt-5.4",
            "openai-responses",
            "openai-responses",
            &SubAgentModelConfig::default(),
            &effort_profiles,
        );
        assert_eq!(requested_profile_with_effort.model, "gpt-mini");
        assert_eq!(
            requested_profile_with_effort.reasoning_effort,
            Some(ReasoningEffort::Low)
        );

        assert_eq!(
            resolve_sub_agent_model_with_config(
                Some("haiku"),
                None,
                None,
                None,
                "gpt-5.4",
                "openai-responses",
                "openai-responses",
                &config,
                &profiles,
            )
            .model,
            "gpt-5.4-mini"
        );
        assert_eq!(
            resolve_sub_agent_model_with_config(
                Some("openai/gpt-5.4"),
                None,
                None,
                None,
                "gpt-5.3-codex",
                "openai-responses",
                "openai-responses",
                &SubAgentModelConfig::default(),
                &profiles,
            )
            .model,
            "gpt-5.4"
        );
        assert_eq!(
            resolve_sub_agent_model(Some("custom-model"), "gpt-5.4", "openai-responses"),
            "custom-model"
        );
    }
}
