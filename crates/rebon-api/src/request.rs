//! `CreateMessageRequest` — the single input struct for every
//! [`crate::ModelClient`] call.
//!
//! Covers the subset of `Anthropic.Messages.MessageCreateParams`
//! that rebon actually populates. Provider-specific rewrites (prompt caching
//! headers, betas) are applied inside the concrete client
//! implementations — the request type here stays provider-agnostic
//! except for [`ThinkingConfig`], which both providers need to
//! inspect.

use serde::Serialize;

use crate::types::{ContentBlock, Message, Role, TextBlock, Tool, ToolChoice};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheTraceContext {
    pub context_policy: Option<String>,
    /// Optional provider-specific prompt-cache routing key.
    pub prompt_cache_key: Option<String>,
    /// Optional provider-specific prompt-cache retention hint.
    pub prompt_cache_retention: Option<String>,
    pub api_path: Option<String>,
    pub previous_response_id_present: Option<bool>,
    pub tools_hash: Option<String>,
    pub schema_hash: Option<String>,
    pub shared_preamble_hash: Option<String>,
    pub profile_preamble_hash: Option<String>,
    pub capsule_hash: Option<String>,
    pub task_hash: Option<String>,
    pub tokens_before_capsule: Option<u32>,
    pub tokens_before_task: Option<u32>,
    pub cross_run_cache_eligible: Option<bool>,
    pub same_dispatch_cache_eligible: Option<bool>,
}

const RUNTIME_CONTEXT_OPEN: &str = "<system-reminder>\n<runtime_context>\n";
const RUNTIME_CONTEXT_CLOSE: &str = "\n</runtime_context>\n</system-reminder>";

/// Wrap request/runtime context in the canonical system-reminder shape.
pub fn wrap_runtime_context(context: &str) -> String {
    format!("{RUNTIME_CONTEXT_OPEN}{context}{RUNTIME_CONTEXT_CLOSE}")
}

/// Build a user-role message carrying canonical runtime context.
pub fn runtime_context_message(context: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::Text(TextBlock {
            text: wrap_runtime_context(context),
        })],
    }
}

/// Return the inner runtime context body when `message` is exactly
/// the canonical user-role runtime reminder.
pub fn runtime_context_body_from_message(message: &Message) -> Option<&str> {
    if message.role != Role::User || message.content.len() != 1 {
        return None;
    }
    let text = message.content.first()?.as_text()?;
    text.strip_prefix(RUNTIME_CONTEXT_OPEN)?
        .strip_suffix(RUNTIME_CONTEXT_CLOSE)
}

/// Whether a message is exactly the canonical runtime reminder.
pub fn is_runtime_context_message(message: &Message) -> bool {
    runtime_context_body_from_message(message).is_some()
}

/// Provider-agnostic thinking / reasoning configuration.
///
/// On the **Anthropic** side this maps to the `thinking` top-level
/// parameter (`{ type: "enabled", budget_tokens }` or
/// `{ type: "disabled" }`).
///
/// On the **OpenAI Responses** side it maps to the `reasoning`
/// parameter (`{ effort, summary }`).
#[derive(Debug, Clone, PartialEq)]
pub enum ThinkingConfig {
    /// Let the model allocate thinking tokens adaptively.
    /// Anthropic: `{ type: "enabled", budget_tokens: <model max> }`.
    /// OpenAI: `{ effort: "high" }`.
    Enabled {
        /// Maximum tokens the model may spend on thinking.
        /// For Anthropic this becomes `budget_tokens`; for OpenAI it
        /// is unused (effort-based).
        budget_tokens: u32,
    },
    /// Thinking explicitly disabled. The parameter is omitted from
    /// the request body.
    Disabled,
}

/// Reasoning effort for OpenAI Responses API.
///
/// Defined in `rebon-types` (so config/coordinator code can parse it
/// without this crate) and re-exported here so `rebon_api::ReasoningEffort`
/// stays the wire-side path.
pub use rebon_types::ReasoningEffort;

/// Reasoning mode for OpenAI Responses API (gpt-5.6+).
///
/// `reasoning.mode: "pro"` enables pro-mode execution on the selected
/// GPT-5.6 model — OpenAI's replacement for a separate Pro model
/// slug. Standard mode is the implicit default (field omitted).
/// Independent of [`ReasoningEffort`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ReasoningMode {
    #[serde(rename = "pro")]
    Pro,
}

/// Split a virtual `-pro` model alias into its real wire slug.
///
/// Rebon treats `gpt-5.6-sol-pro` (and the other gpt-5.6 family
/// variants) as a standalone model in pickers and config, but OpenAI
/// has no separate Pro model slug — pro mode is `reasoning.mode:
/// "pro"` on the base model. The Responses transport calls this to
/// send `model: "gpt-5.6-sol"` + pro mode instead.
///
/// Returns `None` for anything outside the gpt-5.6 family so real
/// model ids that happen to end in `-pro` (e.g. `deepseek-v4-pro`)
/// pass through untouched.
pub fn split_pro_model_alias(model: &str) -> Option<&str> {
    let base = model.strip_suffix("-pro")?;
    (base == "gpt-5.6" || base.starts_with("gpt-5.6-")).then_some(base)
}

/// Reasoning summary style for OpenAI Responses API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummary {
    Auto,
    Concise,
    Detailed,
}

/// Provider-agnostic web search tool configuration.
///
/// When present on a [`CreateMessageRequest`], the provider injects
/// its native web search tool spec into the `tools` array:
/// - **Anthropic**: `{ type: "web_search_20250305", ... }`
/// - **OpenAI Responses**: `{ type: "web_search", ... }`
///
/// The search executes server-side — no client-side tool dispatch is
/// needed.
#[derive(Debug, Clone, PartialEq)]
pub struct WebSearchToolConfig {
    /// Domain allowlist. Anthropic: `allowed_domains` field. OpenAI:
    /// `filters.allowed_domains`.
    pub allowed_domains: Option<Vec<String>>,
    /// Domain blocklist. Only supported by Anthropic.
    pub blocked_domains: Option<Vec<String>>,
    /// Maximum number of web searches per turn. Anthropic only
    /// (`max_uses`). Defaults to 5.
    pub max_uses: Option<u32>,
    /// Context size for search results. OpenAI only: `"low"`,
    /// `"medium"`, or `"high"`.
    pub search_context_size: Option<String>,
    /// User location for localized results. OpenAI only.
    pub user_location: Option<WebSearchUserLocation>,
}

/// Approximate user location for OpenAI web search.
#[derive(Debug, Clone, PartialEq)]
pub struct WebSearchUserLocation {
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub timezone: Option<String>,
}

/// Input for [`crate::ModelClient::create_message_stream`] and
/// [`crate::ModelClient::create_message`].
#[derive(Debug, Clone, PartialEq)]
pub struct CreateMessageRequest {
    /// Target model identifier (e.g. `"claude-sonnet-4-6"`).
    pub model: String,
    /// Conversation history the model should see. Most recent last.
    pub messages: Vec<Message>,
    /// Optional system instruction. Both providers carry this as a
    /// dedicated field, not as a message role.
    pub system: Option<String>,
    /// Request-scoped dynamic context appended after the current user
    /// content for this provider call only.
    pub transient_context: Option<String>,
    /// Tool definitions the model may call.
    pub tools: Vec<Tool>,
    /// Directive for how aggressively the model should use tools.
    pub tool_choice: Option<ToolChoice>,
    /// Maximum tokens the model is allowed to produce. Providers
    /// enforce their own per-model caps; this is the caller's hint.
    pub max_tokens: u32,
    /// Sampling temperature. `None` leaves it at the provider default.
    pub temperature: Option<f32>,
    /// Optional stop sequences that force the assistant to end a turn.
    pub stop_sequences: Vec<String>,
    /// Whether the caller wants a streaming response. Defaults to
    /// `true`; most call sites consume the stream either directly or
    /// via the accumulator.
    pub stream: bool,
    /// Opaque metadata forwarded to the provider. The Anthropic API
    /// uses this for `metadata.user_id`; the OpenAI path drops it.
    pub metadata: Option<serde_json::Value>,
    /// Extended thinking / reasoning configuration. `None` means the
    /// caller has no opinion and the provider should use its default
    /// (typically disabled for Anthropic, no reasoning for OpenAI).
    pub thinking: Option<ThinkingConfig>,
    /// Reasoning effort hint for OpenAI Responses API. Ignored by
    /// the Anthropic provider. `None` leaves the server default.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Reasoning mode for OpenAI Responses API (gpt-5.6+ pro mode).
    /// Ignored by the Anthropic provider. `None` means standard mode
    /// (field omitted from the request body).
    pub reasoning_mode: Option<ReasoningMode>,
    /// Reasoning summary style for OpenAI Responses API. Ignored by
    /// the Anthropic provider. `None` leaves the server default.
    pub reasoning_summary: Option<ReasoningSummary>,
    /// Web search tool configuration. When `Some`, the provider
    /// injects its native server-side web search tool into the
    /// request. Results flow back inline in the stream as
    /// `ServerToolUse` / `WebSearchResult` content blocks and do NOT
    /// trigger client-side tool dispatch.
    pub web_search: Option<WebSearchToolConfig>,
    /// Anthropic `context_management` beta parameter. When set, the
    /// Anthropic provider includes this in the request body and adds
    /// the `context-management-2025-06-27` beta header. Ignored by
    /// non-Anthropic providers.
    pub context_management: Option<ContextManagementConfig>,
    pub cache_trace_context: Option<CacheTraceContext>,
    /// OpenAI Responses remote compaction v2. When `true` the provider
    /// appends a `{"type":"compaction_trigger"}` control item after the
    /// last input item, which tells the backend to answer with a single
    /// `compaction` output item (an opaque replacement for the history)
    /// instead of running the turn.
    ///
    /// Request-scoped and never persisted — the trigger is a control,
    /// not a durable history item, which is why it lives here rather
    /// than in [`Self::messages`]. Ignored by every non-Responses
    /// provider.
    pub compaction_trigger: bool,
}

// ── Anthropic context_management types ──────────────────────────

/// Server-side context management configuration. Sent as the
/// `context_management` field in the Anthropic Messages API request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextManagementConfig {
    pub edits: Vec<ContextEditStrategy>,
}

/// A single server-side context-management strategy.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ContextEditStrategy {
    /// Summarize older conversation history when the prompt reaches a token threshold.
    #[serde(rename = "compact_20260112")]
    Compact {
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger: Option<TokenThreshold>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pause_after_compaction: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    /// Clear tool use/result blocks when input tokens exceed trigger.
    #[serde(rename = "clear_tool_uses_20250919")]
    ClearToolUses {
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger: Option<TokenThreshold>,
        #[serde(skip_serializing_if = "Option::is_none")]
        keep: Option<ToolUsesThreshold>,
        #[serde(skip_serializing_if = "Option::is_none")]
        clear_tool_inputs: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exclude_tools: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        clear_at_least: Option<TokenThreshold>,
    },
    /// Clear old thinking/reasoning blocks.
    #[serde(rename = "clear_thinking_20251015")]
    ClearThinking { keep: ThinkingKeep },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenThreshold {
    #[serde(rename = "type")]
    pub kind: String, // "input_tokens"
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolUsesThreshold {
    #[serde(rename = "type")]
    pub kind: String, // "tool_uses"
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ThinkingKeep {
    /// Keep N most recent thinking turns.
    Structured {
        #[serde(rename = "type")]
        kind: String,
        value: u32,
    },
    /// Keep all thinking turns.
    All(String),
}

impl CreateMessageRequest {
    /// Smallest useful constructor: model + one user message. Every
    /// other field stays at its default.
    pub fn simple(model: impl Into<String>, user_text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: vec![Message::user_text(user_text)],
            system: None,
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: true,
            metadata: None,
            thinking: None,
            reasoning_effort: None,
            reasoning_mode: None,
            reasoning_summary: None,
            web_search: None,
            context_management: None,
            cache_trace_context: None,
            compaction_trigger: false,
        }
    }

    /// Return a copy of the durable messages with the transient context
    /// appended to the current user turn when present.
    pub fn messages_with_transient_context(&self) -> Vec<Message> {
        let Some(context) = self
            .transient_context
            .as_deref()
            .filter(|context| !context.is_empty())
        else {
            return self.messages.clone();
        };

        let mut messages = self.messages.clone();
        let block = ContentBlock::Text(TextBlock {
            text: wrap_runtime_context(context),
        });
        if let Some(last) = messages.last_mut() {
            if last.role == Role::User
                && last
                    .content
                    .iter()
                    .all(|block| !matches!(block, ContentBlock::ToolResult(_)))
            {
                last.content.push(block);
                return messages;
            }
        }
        messages.push(Message {
            role: Role::User,
            content: vec![block],
        });
        messages
    }

    /// Attach a system prompt and return `self` (builder-style).
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Attach transient request context and return `self` (builder-style).
    pub fn with_transient_context(mut self, context: impl Into<String>) -> Self {
        self.transient_context = Some(context.into());
        self
    }

    /// Register a set of tools and return `self`.
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }

    /// Override `max_tokens` and return `self`.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Enable server-side web search and return `self`.
    pub fn with_web_search(mut self, config: WebSearchToolConfig) -> Self {
        self.web_search = Some(config);
        self
    }

    /// Enable server-side context management and return `self`.
    pub fn with_context_management(mut self, config: ContextManagementConfig) -> Self {
        self.context_management = Some(config);
        self
    }
}

impl ContextManagementConfig {
    /// Build Anthropic's server-side context-management profile for
    /// full-history replay providers.
    pub fn default_anthropic(trigger_tokens: u32, target_tokens: u32, has_thinking: bool) -> Self {
        let mut edits = Vec::new();

        if has_thinking {
            edits.push(ContextEditStrategy::ClearThinking {
                keep: ThinkingKeep::Structured {
                    kind: "thinking_turns".into(),
                    value: 1,
                },
            });
        }

        edits.push(ContextEditStrategy::ClearToolUses {
            trigger: Some(TokenThreshold {
                kind: "input_tokens".into(),
                value: trigger_tokens,
            }),
            keep: Some(ToolUsesThreshold {
                kind: "tool_uses".into(),
                value: 3,
            }),
            clear_tool_inputs: None,
            exclude_tools: None,
            clear_at_least: Some(TokenThreshold {
                kind: "input_tokens".into(),
                value: trigger_tokens.saturating_sub(target_tokens),
            }),
        });

        Self { edits }
    }

    pub fn anthropic_full_history_replay() -> Self {
        Self::anthropic_full_history_replay_with_thresholds(50_000, 100_000)
    }

    pub fn anthropic_full_history_replay_with_thresholds(
        tool_clear_trigger_tokens: u32,
        compact_trigger_tokens: u32,
    ) -> Self {
        Self::anthropic_context_management_with_thresholds(
            tool_clear_trigger_tokens,
            Some(compact_trigger_tokens),
        )
    }

    pub fn anthropic_context_management_with_thresholds(
        tool_clear_trigger_tokens: u32,
        compact_trigger_tokens: Option<u32>,
    ) -> Self {
        let mut edits = Self::default_anthropic(
            tool_clear_trigger_tokens.max(1),
            tool_clear_trigger_tokens.saturating_sub(16_000),
            true,
        )
        .edits;
        if let Some(compact_trigger_tokens) = compact_trigger_tokens {
            edits.push(ContextEditStrategy::Compact {
                trigger: Some(TokenThreshold {
                    kind: "input_tokens".into(),
                    value: compact_trigger_tokens.max(50_000),
                }),
                pause_after_compaction: None,
                instructions: Some(ANTHROPIC_COMPACTION_INSTRUCTIONS.to_string()),
            });
        }
        Self { edits }
    }
}

const ANTHROPIC_COMPACTION_INSTRUCTIONS: &str = "Summarize the transcript inside <summary></summary> tags. Include the task state, completed work, important decisions, files or artifacts changed, blockers, and concrete next steps needed to continue. Do not call any tools while writing this summary; respond with text only.";

// ── Effort → Thinking mapping ─────────────────────────────────────
//
// The output-token limits this mapping is built on:
//   default output-token limit = 32_000
//   upper output-token limit   = 64_000
//   Opus-4-6:   default 64_000, upper 128_000
//   Sonnet-4-6: default 32_000, upper 128_000
//   Thinking budget = upper limit - 1
//
// We map effort tiers to graduated fractions of those limits.

/// Anthropic thinking budget for each effort tier.
const ANTHROPIC_BUDGET_LOW: u32 = 4_096;
const ANTHROPIC_BUDGET_MEDIUM: u32 = 16_000;
const ANTHROPIC_BUDGET_HIGH: u32 = 63_999; // 64k upper limit - 1
const ANTHROPIC_BUDGET_MAX: u32 = 127_999; // 128k upper limit - 1

/// `max_tokens` sent alongside each Anthropic budget. Must be
/// strictly greater than `budget_tokens`.
const ANTHROPIC_MAX_TOKENS_LOW: u32 = 16_000;
const ANTHROPIC_MAX_TOKENS_MEDIUM: u32 = 32_000;
const ANTHROPIC_MAX_TOKENS_HIGH: u32 = 64_000; // the upper output-token limit
const ANTHROPIC_MAX_TOKENS_MAX: u32 = 128_000;

/// Map a 0–3 effort ordinal to the Anthropic thinking config.
///
/// `0` = low, `1` = medium, `2` = high (default), `3` = xhigh.
/// Returns `(ThinkingConfig, max_tokens)`.
pub fn effort_to_anthropic_thinking(effort_ordinal: u8) -> (ThinkingConfig, u32) {
    match effort_ordinal {
        0 => (
            ThinkingConfig::Enabled {
                budget_tokens: ANTHROPIC_BUDGET_LOW,
            },
            ANTHROPIC_MAX_TOKENS_LOW,
        ),
        1 => (
            ThinkingConfig::Enabled {
                budget_tokens: ANTHROPIC_BUDGET_MEDIUM,
            },
            ANTHROPIC_MAX_TOKENS_MEDIUM,
        ),
        // Anthropic has no tier above xhigh — ordinal 4 (max) clamps
        // to the same budget.
        3 | 4 => (
            ThinkingConfig::Enabled {
                budget_tokens: ANTHROPIC_BUDGET_MAX,
            },
            ANTHROPIC_MAX_TOKENS_MAX,
        ),
        // 2 and any unknown → high (default)
        _ => (
            ThinkingConfig::Enabled {
                budget_tokens: ANTHROPIC_BUDGET_HIGH,
            },
            ANTHROPIC_MAX_TOKENS_HIGH,
        ),
    }
}

/// Map a 0–3 effort ordinal to the OpenAI reasoning effort.
///
/// `0` = low, `1` = medium, `2` = high (default), `3` = xhigh.
pub fn effort_to_openai_reasoning(effort_ordinal: u8) -> ReasoningEffort {
    match effort_ordinal {
        0 => ReasoningEffort::Low,
        1 => ReasoningEffort::Medium,
        3 => ReasoningEffort::XHigh,
        4 => ReasoningEffort::Max,
        _ => ReasoningEffort::High,
    }
}

/// `max_tokens` for each effort tier on OpenAI-style providers.
///
/// These mirror the Anthropic tiers, but the reason they must scale
/// with effort is different: `max_output_tokens` covers hidden
/// reasoning too (see
/// [`ModelClient::output_budget_includes_reasoning`]), so a flat cap
/// lets a high-effort chain of thought spend the entire allowance and
/// end the turn with no visible text or tool call at all. There is no
/// separate `budget_tokens` knob to bound the reasoning independently
/// — raising the ceiling is the only lever.
///
/// [`ModelClient::output_budget_includes_reasoning`]: crate::ModelClient::output_budget_includes_reasoning
const OPENAI_MAX_TOKENS_LOW: u32 = 16_000;
const OPENAI_MAX_TOKENS_MEDIUM: u32 = 32_000;
const OPENAI_MAX_TOKENS_HIGH: u32 = 64_000;
const OPENAI_MAX_TOKENS_MAX: u32 = 128_000;

/// Map a 0–4 effort ordinal to the OpenAI-style `max_tokens`.
///
/// `0` = low, `1` = medium, `2` = high (default), `3` = xhigh,
/// `4` = max.
pub fn effort_to_openai_max_tokens(effort_ordinal: u8) -> u32 {
    match effort_ordinal {
        0 => OPENAI_MAX_TOKENS_LOW,
        1 => OPENAI_MAX_TOKENS_MEDIUM,
        3 | 4 => OPENAI_MAX_TOKENS_MAX,
        // 2 and any unknown → high (default)
        _ => OPENAI_MAX_TOKENS_HIGH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_pro_model_alias_matches_gpt_5_6_family() {
        assert_eq!(
            split_pro_model_alias("gpt-5.6-sol-pro"),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            split_pro_model_alias("gpt-5.6-terra-pro"),
            Some("gpt-5.6-terra")
        );
        assert_eq!(
            split_pro_model_alias("gpt-5.6-luna-pro"),
            Some("gpt-5.6-luna")
        );
        assert_eq!(split_pro_model_alias("gpt-5.6-pro"), Some("gpt-5.6"));
    }

    #[test]
    fn split_pro_model_alias_ignores_real_pro_models() {
        assert_eq!(split_pro_model_alias("deepseek-v4-pro"), None);
        assert_eq!(split_pro_model_alias("gpt-5.5-pro"), None);
        assert_eq!(split_pro_model_alias("gpt-5.6-sol"), None);
        assert_eq!(split_pro_model_alias("gpt-5.6"), None);
    }

    #[test]
    fn simple_constructor_defaults_to_streaming_with_one_user_message() {
        let req = CreateMessageRequest::simple("claude-sonnet-4-6", "hi");
        assert_eq!(req.model, "claude-sonnet-4-6");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content[0].as_text(), Some("hi"));
        assert_eq!(req.max_tokens, 4096);
        assert!(req.stream);
        assert!(req.tools.is_empty());
    }

    #[test]
    fn builder_chain_composes_system_tools_and_max_tokens() {
        let req = CreateMessageRequest::simple("model", "hi")
            .with_system("You are helpful.")
            .with_max_tokens(8192)
            .with_tools(vec![]);
        assert_eq!(req.system.as_deref(), Some("You are helpful."));
        assert_eq!(req.max_tokens, 8192);
    }

    #[test]
    fn simple_constructor_defaults_web_search_to_none() {
        let req = CreateMessageRequest::simple("model", "hi");
        assert!(req.web_search.is_none());
    }

    #[test]
    fn with_web_search_builder_sets_config() {
        let config = WebSearchToolConfig {
            allowed_domains: Some(vec!["example.com".into()]),
            blocked_domains: None,
            max_uses: Some(5),
            search_context_size: Some("high".into()),
            user_location: Some(WebSearchUserLocation {
                country: Some("US".into()),
                region: Some("California".into()),
                city: None,
                timezone: None,
            }),
        };
        let req = CreateMessageRequest::simple("model", "hi").with_web_search(config.clone());
        let ws = req.web_search.unwrap();
        assert_eq!(ws.allowed_domains, Some(vec!["example.com".to_string()]));
        assert_eq!(ws.max_uses, Some(5));
        assert_eq!(ws.search_context_size, Some("high".to_string()));
        assert!(ws.user_location.is_some());
        let loc = ws.user_location.unwrap();
        assert_eq!(loc.country, Some("US".to_string()));
        assert_eq!(loc.region, Some("California".to_string()));
        assert!(loc.city.is_none());
    }

    #[test]
    fn web_search_config_minimal() {
        let config = WebSearchToolConfig {
            allowed_domains: None,
            blocked_domains: None,
            max_uses: None,
            search_context_size: None,
            user_location: None,
        };
        let req = CreateMessageRequest::simple("model", "hi").with_web_search(config);
        assert!(req.web_search.is_some());
        let ws = req.web_search.unwrap();
        assert!(ws.allowed_domains.is_none());
        assert!(ws.blocked_domains.is_none());
        assert!(ws.max_uses.is_none());
    }

    #[test]
    fn anthropic_context_management_can_omit_compaction() {
        let config =
            ContextManagementConfig::anthropic_context_management_with_thresholds(50_000, None);
        let value = serde_json::to_value(&config).unwrap();
        let edits = value["edits"].as_array().unwrap();

        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0]["type"], "clear_thinking_20251015");
        assert_eq!(edits[1]["type"], "clear_tool_uses_20250919");
        assert!(edits.iter().all(|edit| edit["type"] != "compact_20260112"));
    }

    #[test]
    fn anthropic_full_history_replay_context_management_matches_official_shape() {
        let config =
            ContextManagementConfig::anthropic_full_history_replay_with_thresholds(50_000, 100_000);
        let value = serde_json::to_value(&config).unwrap();
        let edits = value["edits"].as_array().unwrap();

        assert_eq!(edits[0]["type"], "clear_thinking_20251015");
        assert_eq!(edits[0]["keep"]["type"], "thinking_turns");
        assert_eq!(edits[0]["keep"]["value"], 1);
        assert_eq!(edits[1]["type"], "clear_tool_uses_20250919");
        assert_eq!(edits[1]["trigger"]["type"], "input_tokens");
        assert_eq!(edits[1]["trigger"]["value"], 50_000);
        assert_eq!(edits[1]["keep"]["type"], "tool_uses");
        assert_eq!(edits[1]["keep"]["value"], 3);
        assert!(edits[1].get("clear_tool_inputs").is_none());
        assert_eq!(edits[2]["type"], "compact_20260112");
        assert_eq!(edits[2]["trigger"]["type"], "input_tokens");
        assert_eq!(edits[2]["trigger"]["value"], 100_000);
        assert!(edits[2]["instructions"]
            .as_str()
            .unwrap()
            .contains("Do not call any tools"));
    }
}
