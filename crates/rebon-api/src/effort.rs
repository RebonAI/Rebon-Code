use rebon_types::{effort_indicator::EffortProviderKind, ReasoningEffort};

/// Per-turn thinking overrides derived from the current effort level.
pub struct ThinkingOverrides {
    pub thinking_budget: Option<u32>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort_ordinal: Option<u8>,
}

/// Map the current effort level + provider kind to per-turn thinking
/// config. When effort is `None` (auto), defaults to high.
pub fn resolve_thinking_from_effort(
    effort: Option<ReasoningEffort>,
    provider_kind: EffortProviderKind,
) -> ThinkingOverrides {
    let ordinal = match effort {
        Some(ReasoningEffort::Low) => 0u8,
        Some(ReasoningEffort::Medium) => 1,
        Some(ReasoningEffort::High) => 2,
        Some(ReasoningEffort::XHigh) => 3,
        Some(ReasoningEffort::Max) => 4,
        None => 2, // default = high
    };
    match provider_kind {
        EffortProviderKind::Anthropic => {
            let (thinking, max_tokens) = crate::effort_to_anthropic_thinking(ordinal);
            let budget = match thinking {
                crate::ThinkingConfig::Enabled { budget_tokens } => budget_tokens,
                crate::ThinkingConfig::Disabled => 0,
            };
            ThinkingOverrides {
                thinking_budget: Some(budget),
                max_tokens: Some(max_tokens),
                reasoning_effort_ordinal: None,
            }
        }
        // `thinking_budget` is only an on/off signal here — OpenAI-style
        // providers take `reasoning_effort`, not a token budget — so the
        // ceiling is what has to scale with effort.
        EffortProviderKind::OpenAi => ThinkingOverrides {
            thinking_budget: Some(16_000),
            max_tokens: Some(crate::effort_to_openai_max_tokens(ordinal)),
            reasoning_effort_ordinal: Some(ordinal),
        },
    }
}
