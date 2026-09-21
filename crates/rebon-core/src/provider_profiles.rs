//! Declarative provider profiles.
//!
//! Provider preferences — reasoning-effort vocabulary and (later) cache
//! knobs — live as DATA here instead of being scattered through dispatch
//! code. The first instance is the deepseek profile: it carries the
//! dsh-wire reasoning fold that used to be hardcoded in the kernel llm
//! dispatch (behavior identical, pinned by the dispatch crate's existing
//! unit tests). What a model prefers in its *prompt* is not a profile
//! field: those preferences are sections `rebon-plugin-model-prompt` puts
//! on the prompt seat per subject.

use rebon_api::{CreateMessageRequest, ReasoningEffort, ThinkingConfig};

/// One provider's declarative preferences.
pub struct ProviderProfile {
    pub name: &'static str,
    /// Label when thinking is explicitly disabled.
    pub reasoning_disabled_label: &'static str,
    /// Reasoning-effort → wire-vocabulary table. An effort absent from the
    /// table emits no label.
    pub reasoning_effort_labels: &'static [(ReasoningEffort, &'static str)],
}

impl ProviderProfile {
    /// Fold one request's thinking/effort configuration into this
    /// provider's wire vocabulary.
    pub fn reasoning_label(&self, request: &CreateMessageRequest) -> Option<&'static str> {
        if matches!(request.thinking, Some(ThinkingConfig::Disabled)) {
            return Some(self.reasoning_disabled_label);
        }
        let effort = request.reasoning_effort?;
        self.reasoning_effort_labels
            .iter()
            .find(|(candidate, _)| *candidate == effort)
            .map(|(_, label)| *label)
    }
}

/// The deepseek family (dsh adapters): `off | high | max`, exactly the
/// fold calibrated against the real llm-deepseek adapter.
pub static DEEPSEEK_PROFILE: ProviderProfile = ProviderProfile {
    name: "deepseek",
    reasoning_disabled_label: "off",
    reasoning_effort_labels: &[
        (ReasoningEffort::Low, "off"),
        (ReasoningEffort::Medium, "high"),
        (ReasoningEffort::High, "high"),
        (ReasoningEffort::XHigh, "max"),
        (ReasoningEffort::Max, "max"),
    ],
};

/// Resolve the profile for a provider name (case-insensitive prefix — the
/// kernel route names are user-chosen: `deepseek-official`, `deepseek`…).
pub fn provider_profile(provider: &str) -> Option<&'static ProviderProfile> {
    let name = provider.trim();
    (name.len() >= DEEPSEEK_PROFILE.name.len()
        && name[..DEEPSEEK_PROFILE.name.len()].eq_ignore_ascii_case(DEEPSEEK_PROFILE.name))
    .then_some(&DEEPSEEK_PROFILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        thinking: Option<ThinkingConfig>,
        effort: Option<ReasoningEffort>,
    ) -> CreateMessageRequest {
        let mut request = CreateMessageRequest::simple("m", "probe");
        request.thinking = thinking;
        request.reasoning_effort = effort;
        request
    }

    /// The profile table reproduces the historical hardcoded fold exactly.
    #[test]
    fn deepseek_reasoning_fold_matches_the_calibrated_behavior() {
        let p = &DEEPSEEK_PROFILE;
        assert_eq!(
            p.reasoning_label(&request(Some(ThinkingConfig::Disabled), None)),
            Some("off")
        );
        assert_eq!(
            p.reasoning_label(&request(None, Some(ReasoningEffort::Low))),
            Some("off")
        );
        assert_eq!(
            p.reasoning_label(&request(None, Some(ReasoningEffort::Medium))),
            Some("high")
        );
        assert_eq!(
            p.reasoning_label(&request(None, Some(ReasoningEffort::High))),
            Some("high")
        );
        assert_eq!(
            p.reasoning_label(&request(None, Some(ReasoningEffort::XHigh))),
            Some("max")
        );
        assert_eq!(
            p.reasoning_label(&request(None, Some(ReasoningEffort::Max))),
            Some("max")
        );
        assert_eq!(p.reasoning_label(&request(None, None)), None);
    }

    #[test]
    fn lookup_is_prefix_based() {
        assert!(provider_profile("deepseek-official").is_some());
        assert!(provider_profile("DeepSeek").is_some());
        assert!(provider_profile("openai").is_none());
        assert_eq!(provider_profile("deepseek").unwrap().name, "deepseek");
    }
}
