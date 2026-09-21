//! Sub-agent model configuration — pure data read from config/agents files
//! and consumed by the spawner that launches sub-agents.
//!
//! Kept as plain data so a config parser can build a [`SubAgentModelConfig`]
//! without depending on the spawner or its runtime.

use std::collections::HashMap;

use crate::ReasoningEffort;

/// Model + reasoning choice for an agent entry in config.
///
/// This intentionally carries only data from config/agents files. The
/// spawner still owns provider-specific normalization right before it
/// sends the request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubAgentModelSelection {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub model_profile: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl SubAgentModelSelection {
    pub fn new(
        model: Option<String>,
        model_profile: Option<String>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        Self {
            provider: None,
            model,
            model_profile,
            reasoning_effort,
        }
    }

    pub fn provider(provider: impl Into<String>) -> Self {
        Self::default().with_provider(provider)
    }

    pub fn model(model: impl Into<String>) -> Self {
        Self {
            provider: None,
            model: Some(model.into()),
            model_profile: None,
            reasoning_effort: None,
        }
    }

    pub fn model_profile(profile: impl Into<String>) -> Self {
        Self {
            provider: None,
            model: None,
            model_profile: Some(profile.into()),
            reasoning_effort: None,
        }
    }

    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        let provider = provider.into();
        let provider = provider.trim();
        if !provider.is_empty() {
            self.provider = Some(provider.to_string());
        }
        self
    }

    pub fn with_reasoning_effort(mut self, reasoning_effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(reasoning_effort);
        self
    }
}

/// Runtime-configurable sub-agent model map.
///
/// Keys are normalized case-insensitively so `Explore`, `explore`, and
/// `EXPLORE` all refer to the same agent entry. `agents` is matched
/// by `metadata.agent_type`; `categories` is reserved for metadata
/// supplied by future agent registries; `aliases` lets users keep
/// short names like `haiku` without baking their meaning into code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubAgentModelConfig {
    agents: HashMap<String, SubAgentModelSelection>,
    categories: HashMap<String, SubAgentModelSelection>,
    aliases: HashMap<String, SubAgentModelSelection>,
}

impl SubAgentModelConfig {
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty() && self.categories.is_empty() && self.aliases.is_empty()
    }

    pub fn insert_agent(&mut self, key: impl AsRef<str>, selection: SubAgentModelSelection) {
        self.agents
            .insert(normalize_model_config_key(key.as_ref()), selection);
    }

    pub fn insert_category(&mut self, key: impl AsRef<str>, selection: SubAgentModelSelection) {
        self.categories
            .insert(normalize_model_config_key(key.as_ref()), selection);
    }

    pub fn insert_alias(&mut self, key: impl AsRef<str>, selection: SubAgentModelSelection) {
        self.aliases
            .insert(normalize_model_config_key(key.as_ref()), selection);
    }

    pub fn agent(&self, key: &str) -> Option<&SubAgentModelSelection> {
        self.agents.get(&normalize_model_config_key(key))
    }

    pub fn category(&self, key: &str) -> Option<&SubAgentModelSelection> {
        self.categories.get(&normalize_model_config_key(key))
    }

    pub fn alias(&self, key: &str) -> Option<&SubAgentModelSelection> {
        self.aliases.get(&normalize_model_config_key(key))
    }

    pub fn selection_for(
        &self,
        agent_type: Option<&str>,
        category: Option<&str>,
        requested: Option<&str>,
    ) -> Option<&SubAgentModelSelection> {
        agent_type
            .and_then(|name| self.agent(name))
            .or_else(|| category.and_then(|name| self.category(name)))
            .or_else(|| {
                requested
                    .map(str::trim)
                    .filter(|raw| !raw.is_empty())
                    .and_then(|raw| self.alias(raw))
            })
    }
}

fn normalize_model_config_key(key: &str) -> String {
    key.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_normalize_case_and_surrounding_whitespace() {
        let mut config = SubAgentModelConfig::default();
        assert!(config.is_empty());
        config.insert_agent("  Explore ", SubAgentModelSelection::model("m-explore"));
        config.insert_category("REVIEW", SubAgentModelSelection::model_profile("small"));
        config.insert_alias("Haiku", SubAgentModelSelection::provider("anthropic"));
        assert!(!config.is_empty());

        assert_eq!(
            config.agent("EXPLORE").and_then(|s| s.model.as_deref()),
            Some("m-explore")
        );
        assert_eq!(
            config
                .category(" review ")
                .and_then(|s| s.model_profile.as_deref()),
            Some("small")
        );
        assert_eq!(
            config.alias("haiku").and_then(|s| s.provider.as_deref()),
            Some("anthropic")
        );
        assert!(config.agent("unknown").is_none());
    }

    #[test]
    fn selection_for_prefers_agent_then_category_then_alias() {
        let mut config = SubAgentModelConfig::default();
        config.insert_agent("explore", SubAgentModelSelection::model("agent-model"));
        config.insert_category("research", SubAgentModelSelection::model("category-model"));
        config.insert_alias("fast", SubAgentModelSelection::model("alias-model"));

        let pick = |agent: Option<&str>, category: Option<&str>, requested: Option<&str>| {
            config
                .selection_for(agent, category, requested)
                .and_then(|s| s.model.clone())
        };

        assert_eq!(
            pick(Some("Explore"), Some("research"), Some("fast")).as_deref(),
            Some("agent-model")
        );
        assert_eq!(
            pick(Some("other"), Some("Research"), Some("fast")).as_deref(),
            Some("category-model")
        );
        assert_eq!(
            pick(None, None, Some("  FAST ")).as_deref(),
            Some("alias-model")
        );
        assert_eq!(pick(None, None, Some("   ")), None);
        assert_eq!(pick(None, None, None), None);
    }

    #[test]
    fn with_provider_ignores_blank_and_trims() {
        let selection = SubAgentModelSelection::model("m").with_provider("   ");
        assert_eq!(selection.provider, None);
        let selection = SubAgentModelSelection::model("m").with_provider(" openai ");
        assert_eq!(selection.provider.as_deref(), Some("openai"));
        let selection = selection.with_reasoning_effort(ReasoningEffort::High);
        assert_eq!(selection.reasoning_effort, Some(ReasoningEffort::High));
    }
}
