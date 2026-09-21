//! Which company is behind an endpoint, and what that changes on the wire.
//!
//! An OpenAI-compatible base URL says *how to speak* (chat completions) but
//! not *who is listening*. The listener decides the details that make or
//! break an agent loop: how thinking is switched on, which field carries the
//! output cap, which fields are rejected outright, whether historical
//! reasoning must be replayed for tool loops to keep working, whether the
//! prompt cache is keyed on a byte-exact prefix (so mid-history edits are
//! expensive), which usage field reports a cache hit, and whether there is
//! a `GET /models` to discover the catalogue from. Every one of those used
//! to be a `contains(".deepseek.com")` scattered across three client
//! constructors; this module is the one table they read instead.
//!
//! Detection is by host. A provider entry may also pin the vendor
//! explicitly (`"vendor": "deepseek"`) for a gateway whose host says
//! nothing about the upstream — [`ProviderVendor::resolve`] lets the pin win.
//!
//! Every per-vendor fact here was read from that vendor's own API
//! documentation; the page is cited next to the row. Where a vendor
//! documents nothing, the row says so and falls back to plain OpenAI
//! semantics rather than guessing.

use std::fmt;

/// The company (or open-source server) behind an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ProviderVendor {
    /// api.openai.com — Chat Completions and the Responses API.
    OpenAi,
    /// api.anthropic.com, or Claude served by a cloud: Vertex AI
    /// (`*aiplatform*.googleapis.com`), Amazon Bedrock
    /// (`bedrock-mantle.*.api.aws`), Microsoft Foundry
    /// (`*.services.ai.azure.com`).
    Anthropic,
    /// Gemini through its OpenAI-compatible surface
    /// (`generativelanguage.googleapis.com/v1beta/openai`).
    Gemini,
    /// A local Ollama server (`localhost:11434`).
    Ollama,
    /// OpenCode Zen (`opencode.ai/zen`).
    OpenCode,
    /// DeepSeek (`api.deepseek.com`).
    DeepSeek,
    /// Volcengine Ark / 火山方舟 (`ark.cn-beijing.volces.com`).
    Volcengine,
    /// SiliconFlow / 硅基流动 (`api.siliconflow.cn`).
    SiliconFlow,
    /// Zhipu / 智谱 bigmodel.cn (`open.bigmodel.cn`, `api.z.ai`).
    Zhipu,
    /// Alibaba Cloud DashScope / 百炼 Qwen (`dashscope.aliyuncs.com`,
    /// `*.maas.aliyuncs.com`).
    Qwen,
    /// Moonshot Kimi (`api.moonshot.cn`, `api.moonshot.ai`).
    Kimi,
    /// MiniMax (`api.minimaxi.com`, `api.minimax.io`).
    MiniMax,
    /// Nothing recognised: plain OpenAI-compatible semantics.
    #[default]
    Unknown,
}

/// How a vendor expects thinking / reasoning to be switched on a
/// chat-completions request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkingDialect {
    /// `"thinking": {"type": "enabled" | "disabled"}`. DeepSeek's
    /// extension, adopted by Zhipu, Volcengine Ark and Kimi K2.x.
    ThinkingType,
    /// `"thinking": {"type": "adaptive" | "disabled"}` — MiniMax's spelling
    /// of the same switch.
    AdaptiveThinkingType,
    /// `"enable_thinking": true | false`. DashScope Qwen and SiliconFlow.
    EnableThinking,
    /// The vendor has no switch on this surface; thinking is decided by the
    /// model (or by `reasoning_effort` alone). A `thinking` object a config
    /// wrote for some other vendor is dropped, because this one rejects it.
    None,
    /// Nobody knows who is listening (an unrecognised relay, or OpenCode Zen
    /// fronting several vendors): whatever the config wrote is forwarded
    /// untouched, and no switch is added per turn.
    Verbatim,
}

/// Which spellings of `reasoning_effort` a vendor accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffortVocabulary {
    /// Forward Rebon's value unchanged (`low|medium|high|xhigh|max`); the
    /// server maps what it does not know.
    Passthrough,
    /// `low | high | max` only — `medium` folds to `high`, `xhigh` to `max`.
    LowHighMax,
    /// `low | medium | high` only — `xhigh` and `max` fold to `high`.
    LowMediumHigh,
    /// The field is rejected or ignored; never emit it.
    Unsupported,
}

impl EffortVocabulary {
    /// Fold one of Rebon's effort levels into this vocabulary, or `None`
    /// when the field must not be sent.
    pub fn fold(self, effort: &str) -> Option<&'static str> {
        let level = match effort.trim().to_ascii_lowercase().as_str() {
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "xhigh" => "xhigh",
            "max" => "max",
            _ => return None,
        };
        match self {
            Self::Passthrough => Some(level),
            Self::LowHighMax => Some(match level {
                "low" => "low",
                "medium" | "high" => "high",
                _ => "max",
            }),
            Self::LowMediumHigh => Some(match level {
                "low" => "low",
                "medium" => "medium",
                _ => "high",
            }),
            Self::Unsupported => None,
        }
    }
}

/// Which field carries the output-token cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MaxTokensField {
    /// The classic `max_tokens`.
    MaxTokens,
    /// `max_completion_tokens` — the vendor deprecated `max_tokens`
    /// (Moonshot Kimi, MiniMax), or rejects it above a low ceiling in
    /// thinking mode (DashScope: 32,768).
    MaxCompletionTokens,
}

/// Everything about a vendor's chat-completions dialect that the request
/// builder has to know, for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChatWireRules {
    pub thinking: ThinkingDialect,
    pub effort: EffortVocabulary,
    pub max_tokens_field: MaxTokensField,
    /// Body fields the vendor documents as rejected; stripped before
    /// sending. Rebon applies what it can client-side (stop sequences are
    /// re-applied to the accumulated text either way).
    pub unsupported_fields: &'static [&'static str],
    /// MiniMax's `reasoning_split: true` — without it the reasoning is
    /// embedded in `content` between `<think>` tags.
    pub reasoning_split: bool,
    /// Whether enabling thinking per turn must also drop `temperature`
    /// (DeepSeek's thinking endpoint rejects it).
    pub drop_temperature_when_thinking: bool,
    /// Whether historical assistant `reasoning_content` must be replayed so
    /// tool loops keep working (DeepSeek returns 400 without it; Zhipu,
    /// Kimi, MiniMax and Volcengine document the same requirement).
    pub replay_reasoning_content: bool,
    /// Whether the vendor hands back an encrypted copy of the reasoning
    /// (`encrypted_content`) that must travel with the replayed turn.
    /// Volcengine Ark: "必须回传思考内容加密原文".
    pub replay_encrypted_reasoning: bool,
}

impl ChatWireRules {
    /// Plain OpenAI Chat Completions.
    pub const OPENAI: Self = Self {
        thinking: ThinkingDialect::None,
        effort: EffortVocabulary::Passthrough,
        max_tokens_field: MaxTokensField::MaxTokens,
        unsupported_fields: &[],
        reasoning_split: false,
        drop_temperature_when_thinking: false,
        replay_reasoning_content: false,
        replay_encrypted_reasoning: false,
    };
}

/// How a vendor's prompt cache behaves, as far as the client needs to care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptCacheKind {
    /// Prefix cache the server maintains on its own. A stable, growing
    /// request prefix hits; any edit in the middle misses from that point.
    AutomaticPrefix,
    /// The client marks cache breakpoints (`cache_control`) and the server
    /// caches up to each one. Still prefix-based.
    ExplicitBreakpoints,
    /// Not documented, or not offered.
    None,
}

/// A vendor's prompt-cache contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromptCacheProfile {
    pub kind: PromptCacheKind,
    /// The shortest prefix the vendor will cache, in tokens, when the
    /// documentation states one.
    pub min_prefix_tokens: Option<u32>,
    /// The usage field that reports cached input tokens, in the vendor's
    /// own spelling, for diagnostics.
    pub hit_usage_field: &'static str,
}

impl PromptCacheProfile {
    const NONE: Self = Self {
        kind: PromptCacheKind::None,
        min_prefix_tokens: None,
        hit_usage_field: "",
    };

    /// Whether the cache is keyed on the request prefix — the property that
    /// makes in-place edits to old history a net loss.
    pub fn is_prefix_based(self) -> bool {
        !matches!(self.kind, PromptCacheKind::None)
    }
}

/// Which HTTP surface lists a vendor's models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelListing {
    /// `GET {base}/models` with a bearer key; OpenAI's list shape.
    /// `query` is appended verbatim when non-empty (SiliconFlow filters by
    /// `type=text&sub_type=chat`). `documented` is false where the vendor
    /// does not describe the endpoint but it is known to answer (Zhipu):
    /// worth a try, not worth an error when it does not.
    OpenAiCompatible {
        query: &'static str,
        documented: bool,
    },
    /// `GET {base}/v1/models` with `x-api-key` + `anthropic-version`,
    /// paginated by `after_id`; entries carry `max_input_tokens` and
    /// `max_tokens`.
    Anthropic,
    /// Ollama's native `GET /api/tags`, then `POST /api/show` per model for
    /// the architecture's `context_length`.
    Ollama,
    /// DashScope's native `GET /api/v1/models`, paginated, whose entries
    /// carry `model_info.{context_window,max_input_tokens,max_output_tokens}`.
    DashScope,
    /// The vendor documents no list endpoint; the static catalogue is all
    /// there is.
    Unsupported,
}

/// A model the vendor documents, with the limits its page states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KnownModel {
    pub id: &'static str,
    /// Context window in tokens.
    pub context_window: u32,
    /// Largest `max_tokens` the model accepts, when documented.
    pub max_output_tokens: Option<u32>,
}

const fn known(
    id: &'static str,
    context_window: u32,
    max_output_tokens: Option<u32>,
) -> KnownModel {
    KnownModel {
        id,
        context_window,
        max_output_tokens,
    }
}

// Catalogues, from each vendor's model pages. Flagship first; the first
// row is what a fresh entry starts on.

/// developers.openai.com/api/docs/models (GPT-5.6 family and GPT-6 Astra;
/// older ids are on the deprecations page and still answer). Sol stays the
/// first row while Astra is a staged rollout: the first row is what a fresh
/// entry starts on, and an account without Astra access must not start on
/// a model it cannot call.
const OPENAI_MODELS: &[KnownModel] = &[
    known("gpt-5.6-sol", 1_050_000, Some(128_000)),
    // developers.openai.com/api/docs/models/gpt-6-astra (2026-09-05):
    // 1,050,000 context window, 128,000 max output tokens.
    known("gpt-6-astra", 1_050_000, Some(128_000)),
    known("gpt-5.6-terra", 1_050_000, Some(128_000)),
    known("gpt-5.6-luna", 1_050_000, Some(128_000)),
    known("gpt-5.5", 1_050_000, Some(128_000)),
];

/// platform.claude.com/docs/en/about-claude/models/overview.
const ANTHROPIC_MODELS: &[KnownModel] = &[
    known("claude-opus-5", 1_000_000, Some(128_000)),
    known("claude-sonnet-5", 1_000_000, Some(128_000)),
    known("claude-fable-5", 1_000_000, Some(128_000)),
    known("claude-haiku-4-5", 200_000, Some(64_000)),
    known("claude-opus-4-8", 1_000_000, Some(128_000)),
    known("claude-opus-4-7", 1_000_000, Some(128_000)),
    known("claude-opus-4-6", 1_000_000, Some(128_000)),
    known("claude-sonnet-4-6", 1_000_000, Some(128_000)),
    known("claude-opus-4-5", 200_000, Some(64_000)),
    known("claude-sonnet-4-5", 200_000, Some(64_000)),
];

/// ai.google.dev/gemini-api/docs/models.
const GEMINI_MODELS: &[KnownModel] = &[
    known("gemini-3.7-flash", 1_048_576, Some(65_536)),
    known("gemini-3.6-flash", 1_048_576, Some(65_536)),
    known("gemini-3.5-flash", 1_048_576, Some(65_536)),
    known("gemini-3.5-flash-lite", 1_048_576, Some(65_536)),
    known("gemini-3.1-pro-preview", 1_048_576, Some(65_536)),
    known("gemini-3.1-flash-lite", 1_048_576, Some(65_536)),
    known("gemini-2.5-pro", 1_048_576, Some(65_536)),
    known("gemini-2.5-flash", 1_048_576, Some(65_536)),
    known("gemini-2.5-flash-lite", 1_048_576, Some(65_536)),
];

/// api-docs.deepseek.com/quick_start/pricing.
const DEEPSEEK_MODELS: &[KnownModel] = &[
    known("deepseek-v4-pro", 1_000_000, Some(384_000)),
    known("deepseek-flash", 1_000_000, Some(384_000)),
    known("deepseek-v4-flash-vision-exp", 1_000_000, Some(384_000)),
];

/// docs.volcengine.com/docs/82379/1330310 (模型列表). No list endpoint on
/// the data plane, so this table is the catalogue.
const VOLCENGINE_MODELS: &[KnownModel] = &[
    known("doubao-seed-evolving", 1_024_000, Some(256_000)),
    known("doubao-seed-2-1-pro-260628", 256_000, Some(256_000)),
    known("doubao-seed-2-1-turbo-260628", 256_000, Some(256_000)),
    known("doubao-seed-2-0-pro-260215", 256_000, Some(128_000)),
    known("doubao-seed-2-0-lite-260428", 256_000, Some(128_000)),
    known("doubao-seed-2-0-mini-260428", 256_000, Some(128_000)),
    known(
        "doubao-seed-2-0-code-preview-260215",
        256_000,
        Some(128_000),
    ),
    known("glm-5-2-260617", 1_024_000, Some(128_000)),
    known("deepseek-v4-pro-ga-260813", 1_024_000, Some(384_000)),
    known("deepseek-v4-flash-ga-260731", 1_024_000, Some(384_000)),
];

/// siliconflow.cn/models (the list endpoint carries no limits).
const SILICONFLOW_MODELS: &[KnownModel] = &[
    known("deepseek-ai/DeepSeek-V4-Pro", 1_024_000, None),
    known("deepseek-ai/DeepSeek-V4-Flash", 1_024_000, None),
    known("moonshotai/Kimi-K2.7-Code", 256_000, None),
    known("Pro/moonshotai/Kimi-K2.6", 256_000, None),
    known("zai-org/GLM-5.2", 1_024_000, None),
    known("Pro/zai-org/GLM-5.1", 198_000, None),
    known("Qwen/Qwen3.5-397B-A17B", 256_000, None),
    known("Qwen/Qwen3.6-35B-A3B", 256_000, None),
    known("Qwen/Qwen3.6-27B", 256_000, None),
    known("deepseek-ai/DeepSeek-V3.2", 160_000, None),
    known("MiniMaxAI/MiniMax-M2.5", 192_000, None),
];

/// docs.bigmodel.cn/cn/guide/start/model-overview.
const ZHIPU_MODELS: &[KnownModel] = &[
    known("glm-5.3", 1_000_000, Some(128_000)),
    known("glm-5.3-flash", 1_000_000, None),
    known("glm-5.2", 1_000_000, Some(128_000)),
    known("glm-5.1", 200_000, Some(128_000)),
    known("glm-5", 200_000, Some(128_000)),
    known("glm-5-turbo", 200_000, Some(128_000)),
    known("glm-4.7", 200_000, Some(128_000)),
    known("glm-4.6", 200_000, Some(128_000)),
    known("glm-4.5-air", 128_000, Some(96_000)),
];

/// help.aliyun.com/zh/model-studio/models.
const QWEN_MODELS: &[KnownModel] = &[
    known("qwen3.8-max", 1_000_000, Some(131_072)),
    known("qwen3.7-plus", 1_000_000, Some(131_072)),
    known("qwen3.7-flash", 1_000_000, Some(131_072)),
    known("qwen3-coder-plus", 1_000_000, Some(65_536)),
    known("qwen3-coder-flash", 1_000_000, Some(65_536)),
    known("qwen-plus", 1_000_000, Some(32_768)),
    known("deepseek-v4-pro", 1_000_000, Some(393_216)),
    known("kimi-k3", 1_048_576, Some(1_048_576)),
    known("glm-5.3", 200_000, None),
];

/// platform.kimi.com/docs/models.
const KIMI_MODELS: &[KnownModel] = &[
    known("kimi-k3", 1_048_576, Some(1_048_576)),
    known("kimi-k2.7-code", 262_144, None),
    known("kimi-k2.7-code-highspeed", 262_144, None),
    known("kimi-k2.6", 262_144, None),
    known("kimi-k2.5", 262_144, None),
];

/// platform.minimax.io/docs/guides/models-intro.
const MINIMAX_MODELS: &[KnownModel] = &[
    known("MiniMax-M3", 1_000_000, Some(524_288)),
    known("MiniMax-M2.7", 204_800, Some(204_800)),
    known("MiniMax-M2.7-highspeed", 204_800, Some(204_800)),
    known("MiniMax-M2.5", 204_800, Some(204_800)),
    known("MiniMax-M2.1", 204_800, Some(204_800)),
];

impl ProviderVendor {
    /// Every vendor with a real identity, in display order.
    pub const ALL: &'static [ProviderVendor] = &[
        Self::OpenAi,
        Self::Anthropic,
        Self::Gemini,
        Self::Ollama,
        Self::OpenCode,
        Self::DeepSeek,
        Self::Volcengine,
        Self::SiliconFlow,
        Self::Zhipu,
        Self::Qwen,
        Self::Kimi,
        Self::MiniMax,
    ];

    /// Stable identifier, also the value of the `vendor` config key.
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::Ollama => "ollama",
            Self::OpenCode => "opencode",
            Self::DeepSeek => "deepseek",
            Self::Volcengine => "volcengine",
            Self::SiliconFlow => "siliconflow",
            Self::Zhipu => "zhipu",
            Self::Qwen => "qwen",
            Self::Kimi => "kimi",
            Self::MiniMax => "minimax",
            Self::Unknown => "unknown",
        }
    }

    /// Human-facing name.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Google Gemini",
            Self::Ollama => "Ollama",
            Self::OpenCode => "OpenCode Zen",
            Self::Volcengine => "火山方舟",
            Self::SiliconFlow => "硅基流动",
            Self::Zhipu => "智谱 GLM",
            Self::Qwen => "通义千问",
            Self::DeepSeek => "DeepSeek",
            Self::Kimi => "Kimi",
            Self::MiniMax => "MiniMax",
            Self::Unknown => "OpenAI 兼容",
        }
    }

    /// Parse a `vendor` config value. Case-insensitive; a few aliases people
    /// actually type are accepted (`glm`, `zhipuai`, `dashscope`, `ark`,
    /// `moonshot`, `google`).
    pub fn parse(id: &str) -> Option<Self> {
        match id.trim().to_ascii_lowercase().as_str() {
            "openai" => Some(Self::OpenAi),
            "anthropic" | "claude" => Some(Self::Anthropic),
            "gemini" | "google" => Some(Self::Gemini),
            "ollama" => Some(Self::Ollama),
            "opencode" | "opencode-zen" | "zen" => Some(Self::OpenCode),
            "deepseek" => Some(Self::DeepSeek),
            "volcengine" | "ark" | "doubao" | "volces" => Some(Self::Volcengine),
            "siliconflow" => Some(Self::SiliconFlow),
            "zhipu" | "zhipuai" | "glm" | "bigmodel" => Some(Self::Zhipu),
            "qwen" | "dashscope" | "aliyun" | "bailian" => Some(Self::Qwen),
            "kimi" | "moonshot" => Some(Self::Kimi),
            "minimax" => Some(Self::MiniMax),
            _ => None,
        }
    }

    /// Recognise the vendor from an endpoint's host.
    ///
    /// Only the host is consulted: a path can be anything a gateway chooses,
    /// but the host is what the request is actually sent to. An empty or
    /// unparseable URL is [`Self::Unknown`].
    pub fn detect(base_url: &str) -> Self {
        let host = match host_of(base_url) {
            Some(host) => host,
            None => return Self::Unknown,
        };
        let h = host.as_str();
        if h == "api.openai.com" || h == "chatgpt.com" {
            Self::OpenAi
        } else if h == "api.anthropic.com"
            || h.ends_with("aiplatform.googleapis.com")
            || h.ends_with(".rep.googleapis.com")
            || (h.starts_with("bedrock-mantle.") && h.ends_with(".api.aws"))
            || (h.starts_with("aws-external-anthropic.") && h.ends_with(".api.aws"))
            || h.ends_with(".services.ai.azure.com")
        {
            Self::Anthropic
        } else if h == "generativelanguage.googleapis.com" {
            Self::Gemini
        } else if h == "opencode.ai" {
            Self::OpenCode
        } else if h == "api.deepseek.com" || h.ends_with(".deepseek.com") {
            Self::DeepSeek
        } else if h.ends_with(".volces.com")
            || h.ends_with(".volcengineapi.com")
            || h.ends_with(".byteplusapi.com")
        {
            Self::Volcengine
        } else if h == "api.siliconflow.cn"
            || h == "api.siliconflow.com"
            || h.ends_with(".siliconflow.com")
        {
            Self::SiliconFlow
        } else if h == "open.bigmodel.cn" || h == "api.z.ai" {
            Self::Zhipu
        } else if h.ends_with("dashscope.aliyuncs.com")
            || h.ends_with("dashscope-intl.aliyuncs.com")
            || h.ends_with(".maas.aliyuncs.com")
        {
            Self::Qwen
        } else if h == "api.moonshot.cn" || h == "api.moonshot.ai" {
            Self::Kimi
        } else if h == "api.minimaxi.com" || h == "api.minimax.io" || h == "api.minimax.chat" {
            Self::MiniMax
        } else if is_ollama_host(h, base_url) {
            Self::Ollama
        } else {
            Self::Unknown
        }
    }

    /// The vendor an entry runs as: an explicit pin wins, otherwise the host.
    pub fn resolve(pinned: Option<&str>, base_url: &str) -> Self {
        pinned
            .and_then(Self::parse)
            .unwrap_or_else(|| Self::detect(base_url))
    }

    /// The chat-completions dialect this vendor speaks for `model`.
    ///
    /// Model-sensitive where the vendor's own docs are: Kimi K3 takes
    /// `reasoning_effort` and rejects the `thinking` object that K2.x
    /// requires.
    pub fn chat_wire_rules(self, model: &str) -> ChatWireRules {
        let model = model.trim().to_ascii_lowercase();
        match self {
            // api-docs.deepseek.com/guides/thinking_mode: `thinking.type`
            // plus `reasoning_effort` low|high|max (medium/xhigh fold to
            // high server-side); `reasoning_content` must be passed back
            // whenever tools are present; thinking ignores temperature.
            Self::DeepSeek => ChatWireRules {
                thinking: ThinkingDialect::ThinkingType,
                effort: EffortVocabulary::Passthrough,
                drop_temperature_when_thinking: true,
                replay_reasoning_content: true,
                ..ChatWireRules::OPENAI
            },
            // docs.bigmodel.cn thinking-mode guide: `thinking.type` +
            // `reasoning_effort`; GLM-5.3 accepts only low|high|max, so the
            // narrow vocabulary is the safe one for the whole family. The
            // guide asks for historical `reasoning_content` back on tool
            // turns.
            Self::Zhipu => ChatWireRules {
                thinking: ThinkingDialect::ThinkingType,
                effort: EffortVocabulary::LowHighMax,
                replay_reasoning_content: true,
                ..ChatWireRules::OPENAI
            },
            // platform.kimi.com: K3 reasons always, tuned by
            // `reasoning_effort` low|high|max, and errors on a `thinking`
            // object; K2.x switch with `thinking.type`. `max_tokens` is
            // deprecated for `max_completion_tokens`. Both replay
            // `reasoning_content` in tool loops.
            Self::Kimi => {
                let k3 = model.starts_with("kimi-k3");
                ChatWireRules {
                    thinking: if k3 {
                        ThinkingDialect::None
                    } else {
                        ThinkingDialect::ThinkingType
                    },
                    effort: if k3 {
                        EffortVocabulary::LowHighMax
                    } else {
                        EffortVocabulary::Unsupported
                    },
                    max_tokens_field: MaxTokensField::MaxCompletionTokens,
                    replay_reasoning_content: true,
                    ..ChatWireRules::OPENAI
                }
            }
            // platform.minimax.io chat reference: `thinking.type`
            // adaptive|disabled, `reasoning_split` to get
            // `reasoning_content` out of the text, `max_completion_tokens`;
            // `tool_choice`, `parallel_tool_calls`, `response_format`,
            // `reasoning_effort`, `stop`, `n` are unsupported.
            Self::MiniMax => ChatWireRules {
                thinking: ThinkingDialect::AdaptiveThinkingType,
                effort: EffortVocabulary::Unsupported,
                max_tokens_field: MaxTokensField::MaxCompletionTokens,
                unsupported_fields: &[
                    "tool_choice",
                    "parallel_tool_calls",
                    "response_format",
                    "stop",
                    "n",
                ],
                reasoning_split: true,
                replay_reasoning_content: true,
                ..ChatWireRules::OPENAI
            },
            // docs.volcengine.com/docs/82379/1449737 (深度思考):
            // `thinking.type` enabled|disabled|auto, `reasoning_effort`
            // none..max mapped per model; tool loops must return
            // `reasoning_content` and the `encrypted_content` copy.
            // `max_tokens` (answer only) stays: `max_completion_tokens`
            // caps at 65,536 and includes the chain of thought.
            Self::Volcengine => ChatWireRules {
                thinking: ThinkingDialect::ThinkingType,
                effort: EffortVocabulary::Passthrough,
                replay_reasoning_content: true,
                replay_encrypted_reasoning: true,
                ..ChatWireRules::OPENAI
            },
            // help.aliyun.com/zh/model-studio/qwen-api-via-openai-chat-completions:
            // `enable_thinking`, `thinking_budget`; `max_tokens` is being
            // retired and is rejected above 32,768 in thinking mode, so the
            // cap goes in `max_completion_tokens`. History
            // `reasoning_content` is ignored unless `preserve_thinking`,
            // except for the Kimi models, which need it on tool turns.
            Self::Qwen => ChatWireRules {
                thinking: ThinkingDialect::EnableThinking,
                effort: EffortVocabulary::Unsupported,
                max_tokens_field: MaxTokensField::MaxCompletionTokens,
                replay_reasoning_content: true,
                ..ChatWireRules::OPENAI
            },
            // docs.siliconflow.cn chat-completions reference:
            // `enable_thinking`, `thinking_budget`; `reasoning_effort` only
            // high|max on a few models, so it is not sent.
            Self::SiliconFlow => ChatWireRules {
                thinking: ThinkingDialect::EnableThinking,
                effort: EffortVocabulary::Unsupported,
                replay_reasoning_content: true,
                ..ChatWireRules::OPENAI
            },
            // ai.google.dev/gemini-api/docs/openai: `reasoning_effort`
            // none|minimal|low|medium|high maps onto a thinking budget.
            Self::Gemini => ChatWireRules {
                effort: EffortVocabulary::LowMediumHigh,
                ..ChatWireRules::OPENAI
            },
            // docs.ollama.com/api/openai-compatibility: `reasoning_effort`
            // is honoured (xhigh folds to max server-side); `tool_choice`,
            // `n`, `user`, `logit_bias` are unsupported.
            Self::Ollama => ChatWireRules {
                unsupported_fields: &["tool_choice", "n", "user", "logit_bias"],
                ..ChatWireRules::OPENAI
            },
            // Zen's chat-completions path fronts DeepSeek, GLM, Kimi and
            // MiniMax models, which do take a `thinking` object; a relay
            // nobody recognised gets the same benefit of the doubt.
            Self::OpenCode | Self::Unknown => ChatWireRules {
                thinking: ThinkingDialect::Verbatim,
                ..ChatWireRules::OPENAI
            },
            Self::OpenAi | Self::Anthropic => ChatWireRules::OPENAI,
        }
    }

    /// The vendor's prompt-cache contract.
    pub fn prompt_cache(self) -> PromptCacheProfile {
        match self {
            // developers.openai.com prompt-caching guide: automatic, 1,024
            // tokens on GPT-5.6+ (2,048 before).
            Self::OpenAi => PromptCacheProfile {
                kind: PromptCacheKind::AutomaticPrefix,
                min_prefix_tokens: Some(1024),
                hit_usage_field: "prompt_tokens_details.cached_tokens",
            },
            // platform.claude.com prompt-caching: explicit breakpoints (or
            // the top-level automatic marker); 512–4,096 floor by model.
            Self::Anthropic => PromptCacheProfile {
                kind: PromptCacheKind::ExplicitBreakpoints,
                min_prefix_tokens: Some(1024),
                hit_usage_field: "cache_read_input_tokens",
            },
            // ai.google.dev/gemini-api/docs/caching: implicit caching on
            // every 2.5+ model; 4,096-token floor on the 3.x family
            // (2,048 on 2.5).
            Self::Gemini => PromptCacheProfile {
                kind: PromptCacheKind::AutomaticPrefix,
                min_prefix_tokens: Some(4096),
                hit_usage_field: "usage.total_cached_tokens",
            },
            // api-docs.deepseek.com/guides/kv_cache: disk prefix cache on
            // by default; no floor documented any more.
            Self::DeepSeek => PromptCacheProfile {
                kind: PromptCacheKind::AutomaticPrefix,
                min_prefix_tokens: None,
                hit_usage_field: "prompt_cache_hit_tokens",
            },
            // docs.bigmodel.cn/cn/guide/capabilities/cache: implicit only;
            // no floor or TTL is quantified.
            Self::Zhipu => PromptCacheProfile {
                kind: PromptCacheKind::AutomaticPrefix,
                min_prefix_tokens: None,
                hit_usage_field: "prompt_tokens_details.cached_tokens",
            },
            // platform.kimi.com context-caching guide: automatic once a
            // prompt exceeds 256 tokens; `prompt_cache_key` routes.
            Self::Kimi => PromptCacheProfile {
                kind: PromptCacheKind::AutomaticPrefix,
                min_prefix_tokens: Some(256),
                hit_usage_field: "cached_tokens",
            },
            // help.aliyun.com/zh/model-studio/context-cache: implicit from
            // 256 tokens; explicit `cache_control` on top (1,024 floor,
            // 5-minute TTL).
            Self::Qwen => PromptCacheProfile {
                kind: PromptCacheKind::AutomaticPrefix,
                min_prefix_tokens: Some(256),
                hit_usage_field: "prompt_tokens_details.cached_tokens",
            },
            // docs.volcengine.com/docs/82379/1398933: implicit from 1,024
            // tokens on Seed 2.0+; explicit only on the Responses API.
            Self::Volcengine => PromptCacheProfile {
                kind: PromptCacheKind::AutomaticPrefix,
                min_prefix_tokens: Some(1024),
                hit_usage_field: "prompt_tokens_details.cached_tokens",
            },
            // platform.minimax.io prompt-caching page: passive caching
            // from 512 input tokens, tools → system → messages order.
            Self::MiniMax => PromptCacheProfile {
                kind: PromptCacheKind::AutomaticPrefix,
                min_prefix_tokens: Some(512),
                hit_usage_field: "prompt_tokens_details.cached_tokens",
            },
            // siliconflow.cn/pricing has a cache-hit column and the usage
            // carries `prompt_cache_hit_tokens`, but no page describes the
            // mechanism; treat it as prefix-based, undocumented floor.
            Self::SiliconFlow => PromptCacheProfile {
                kind: PromptCacheKind::AutomaticPrefix,
                min_prefix_tokens: None,
                hit_usage_field: "prompt_cache_hit_tokens",
            },
            // Zen resells upstream models; the upstream cache applies and
            // its billing shows cache read/write lines.
            Self::OpenCode => PromptCacheProfile {
                kind: PromptCacheKind::AutomaticPrefix,
                min_prefix_tokens: None,
                hit_usage_field: "prompt_tokens_details.cached_tokens",
            },
            Self::Ollama | Self::Unknown => PromptCacheProfile::NONE,
        }
    }

    /// Where the model catalogue can be listed from, for an endpoint of the
    /// given wire family.
    pub fn model_listing(self, wire: WireFamily) -> ModelListing {
        const OPENAI_LIST: ModelListing = ModelListing::OpenAiCompatible {
            query: "",
            documented: true,
        };
        match (self, wire) {
            // OpenCode Zen serves every wire format from one catalogue, and
            // that catalogue is an unauthenticated OpenAI-shaped list.
            (Self::OpenCode, _) => OPENAI_LIST,
            // DashScope has no list on compatible-mode; the native list is
            // reachable from the same host and knows the limits.
            (Self::Qwen, _) => ModelListing::DashScope,
            (_, WireFamily::Anthropic) => ModelListing::Anthropic,
            (Self::Ollama, _) => ModelListing::Ollama,
            (Self::SiliconFlow, _) => ModelListing::OpenAiCompatible {
                query: "sub_type=chat",
                documented: true,
            },
            // docs.bigmodel.cn has no list-models page, but
            // `GET /api/paas/v4/models` answers in OpenAI's shape.
            (Self::Zhipu, _) => ModelListing::OpenAiCompatible {
                query: "",
                documented: false,
            },
            // Ark's data plane has no list; the management plane is
            // HMAC-signed and out of reach for an API key.
            (Self::Volcengine, _) => ModelListing::Unsupported,
            (_, _) => OPENAI_LIST,
        }
    }

    /// The models this vendor's documentation lists, flagship first.
    /// Empty for vendors whose catalogue is whatever the user installed
    /// (Ollama) or resold from elsewhere (OpenCode Zen).
    pub fn known_models(self) -> &'static [KnownModel] {
        match self {
            Self::OpenAi => OPENAI_MODELS,
            Self::Anthropic => ANTHROPIC_MODELS,
            Self::Gemini => GEMINI_MODELS,
            Self::DeepSeek => DEEPSEEK_MODELS,
            Self::Volcengine => VOLCENGINE_MODELS,
            Self::SiliconFlow => SILICONFLOW_MODELS,
            Self::Zhipu => ZHIPU_MODELS,
            Self::Qwen => QWEN_MODELS,
            Self::Kimi => KIMI_MODELS,
            Self::MiniMax => MINIMAX_MODELS,
            Self::Ollama | Self::OpenCode | Self::Unknown => &[],
        }
    }

    /// The documented limits for `model`, if this vendor's catalogue (or,
    /// for a reseller, any catalogue) lists it. Matching is
    /// case-insensitive and tolerates a dated snapshot suffix
    /// (`claude-sonnet-4-5-20250929` finds `claude-sonnet-4-5`; a Vertex
    /// `@20250929` suffix likewise).
    pub fn known_model(self, model: &str) -> Option<KnownModel> {
        let wanted = model.trim().to_ascii_lowercase();
        if wanted.is_empty() {
            return None;
        }
        let tables: Vec<&'static [KnownModel]> = match self {
            Self::Ollama => Vec::new(),
            Self::OpenCode | Self::Unknown => ProviderVendor::ALL
                .iter()
                .map(|vendor| vendor.known_models())
                .collect(),
            other => vec![other.known_models()],
        };
        let bare = wanted.split('@').next().unwrap_or(&wanted);
        for table in tables {
            if let Some(exact) = table
                .iter()
                .find(|entry| entry.id.eq_ignore_ascii_case(bare))
            {
                return Some(*exact);
            }
        }
        // Dated snapshot of a known id: `<id>-YYYYMMDD` or `<id>-YYYY-MM-DD`.
        for table in ProviderVendor::ALL.iter().map(|v| v.known_models()) {
            if let Some(entry) = table.iter().find(|entry| {
                let id = entry.id.to_ascii_lowercase();
                bare.strip_prefix(id.as_str())
                    .and_then(|rest| rest.strip_prefix('-'))
                    .is_some_and(|rest| {
                        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '-')
                    })
            }) {
                return Some(*entry);
            }
        }
        None
    }

    /// Whether a model id from this vendor's catalogue is a chat model a
    /// coding agent can drive, as opposed to an embedding, speech, image,
    /// moderation or realtime endpoint that shares the same list.
    ///
    /// Conservative on purpose: an id this cannot place is kept, because
    /// hiding a real model is worse than showing an odd one.
    pub fn is_chat_model_id(self, id: &str) -> bool {
        let id = id.trim().to_ascii_lowercase();
        if id.is_empty() {
            return false;
        }
        const NON_CHAT_MARKERS: &[&str] = &[
            "embedding",
            "embed-",
            "-tts",
            "tts-",
            "transcribe",
            "whisper",
            "realtime",
            "moderation",
            "dall-e",
            "gpt-image",
            "image-",
            "imagen",
            "veo-",
            "sora",
            "-audio",
            "audio-",
            "rerank",
            "speech",
            "vector",
            "clip-",
            "daybreak-",
            "-cyber",
            "text-similarity",
            "text-search",
            "davinci",
            "babbage",
            "curie",
            "ada-",
            "codex-mini",
            "computer-use-preview",
            "cogview",
            "cogvideo",
            "charglm",
            "emohaa",
            "wanx",
            "stable-diffusion",
            "flux",
            "kolors",
            "sensevoice",
            "cosyvoice",
            "qwen-vl-ocr",
            "qwen-mt",
            "qwen-audio",
            "qwen-omni",
            "qwen-image",
            "seedream",
            "seedance",
            "seededit",
            "doubao-embedding",
            "doubao-tts",
            "doubao-vision-embedding",
            "-asr",
            "text2image",
            "hailuo",
            "music-",
            "video-",
            "voice",
            "learnlm",
            "aqa",
        ];
        !NON_CHAT_MARKERS.iter().any(|marker| id.contains(marker))
    }

    /// OpenCode Zen routes each model over one wire format, chosen by the
    /// model's family; a Zen entry configured for one format can only run
    /// the models that speak it. Everything else is `true`.
    pub fn model_speaks_wire(self, id: &str, wire: WireFamily) -> bool {
        if self != Self::OpenCode {
            return true;
        }
        let id = id.trim().to_ascii_lowercase();
        let responses =
            id.starts_with("gpt-") || id.starts_with("grok-") || id.starts_with("muse-");
        let anthropic = id.starts_with("claude-") || id.starts_with("qwen");
        let google = id.starts_with("gemini-");
        match wire {
            WireFamily::OpenAiResponses => responses,
            WireFamily::Anthropic => anthropic,
            WireFamily::OpenAiChat => !responses && !anthropic && !google,
        }
    }
}

impl fmt::Display for ProviderVendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// The wire protocol an endpoint speaks, as far as listing models cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireFamily {
    /// Chat Completions.
    OpenAiChat,
    /// The Responses API — shares OpenAI's `/models`.
    OpenAiResponses,
    /// Anthropic Messages.
    Anthropic,
}

/// Lower-cased host (no port) of a URL, or `None` when there is none.
pub fn host_of(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let rest = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Whether a URL names the ChatGPT Codex Responses endpoint.
///
/// Keep endpoint recognition beside the vendor table and compare the parsed
/// host exactly; substring checks would accept attacker-controlled lookalikes.
pub(crate) fn is_chatgpt_codex_endpoint(url: &str) -> bool {
    if host_of(url).as_deref() != Some("chatgpt.com") {
        return false;
    }
    let rest = url
        .trim()
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url.trim());
    let path = rest
        .split_once('/')
        .map(|(_, path)| path)
        .unwrap_or("")
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    path == "backend-api/codex" || path.starts_with("backend-api/codex/")
}

/// Ollama has no fixed host — it is wherever the user runs it — but it does
/// have a fixed default port, and that is what a default install exposes.
fn is_ollama_host(host: &str, base_url: &str) -> bool {
    if host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "0.0.0.0" {
        let rest = base_url
            .trim()
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(base_url.trim());
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        return authority.ends_with(":11434");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatgpt_codex_endpoint_requires_exact_host_and_path() {
        assert!(is_chatgpt_codex_endpoint(
            "https://chatgpt.com/backend-api/codex"
        ));
        assert!(is_chatgpt_codex_endpoint(
            "https://chatgpt.com/backend-api/codex/responses"
        ));
        assert!(!is_chatgpt_codex_endpoint(
            "https://chatgpt.com.evil.example/backend-api/codex"
        ));
        assert!(!is_chatgpt_codex_endpoint(
            "https://relay.example/chatgpt.com/backend-api/codex"
        ));
        assert!(!is_chatgpt_codex_endpoint("https://chatgpt.com/other"));
    }

    #[test]
    fn every_vendor_round_trips_through_its_id() {
        for vendor in ProviderVendor::ALL {
            assert_eq!(
                ProviderVendor::parse(vendor.id()),
                Some(*vendor),
                "{vendor:?}"
            );
            assert_eq!(
                ProviderVendor::parse(&vendor.id().to_ascii_uppercase()),
                Some(*vendor)
            );
        }
        assert_eq!(ProviderVendor::parse("unknown"), None);
        assert_eq!(ProviderVendor::parse(""), None);
    }

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<&str> = ProviderVendor::ALL.iter().map(|v| v.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ProviderVendor::ALL.len());
    }

    #[test]
    fn detects_each_documented_host() {
        let cases = [
            ("https://api.openai.com/v1", ProviderVendor::OpenAi),
            (
                "https://chatgpt.com/backend-api/codex",
                ProviderVendor::OpenAi,
            ),
            ("https://api.anthropic.com", ProviderVendor::Anthropic),
            (
                "https://aiplatform.googleapis.com/v1/projects/p/locations/global",
                ProviderVendor::Anthropic,
            ),
            (
                "https://us-east5-aiplatform.googleapis.com/v1/projects/p/locations/us-east5",
                ProviderVendor::Anthropic,
            ),
            (
                "https://aiplatform.us.rep.googleapis.com/v1/projects/p/locations/us",
                ProviderVendor::Anthropic,
            ),
            (
                "https://bedrock-mantle.us-east-1.api.aws/anthropic",
                ProviderVendor::Anthropic,
            ),
            (
                "https://my-resource.services.ai.azure.com/anthropic",
                ProviderVendor::Anthropic,
            ),
            (
                "https://generativelanguage.googleapis.com/v1beta/openai",
                ProviderVendor::Gemini,
            ),
            ("http://localhost:11434/v1", ProviderVendor::Ollama),
            ("http://127.0.0.1:11434", ProviderVendor::Ollama),
            ("https://opencode.ai/zen/v1", ProviderVendor::OpenCode),
            ("https://api.deepseek.com", ProviderVendor::DeepSeek),
            ("https://api.deepseek.com/v1/", ProviderVendor::DeepSeek),
            (
                "https://ark.cn-beijing.volces.com/api/v3",
                ProviderVendor::Volcengine,
            ),
            ("https://api.siliconflow.cn/v1", ProviderVendor::SiliconFlow),
            (
                "https://open.bigmodel.cn/api/paas/v4",
                ProviderVendor::Zhipu,
            ),
            ("https://api.z.ai/api/paas/v4", ProviderVendor::Zhipu),
            (
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
                ProviderVendor::Qwen,
            ),
            (
                "https://ws-123.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
                ProviderVendor::Qwen,
            ),
            ("https://api.moonshot.cn/v1", ProviderVendor::Kimi),
            ("https://api.minimaxi.com/v1", ProviderVendor::MiniMax),
            ("https://api.minimax.io/v1", ProviderVendor::MiniMax),
            ("https://relay.example.com/v1", ProviderVendor::Unknown),
            ("http://localhost:8080/v1", ProviderVendor::Unknown),
            ("", ProviderVendor::Unknown),
            ("not a url", ProviderVendor::Unknown),
        ];
        for (url, expected) in cases {
            assert_eq!(ProviderVendor::detect(url), expected, "{url}");
        }
    }

    #[test]
    fn host_is_matched_exactly_not_by_substring() {
        // A look-alike host must not inherit a vendor's wire quirks.
        assert_eq!(
            ProviderVendor::detect("https://api.deepseek.com.evil.example/v1"),
            ProviderVendor::Unknown
        );
        assert_eq!(
            ProviderVendor::detect("https://notapi.openai.com/v1"),
            ProviderVendor::Unknown
        );
        assert_eq!(
            ProviderVendor::detect("https://bedrock-mantle.example.com/anthropic"),
            ProviderVendor::Unknown
        );
    }

    #[test]
    fn host_of_handles_ports_credentials_and_paths() {
        assert_eq!(
            host_of("https://Api.Example.com:8443/v1/x?y=1"),
            Some("api.example.com".into())
        );
        assert_eq!(
            host_of("http://user:pw@host.local/v1"),
            Some("host.local".into())
        );
        assert_eq!(host_of("http://[::1]:11434/v1"), Some("::1".into()));
        assert_eq!(host_of("api.deepseek.com"), Some("api.deepseek.com".into()));
        assert_eq!(host_of(""), None);
        assert_eq!(host_of("https:///nohost"), None);
    }

    #[test]
    fn a_pinned_vendor_beats_host_detection() {
        assert_eq!(
            ProviderVendor::resolve(Some("deepseek"), "https://relay.example.com/v1"),
            ProviderVendor::DeepSeek
        );
        assert_eq!(
            ProviderVendor::resolve(Some("glm"), "https://api.deepseek.com"),
            ProviderVendor::Zhipu
        );
        // An unparseable pin falls back to the host rather than to Unknown.
        assert_eq!(
            ProviderVendor::resolve(Some("nonsense"), "https://api.deepseek.com"),
            ProviderVendor::DeepSeek
        );
        assert_eq!(
            ProviderVendor::resolve(None, "https://api.moonshot.cn/v1"),
            ProviderVendor::Kimi
        );
    }

    #[test]
    fn listing_surface_follows_the_vendor_docs() {
        for vendor in ProviderVendor::ALL {
            let expected = match vendor {
                ProviderVendor::OpenCode => ModelListing::OpenAiCompatible {
                    query: "",
                    documented: true,
                },
                ProviderVendor::Qwen => ModelListing::DashScope,
                _ => ModelListing::Anthropic,
            };
            assert_eq!(
                vendor.model_listing(WireFamily::Anthropic),
                expected,
                "{vendor:?}"
            );
        }
        assert_eq!(
            ProviderVendor::Ollama.model_listing(WireFamily::OpenAiChat),
            ModelListing::Ollama
        );
        assert_eq!(
            ProviderVendor::Volcengine.model_listing(WireFamily::OpenAiChat),
            ModelListing::Unsupported
        );
        assert_eq!(
            ProviderVendor::Zhipu.model_listing(WireFamily::OpenAiChat),
            ModelListing::OpenAiCompatible {
                query: "",
                documented: false
            }
        );
        assert_eq!(
            ProviderVendor::SiliconFlow.model_listing(WireFamily::OpenAiChat),
            ModelListing::OpenAiCompatible {
                query: "sub_type=chat",
                documented: true
            }
        );
        assert_eq!(
            ProviderVendor::OpenAi.model_listing(WireFamily::OpenAiResponses),
            ModelListing::OpenAiCompatible {
                query: "",
                documented: true
            }
        );
    }

    #[test]
    fn prefix_cache_vendors_are_the_ones_that_document_one() {
        for vendor in [
            ProviderVendor::OpenAi,
            ProviderVendor::Anthropic,
            ProviderVendor::Gemini,
            ProviderVendor::DeepSeek,
            ProviderVendor::Zhipu,
            ProviderVendor::Kimi,
            ProviderVendor::Qwen,
            ProviderVendor::Volcengine,
            ProviderVendor::MiniMax,
        ] {
            assert!(vendor.prompt_cache().is_prefix_based(), "{vendor:?}");
        }
        assert!(!ProviderVendor::Unknown.prompt_cache().is_prefix_based());
        assert!(!ProviderVendor::Ollama.prompt_cache().is_prefix_based());
        assert_eq!(
            ProviderVendor::Kimi.prompt_cache().min_prefix_tokens,
            Some(256)
        );
        assert_eq!(
            ProviderVendor::MiniMax.prompt_cache().min_prefix_tokens,
            Some(512)
        );
        assert_eq!(
            ProviderVendor::Volcengine.prompt_cache().min_prefix_tokens,
            Some(1024)
        );
    }

    #[test]
    fn chat_model_filter_drops_the_shared_list_noise_and_keeps_models() {
        let v = ProviderVendor::OpenAi;
        for kept in [
            "gpt-5.6-sol",
            "gpt-5.5",
            "o3",
            "deepseek-v4-pro",
            "claude-opus-5",
            "gemini-3.7-flash",
            "kimi-k3",
            "MiniMax-M3",
            "glm-5.3",
            "qwen3.7-plus",
            "doubao-seed-2-1-pro-260628",
            "Qwen/Qwen3-Coder-480B-A35B-Instruct",
            "llama3.3:70b",
            "something-nobody-knows",
        ] {
            assert!(v.is_chat_model_id(kept), "{kept}");
        }
        for dropped in [
            "text-embedding-3-large",
            "gpt-4o-mini-tts",
            "gpt-realtime-2.1",
            "gpt-4o-transcribe",
            "whisper-1",
            "dall-e-3",
            "gpt-image-2",
            "omni-moderation-latest",
            "gemini-embedding-001",
            "imagen-4.0-generate-001",
            "cogview-4",
            "BAAI/bge-reranker-v2-m3",
            "qwen-tts",
            "doubao-embedding-large",
            "daybreak-red-latest",
            "gpt-5.6-cyber",
            "",
        ] {
            assert!(!v.is_chat_model_id(dropped), "{dropped}");
        }
    }

    #[test]
    fn zen_models_are_split_by_the_wire_their_family_speaks() {
        let zen = ProviderVendor::OpenCode;
        assert!(zen.model_speaks_wire("gpt-5.6-sol", WireFamily::OpenAiResponses));
        assert!(zen.model_speaks_wire("grok-4.6", WireFamily::OpenAiResponses));
        assert!(!zen.model_speaks_wire("gpt-5.6-sol", WireFamily::OpenAiChat));
        assert!(zen.model_speaks_wire("claude-opus-5", WireFamily::Anthropic));
        assert!(zen.model_speaks_wire("qwen3.7-max", WireFamily::Anthropic));
        assert!(!zen.model_speaks_wire("claude-opus-5", WireFamily::OpenAiChat));
        assert!(zen.model_speaks_wire("kimi-k3", WireFamily::OpenAiChat));
        assert!(zen.model_speaks_wire("deepseek-v4-pro", WireFamily::OpenAiChat));
        assert!(zen.model_speaks_wire("big-pickle", WireFamily::OpenAiChat));
        // Gemini speaks Google's own wire, which Rebon does not have.
        assert!(!zen.model_speaks_wire("gemini-3.7-flash", WireFamily::OpenAiChat));
        assert!(!zen.model_speaks_wire("gemini-3.7-flash", WireFamily::Anthropic));
        // Everyone else runs every model over the configured wire.
        assert!(
            ProviderVendor::DeepSeek.model_speaks_wire("deepseek-v4-pro", WireFamily::Anthropic)
        );
    }

    #[test]
    fn kimi_rules_follow_the_model_generation() {
        let k3 = ProviderVendor::Kimi.chat_wire_rules("kimi-k3");
        assert_eq!(k3.thinking, ThinkingDialect::None);
        assert_eq!(k3.effort, EffortVocabulary::LowHighMax);
        assert_eq!(k3.max_tokens_field, MaxTokensField::MaxCompletionTokens);
        let k2 = ProviderVendor::Kimi.chat_wire_rules("kimi-k2.6");
        assert_eq!(k2.thinking, ThinkingDialect::ThinkingType);
        assert_eq!(k2.effort, EffortVocabulary::Unsupported);
        assert!(k2.replay_reasoning_content);
    }

    #[test]
    fn effort_vocabularies_fold_as_documented() {
        assert_eq!(EffortVocabulary::Passthrough.fold("xhigh"), Some("xhigh"));
        assert_eq!(EffortVocabulary::LowHighMax.fold("medium"), Some("high"));
        assert_eq!(EffortVocabulary::LowHighMax.fold("xhigh"), Some("max"));
        assert_eq!(EffortVocabulary::LowHighMax.fold("low"), Some("low"));
        assert_eq!(EffortVocabulary::LowMediumHigh.fold("max"), Some("high"));
        assert_eq!(
            EffortVocabulary::LowMediumHigh.fold("medium"),
            Some("medium")
        );
        assert_eq!(EffortVocabulary::Unsupported.fold("high"), None);
        assert_eq!(EffortVocabulary::Passthrough.fold("ultra"), None);
    }

    #[test]
    fn minimax_strips_what_its_reference_lists_as_unsupported() {
        let rules = ProviderVendor::MiniMax.chat_wire_rules("MiniMax-M3");
        assert!(rules.unsupported_fields.contains(&"tool_choice"));
        assert!(rules.unsupported_fields.contains(&"stop"));
        assert!(rules.reasoning_split);
        assert_eq!(rules.thinking, ThinkingDialect::AdaptiveThinkingType);
        assert_eq!(rules.effort, EffortVocabulary::Unsupported);
        assert_eq!(rules.max_tokens_field, MaxTokensField::MaxCompletionTokens);
    }

    #[test]
    fn qwen_moves_the_cap_to_max_completion_tokens() {
        let rules = ProviderVendor::Qwen.chat_wire_rules("qwen3.7-plus");
        assert_eq!(rules.max_tokens_field, MaxTokensField::MaxCompletionTokens);
        assert_eq!(rules.thinking, ThinkingDialect::EnableThinking);
        assert_eq!(rules.effort, EffortVocabulary::Unsupported);
    }

    #[test]
    fn volcengine_replays_the_encrypted_reasoning_copy() {
        let rules = ProviderVendor::Volcengine.chat_wire_rules("doubao-seed-evolving");
        assert!(rules.replay_reasoning_content);
        assert!(rules.replay_encrypted_reasoning);
        assert_eq!(rules.max_tokens_field, MaxTokensField::MaxTokens);
        for other in ProviderVendor::ALL
            .iter()
            .filter(|v| **v != ProviderVendor::Volcengine)
        {
            assert!(
                !other.chat_wire_rules("m").replay_encrypted_reasoning,
                "{other:?}"
            );
        }
    }

    #[test]
    fn known_models_start_with_the_flagship_and_are_unique_per_vendor() {
        for vendor in ProviderVendor::ALL {
            let models = vendor.known_models();
            let mut ids: Vec<&str> = models.iter().map(|m| m.id).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), models.len(), "{vendor:?} has a duplicate id");
            for model in models {
                assert!(model.context_window > 0, "{vendor:?} {}", model.id);
                assert!(vendor.is_chat_model_id(model.id), "{vendor:?} {}", model.id);
            }
        }
        assert_eq!(ProviderVendor::OpenAi.known_models()[0].id, "gpt-5.6-sol");
        assert_eq!(
            ProviderVendor::Anthropic.known_models()[0].id,
            "claude-opus-5"
        );
        assert_eq!(
            ProviderVendor::DeepSeek.known_models()[0].id,
            "deepseek-v4-pro"
        );
        assert!(ProviderVendor::Ollama.known_models().is_empty());
    }

    #[test]
    fn known_model_lookup_tolerates_snapshots_and_resellers() {
        let sonnet = ProviderVendor::Anthropic
            .known_model("claude-sonnet-4-5-20250929")
            .unwrap();
        assert_eq!(sonnet.context_window, 200_000);
        let vertex = ProviderVendor::Anthropic
            .known_model("claude-sonnet-4-5@20250929")
            .unwrap();
        assert_eq!(vertex.id, "claude-sonnet-4-5");
        let deepseek = ProviderVendor::DeepSeek
            .known_model("DeepSeek-V4-Pro")
            .unwrap();
        assert_eq!(deepseek.max_output_tokens, Some(384_000));
        // A reseller finds any vendor's model.
        assert_eq!(
            ProviderVendor::OpenCode
                .known_model("kimi-k3")
                .unwrap()
                .context_window,
            1_048_576
        );
        // Ollama's catalogue is whatever is installed; nothing is "known".
        assert!(ProviderVendor::Ollama.known_model("llama3.3").is_none());
        assert!(ProviderVendor::OpenAi.known_model("").is_none());
        assert!(ProviderVendor::OpenAi.known_model("gpt-9").is_none());
    }
}
