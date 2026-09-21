use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::events::{ContentBlockDelta, ContentBlockStart, MessageDeltaFields, StreamEvent};
use crate::request::{
    CreateMessageRequest, ReasoningEffort, ReasoningMode, ReasoningSummary, ThinkingConfig,
    WebSearchToolConfig, WebSearchUserLocation,
};
use crate::types::{
    CompactionBlock, ContentBlock, DocumentBlock, DocumentSource, GeneratedImageBlock, ImageBlock,
    ImageSource, Message, Role, ServerToolUseBlock, StopReason, TextBlock, ThinkingBlock, Tool,
    ToolChoice, ToolResultBlock, ToolResultContent, ToolResultContentBlock, ToolUseBlock, Usage,
    WebSearchResultBlock,
};

pub const MODEL_PROVIDER_PROTOCOL_VERSION: u32 = 1;
pub const MODEL_PROVIDER_PROTOCOL_NAME: &str = "rebon.modelProvider";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderInitializeParamsV1 {
    pub protocol_version: u32,
    pub client_info: ModelProviderClientInfoV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ProviderConnectionConfigV1>,
}

/// User-configured connection settings for the selected provider entry,
/// resolved from `config.json` and handed to the plugin at initialize time.
/// Absent when the user configured nothing beyond selecting the provider —
/// a plugin must fall back to its own defaults (bundled base URL, env vars).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderConnectionConfigV1 {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Provider-level request body options as configured by the user
    /// (`{"body": {...}, "extraBody": {...}, "omitBodyFields": [...]}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_options: Option<serde_json::Value>,
    /// Per-model request body options, same shape as `request_options`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_request_options: BTreeMap<String, serde_json::Value>,
}

impl ProviderConnectionConfigV1 {
    pub fn is_empty(&self) -> bool {
        self.base_url.is_empty()
            && self.api_key.is_empty()
            && self.headers.is_empty()
            && self.request_options.is_none()
            && self.model_request_options.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderClientInfoV1 {
    pub name: String,
    pub version: String,
}

/// One turn, as it travels on the plugin plane's `llm/stream`.
///
/// The connection rides the turn rather than an `initialize` handshake, and
/// that is the substantive difference between this and the child-process
/// protocol it replaces. A provider used to be a process per provider *per
/// configuration*, so the configuration could be handed over once at startup;
/// on a shared host it belongs to the call, because the same loaded adapter
/// serves whichever provider entry the user has selected right now.
///
/// The turn's own id is the plugin call id, so there is no `streamId` here:
/// the transport already has one, and a second would be a second thing to keep
/// in step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderTurnV1 {
    pub protocol_version: u32,
    /// What the user configured for this provider entry. Absent means "nothing
    /// beyond selecting it" — the adapter falls back to its own defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ProviderConnectionConfigV1>,
    pub request: CreateMessageRequestV1,
}

/// What an adapter says about itself in its ready report.
///
/// The successor to the `initialize` result, and wider: capabilities were all
/// that handshake could carry, while a package manifest separately declared
/// the models and the default. Both now arrive together, from the adapter that
/// knows, at the one moment before anything routes to it.
///
/// Every field is optional. An adapter that reports nothing leaves whatever
/// its package manifest declared standing, which is the same union the
/// handshake's capabilities had.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderAdapterInfoV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
    #[serde(default)]
    pub capabilities: ModelProviderRuntimeCapabilitiesV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderInitializeResultV1 {
    pub protocol_version: u32,
    #[serde(default)]
    pub capabilities: ModelProviderRuntimeCapabilitiesV1,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderRuntimeCapabilitiesV1 {
    #[serde(default)]
    pub request_scoped_transient_context: bool,
    #[serde(default)]
    pub forced_tool_choice: bool,
    #[serde(default)]
    pub web_search: bool,
    /// Accepted and ignored: hosted image generation was removed, so
    /// no request ever asks a provider for it. Kept so plugins that
    /// still declare it pass `deny_unknown_fields`.
    #[serde(default)]
    pub image_generation: bool,
    #[serde(default)]
    pub computer_use: bool,
    #[serde(default)]
    pub context_management: bool,
    /// Provider keeps server-side response state (e.g. `previous_response_id`).
    #[serde(default)]
    pub stateful_responses: bool,
    /// Provider offers a server-side compaction endpoint of its own.
    #[serde(default)]
    pub remote_compaction: bool,
    /// Provider opts into first-request Minimal anchoring followed by normal projection.
    #[serde(default)]
    pub anchored_minimal: bool,
    /// Provider streams raw reasoning text (`thinking_delta` events).
    #[serde(default)]
    pub reasoning_text: bool,
    /// Provider emits freeform custom tool calls translated to `tool_use`.
    #[serde(default)]
    pub custom_tool_call: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderCreateStreamParamsV1 {
    pub protocol_version: u32,
    pub stream_id: String,
    pub request: CreateMessageRequestV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderCreateStreamResultV1 {
    #[serde(default)]
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderStreamControlParamsV1 {
    pub protocol_version: u32,
    pub stream_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderLifecycleParamsV1 {
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderStreamEventNotificationV1 {
    pub protocol_version: u32,
    pub stream_id: String,
    pub event: StreamEventV1,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateMessageRequestV1 {
    pub model: String,
    pub messages: Vec<MessageV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient_context: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoiceV1>,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfigV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffortV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_mode: Option<ReasoningModeV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_summary: Option<ReasoningSummaryV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<WebSearchToolConfigV1>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageV1 {
    pub role: RoleV1,
    pub content: Vec<ContentBlockV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleV1 {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlockV1 {
    Text {
        text: String,
    },
    Image {
        source: ImageSourceV1,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: ToolResultContentV1,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
    ServerToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    WebSearchResult {
        tool_use_id: String,
        results: Vec<SearchResultEntryV1>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_content: Option<serde_json::Value>,
    },
    // Plaintext summaries only. The protocol enums are
    // `deny_unknown_fields`, so an `encrypted_content` field here would
    // break every plugin built against v1 the moment a blob appeared —
    // and a blob is meaningless to an external provider anyway, so
    // `MessageV1::from_message` drops those blocks instead.
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    GeneratedImage {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revised_prompt: Option<String>,
        media_type: String,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        saved_path: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageSourceV1 {
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContentV1 {
    Text(String),
    Blocks(Vec<ToolResultContentBlockV1>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolResultContentBlockV1 {
    Text { text: String },
    Image { source: ImageSourceV1 },
    Document { source: DocumentSourceV1 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentSourceV1 {
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchResultEntryV1 {
    pub title: Option<String>,
    pub url: Option<String>,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolV1 {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolChoiceV1 {
    Auto,
    Any,
    Tool { name: String },
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThinkingConfigV1 {
    Enabled { budget_tokens: u32 },
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffortV1 {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningModeV1 {
    Pro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummaryV1 {
    Auto,
    Concise,
    Detailed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebSearchToolConfigV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_context_size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<WebSearchUserLocationV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebSearchUserLocationV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StreamEventV1 {
    MessageStart {
        message_id: String,
        model: String,
        usage: UsageV1,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockStartV1,
    },
    ContentBlockDelta {
        index: usize,
        delta: ContentBlockDeltaV1,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaFieldsV1,
    },
    MessageStop,
    Error {
        error_type: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlockStartV1 {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
    ServerToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    WebSearchResult {
        tool_use_id: String,
        #[serde(default)]
        content: serde_json::Value,
    },
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    ImageGeneration {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlockDeltaV1 {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    ThinkingDataDelta {
        data: String,
    },
    SignatureDelta {
        signature: String,
    },
    CompactionDelta {
        content: String,
    },
    ImageDataDelta {
        b64_json: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial_index: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revised_prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageDeltaFieldsV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReasonV1>,
    #[serde(default)]
    pub usage: UsageV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReasonV1 {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    PauseTurn,
    Refusal,
    Compaction,
    ModelContextWindowExceeded,
    Other(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageV1 {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub prompt_cache_hit_tokens: u32,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u32,
    #[serde(default)]
    pub total_input_tokens: u32,
    #[serde(default)]
    pub total_output_tokens: u32,
    #[serde(default)]
    pub reasoning_tokens: u32,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ModelProviderProtocolError {
    #[error("unsupported model-provider protocol version {found}; expected {expected}")]
    UnsupportedVersion { expected: u32, found: u32 },
    #[error("model-provider protocol does not support request field `{0}`")]
    UnsupportedRequestField(&'static str),
    #[error("model-provider protocol conversion failed: {0}")]
    Conversion(String),
}

pub type ProtocolResult<T> = Result<T, ModelProviderProtocolError>;

pub fn ensure_protocol_version(found: u32) -> ProtocolResult<()> {
    if found == MODEL_PROVIDER_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ModelProviderProtocolError::UnsupportedVersion {
            expected: MODEL_PROVIDER_PROTOCOL_VERSION,
            found,
        })
    }
}

impl CreateMessageRequestV1 {
    pub fn from_request(request: &CreateMessageRequest) -> ProtocolResult<Self> {
        if request.context_management.is_some() {
            return Err(ModelProviderProtocolError::UnsupportedRequestField(
                "context_management",
            ));
        }
        // Prompt-cache routing travels in `extensions` so older plugins (which
        // never look at the key) keep working without a protocol bump. The
        // retention value is Rebon's internal hint ("session", ...) — plugins
        // that forward it to an endpoint own the mapping to that endpoint's
        // wire values.
        let mut extensions = BTreeMap::new();
        if let Some(trace) = request.cache_trace_context.as_ref() {
            let mut prompt_cache = serde_json::Map::new();
            if let Some(key) = trace.prompt_cache_key.as_deref() {
                prompt_cache.insert("key".into(), serde_json::Value::String(key.to_string()));
            }
            if let Some(retention) = trace.prompt_cache_retention.as_deref() {
                prompt_cache.insert(
                    "retention".into(),
                    serde_json::Value::String(retention.to_string()),
                );
            }
            if !prompt_cache.is_empty() {
                extensions.insert(
                    "promptCache".to_string(),
                    serde_json::Value::Object(prompt_cache),
                );
            }
        }
        Ok(Self {
            model: request.model.clone(),
            messages: request
                .messages
                .iter()
                .map(MessageV1::from_message)
                .collect::<ProtocolResult<Vec<_>>>()?,
            system: request.system.clone(),
            transient_context: request.transient_context.clone(),
            tools: request.tools.iter().map(ToolV1::from_tool).collect(),
            tool_choice: request
                .tool_choice
                .as_ref()
                .map(ToolChoiceV1::from_tool_choice),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stop_sequences: request.stop_sequences.clone(),
            stream: true,
            metadata: request.metadata.clone(),
            thinking: request
                .thinking
                .as_ref()
                .map(ThinkingConfigV1::from_thinking),
            reasoning_effort: request.reasoning_effort.map(ReasoningEffortV1::from_effort),
            reasoning_mode: request.reasoning_mode.map(ReasoningModeV1::from_mode),
            reasoning_summary: request
                .reasoning_summary
                .map(ReasoningSummaryV1::from_summary),
            web_search: request
                .web_search
                .as_ref()
                .map(WebSearchToolConfigV1::from_config),
            extensions,
        })
    }
}

impl MessageV1 {
    fn from_message(message: &Message) -> ProtocolResult<Self> {
        Ok(Self {
            role: RoleV1::from_role(message.role),
            content: message
                .content
                .iter()
                // A server-side compaction blob is readable only by the
                // backend that minted it, so it has nothing to say to an
                // external provider — and v1 has no field to carry it.
                // Dropping it loses the history it stood in for, which is
                // the same loss any provider switch causes here.
                .filter(|block| !block.is_backend_bound_compaction())
                .map(ContentBlockV1::from_content_block)
                .collect::<ProtocolResult<Vec<_>>>()?,
        })
    }
}

impl RoleV1 {
    fn from_role(role: Role) -> Self {
        match role {
            Role::User => Self::User,
            Role::Assistant => Self::Assistant,
            Role::System => Self::System,
        }
    }
}

impl ContentBlockV1 {
    fn from_content_block(block: &ContentBlock) -> ProtocolResult<Self> {
        Ok(match block {
            ContentBlock::Text(TextBlock { text }) => Self::Text { text: text.clone() },
            ContentBlock::Image(ImageBlock { source }) => Self::Image {
                source: ImageSourceV1::from_image_source(source),
            },
            ContentBlock::ToolUse(ToolUseBlock { id, name, input }) => Self::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            ContentBlock::ToolResult(ToolResultBlock {
                tool_use_id,
                content,
                is_error,
            }) => Self::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: ToolResultContentV1::from_content(content),
                is_error: *is_error,
            },
            ContentBlock::Thinking(ThinkingBlock {
                thinking,
                signature,
                data,
            }) => Self::Thinking {
                thinking: thinking.clone(),
                signature: signature.clone(),
                data: data.clone(),
            },
            ContentBlock::ServerToolUse(ServerToolUseBlock { id, name, input }) => {
                Self::ServerToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }
            }
            ContentBlock::WebSearchResult(WebSearchResultBlock {
                tool_use_id,
                results,
                raw_content,
            }) => Self::WebSearchResult {
                tool_use_id: tool_use_id.clone(),
                results: results
                    .iter()
                    .map(|entry| SearchResultEntryV1 {
                        title: Some(entry.title.clone()),
                        url: Some(entry.url.clone()),
                        snippet: entry.snippet.clone(),
                    })
                    .collect(),
                raw_content: raw_content.clone(),
            },
            ContentBlock::Compaction(CompactionBlock { content, .. }) => Self::Compaction {
                content: content.clone(),
            },
            ContentBlock::GeneratedImage(GeneratedImageBlock {
                id,
                status,
                revised_prompt,
                media_type,
                data,
                saved_path,
            }) => Self::GeneratedImage {
                id: id.clone(),
                status: status.clone(),
                revised_prompt: revised_prompt.clone(),
                media_type: media_type.clone(),
                data: data.clone(),
                saved_path: saved_path.clone(),
            },
        })
    }
}

impl ImageSourceV1 {
    fn from_image_source(source: &ImageSource) -> Self {
        Self {
            kind: source.kind.clone(),
            media_type: source.media_type.clone(),
            data: source.data.clone(),
        }
    }
}

impl DocumentSourceV1 {
    fn from_document_source(source: &DocumentSource) -> Self {
        Self {
            kind: source.kind.clone(),
            media_type: source.media_type.clone(),
            data: source.data.clone(),
        }
    }
}

impl ToolResultContentV1 {
    fn from_content(content: &ToolResultContent) -> Self {
        match content {
            ToolResultContent::Text(text) => Self::Text(text.clone()),
            ToolResultContent::Blocks(blocks) => Self::Blocks(
                blocks
                    .iter()
                    .map(ToolResultContentBlockV1::from_block)
                    .collect(),
            ),
        }
    }
}

impl ToolResultContentBlockV1 {
    fn from_block(block: &ToolResultContentBlock) -> Self {
        match block {
            ToolResultContentBlock::Text(TextBlock { text }) => Self::Text { text: text.clone() },
            ToolResultContentBlock::Image(ImageBlock { source }) => Self::Image {
                source: ImageSourceV1::from_image_source(source),
            },
            ToolResultContentBlock::Document(DocumentBlock { source }) => Self::Document {
                source: DocumentSourceV1::from_document_source(source),
            },
        }
    }
}

impl ToolV1 {
    fn from_tool(tool: &Tool) -> Self {
        Self {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        }
    }
}

impl ToolChoiceV1 {
    fn from_tool_choice(choice: &ToolChoice) -> Self {
        match choice {
            ToolChoice::Auto => Self::Auto,
            ToolChoice::Any => Self::Any,
            ToolChoice::Tool { name } => Self::Tool { name: name.clone() },
            ToolChoice::None => Self::None,
        }
    }
}

impl ThinkingConfigV1 {
    fn from_thinking(thinking: &ThinkingConfig) -> Self {
        match thinking {
            ThinkingConfig::Enabled { budget_tokens } => Self::Enabled {
                budget_tokens: *budget_tokens,
            },
            ThinkingConfig::Disabled => Self::Disabled,
        }
    }
}

impl ReasoningEffortV1 {
    fn from_effort(effort: ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
            ReasoningEffort::XHigh => Self::Xhigh,
            ReasoningEffort::Max => Self::Max,
        }
    }
}

impl ReasoningModeV1 {
    fn from_mode(mode: ReasoningMode) -> Self {
        match mode {
            ReasoningMode::Pro => Self::Pro,
        }
    }
}

impl ReasoningSummaryV1 {
    fn from_summary(summary: ReasoningSummary) -> Self {
        match summary {
            ReasoningSummary::Auto => Self::Auto,
            ReasoningSummary::Concise => Self::Concise,
            ReasoningSummary::Detailed => Self::Detailed,
        }
    }
}

impl WebSearchToolConfigV1 {
    fn from_config(config: &WebSearchToolConfig) -> Self {
        Self {
            allowed_domains: config.allowed_domains.clone(),
            blocked_domains: config.blocked_domains.clone(),
            max_uses: config.max_uses,
            search_context_size: config.search_context_size.clone(),
            user_location: config
                .user_location
                .as_ref()
                .map(WebSearchUserLocationV1::from_location),
        }
    }
}

impl WebSearchUserLocationV1 {
    fn from_location(location: &WebSearchUserLocation) -> Self {
        Self {
            country: location.country.clone(),
            region: location.region.clone(),
            city: location.city.clone(),
            timezone: location.timezone.clone(),
        }
    }
}

impl StreamEventV1 {
    pub fn into_stream_event(self) -> ProtocolResult<StreamEvent> {
        Ok(match self {
            Self::MessageStart {
                message_id,
                model,
                usage,
            } => StreamEvent::MessageStart {
                message_id,
                model,
                usage: usage.into_usage(),
            },
            Self::ContentBlockStart {
                index,
                content_block,
            } => StreamEvent::ContentBlockStart {
                index,
                content_block: content_block.into_content_block_start(),
            },
            Self::ContentBlockDelta { index, delta } => StreamEvent::ContentBlockDelta {
                index,
                delta: delta.into_content_block_delta(),
            },
            Self::ContentBlockStop { index } => StreamEvent::ContentBlockStop { index },
            Self::MessageDelta { delta } => StreamEvent::MessageDelta {
                delta: delta.into_message_delta_fields(),
            },
            Self::MessageStop => StreamEvent::MessageStop,
            Self::Error {
                error_type,
                message,
            } => StreamEvent::Error {
                error_type,
                message,
            },
        })
    }
}

impl ContentBlockStartV1 {
    fn into_content_block_start(self) -> ContentBlockStart {
        match self {
            Self::Text { text } => ContentBlockStart::Text { text },
            Self::ToolUse { id, name } => ContentBlockStart::ToolUse { id, name },
            Self::Thinking { thinking, data } => ContentBlockStart::Thinking { thinking, data },
            Self::ServerToolUse { id, name, input } => {
                ContentBlockStart::ServerToolUse { id, name, input }
            }
            Self::WebSearchResult {
                tool_use_id,
                content,
            } => ContentBlockStart::WebSearchResult {
                tool_use_id,
                content,
            },
            Self::Compaction { content } => ContentBlockStart::Compaction {
                content,
                // No v1 plugin can mint a server-side compaction blob.
                encrypted_content: None,
            },
            Self::ImageGeneration { id, status } => {
                ContentBlockStart::ImageGeneration { id, status }
            }
        }
    }
}

impl ContentBlockDeltaV1 {
    fn into_content_block_delta(self) -> ContentBlockDelta {
        match self {
            Self::TextDelta { text } => ContentBlockDelta::TextDelta { text },
            Self::InputJsonDelta { partial_json } => {
                ContentBlockDelta::InputJsonDelta { partial_json }
            }
            Self::ThinkingDelta { thinking } => ContentBlockDelta::ThinkingDelta { thinking },
            Self::ThinkingDataDelta { data } => ContentBlockDelta::ThinkingDataDelta { data },
            Self::SignatureDelta { signature } => ContentBlockDelta::SignatureDelta { signature },
            Self::CompactionDelta { content } => ContentBlockDelta::CompactionDelta { content },
            Self::ImageDataDelta {
                b64_json,
                partial_index,
                revised_prompt,
                media_type,
            } => ContentBlockDelta::ImageDataDelta {
                b64_json,
                partial_index,
                revised_prompt,
                media_type,
            },
        }
    }
}

impl MessageDeltaFieldsV1 {
    fn into_message_delta_fields(self) -> MessageDeltaFields {
        MessageDeltaFields {
            stop_reason: self.stop_reason.map(StopReasonV1::into_stop_reason),
            usage: self.usage.into_usage(),
        }
    }
}

impl StopReasonV1 {
    fn into_stop_reason(self) -> StopReason {
        match self {
            Self::EndTurn => StopReason::EndTurn,
            Self::MaxTokens => StopReason::MaxTokens,
            Self::StopSequence => StopReason::StopSequence,
            Self::ToolUse => StopReason::ToolUse,
            Self::PauseTurn => StopReason::PauseTurn,
            Self::Refusal => StopReason::Refusal,
            Self::Compaction => StopReason::Compaction,
            Self::ModelContextWindowExceeded => StopReason::ModelContextWindowExceeded,
            Self::Other(value) => StopReason::Other(value),
        }
    }
}

impl UsageV1 {
    pub fn from_usage(usage: Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            prompt_cache_hit_tokens: usage.prompt_cache_hit_tokens,
            prompt_cache_miss_tokens: usage.prompt_cache_miss_tokens,
            total_input_tokens: usage.total_input_tokens,
            total_output_tokens: usage.total_output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }

    pub fn into_usage(self) -> Usage {
        Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            prompt_cache_hit_tokens: self.prompt_cache_hit_tokens,
            prompt_cache_miss_tokens: self.prompt_cache_miss_tokens,
            total_input_tokens: self.total_input_tokens,
            total_output_tokens: self.total_output_tokens,
            reasoning_tokens: self.reasoning_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Tool, ToolChoice};

    #[test]
    fn computer_use_runtime_capability_round_trips() {
        let capabilities = ModelProviderRuntimeCapabilitiesV1 {
            computer_use: true,
            ..Default::default()
        };
        let value = serde_json::to_value(&capabilities).unwrap();
        assert_eq!(value["computerUse"], true);
        let decoded: ModelProviderRuntimeCapabilitiesV1 = serde_json::from_value(value).unwrap();
        assert!(decoded.computer_use);
    }

    #[test]
    fn provider_connection_config_round_trips_camel_case() {
        let connection = ProviderConnectionConfigV1 {
            base_url: "https://api.deepseek.com".into(),
            api_key: "sk-test".into(),
            headers: BTreeMap::from([("X-Header".to_string(), "v".to_string())]),
            request_options: Some(serde_json::json!({"body": {"maxTokens": 2}})),
            model_request_options: BTreeMap::from([(
                "m".to_string(),
                serde_json::json!({"extraBody": {"thinking": true}}),
            )]),
        };
        let value = serde_json::to_value(&connection).unwrap();
        assert_eq!(value["baseUrl"], "https://api.deepseek.com");
        assert_eq!(value["apiKey"], "sk-test");
        assert_eq!(value["headers"]["X-Header"], "v");
        let decoded: ProviderConnectionConfigV1 = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, connection);
        assert!(!connection.is_empty());
        assert!(ProviderConnectionConfigV1::default().is_empty());
    }

    #[test]
    fn initialize_params_omit_absent_connection() {
        let params = ModelProviderInitializeParamsV1 {
            protocol_version: MODEL_PROVIDER_PROTOCOL_VERSION,
            client_info: ModelProviderClientInfoV1 {
                name: "rebon".into(),
                version: "0.0.0".into(),
            },
            provider_id: None,
            connection: None,
        };
        let value = serde_json::to_value(&params).unwrap();
        assert!(value.get("connection").is_none());
    }

    /// The protocol enums are `deny_unknown_fields`, so a v1 plugin
    /// rejects the whole message over a field it has never seen. A
    /// server-side compaction blob has no v1 field and nothing to say to
    /// an external provider, so it never reaches the wire; Anthropic's
    /// plaintext compaction summary still does.
    #[test]
    fn a_backend_bound_compaction_block_never_reaches_a_plugin() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Compaction(CompactionBlock {
                    content: None,
                    encrypted_content: Some("openai-blob".into()),
                }),
                ContentBlock::Compaction(CompactionBlock {
                    content: Some("a plaintext summary".into()),
                    encrypted_content: None,
                }),
            ],
        };

        let wire = MessageV1::from_message(&message).unwrap();

        assert_eq!(
            wire.content,
            vec![ContentBlockV1::Compaction {
                content: Some("a plaintext summary".into()),
            }]
        );
        let json = serde_json::to_value(&wire).unwrap();
        assert!(
            json.to_string().find("encrypted").is_none(),
            "the blob must not appear on the v1 wire at all: {json}"
        );
    }

    #[test]
    fn extended_capabilities_round_trip_and_default_off() {
        let capabilities = ModelProviderRuntimeCapabilitiesV1 {
            stateful_responses: true,
            remote_compaction: true,
            anchored_minimal: true,
            reasoning_text: true,
            custom_tool_call: true,
            ..Default::default()
        };
        let value = serde_json::to_value(&capabilities).unwrap();
        assert_eq!(value["statefulResponses"], true);
        assert_eq!(value["remoteCompaction"], true);
        assert_eq!(value["anchoredMinimal"], true);
        assert_eq!(value["reasoningText"], true);
        assert_eq!(value["customToolCall"], true);
        let decoded: ModelProviderRuntimeCapabilitiesV1 = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, capabilities);

        let legacy: ModelProviderRuntimeCapabilitiesV1 =
            serde_json::from_value(serde_json::json!({"webSearch": true})).unwrap();
        assert!(legacy.web_search);
        assert!(!legacy.stateful_responses);
        assert!(!legacy.anchored_minimal);
        assert!(!legacy.reasoning_text);
    }

    #[test]
    fn usage_v1_carries_reasoning_tokens_both_ways() {
        let usage = UsageV1 {
            output_tokens: 10,
            reasoning_tokens: 4,
            ..Default::default()
        };
        let internal = usage.into_usage();
        assert_eq!(internal.reasoning_tokens, 4);
        let back = UsageV1::from_usage(internal);
        assert_eq!(back.reasoning_tokens, 4);
    }

    #[test]
    fn rejects_unknown_protocol_version() {
        let err = ensure_protocol_version(2).unwrap_err();
        assert!(err
            .to_string()
            .contains("unsupported model-provider protocol version"));
    }

    #[test]
    fn converts_create_message_request_to_v1_dto() {
        let request = CreateMessageRequest::simple("demo-model", "hello")
            .with_system("system")
            .with_tools(vec![Tool {
                name: "Bash".into(),
                description: "Run shell".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }]);
        let mut request = request;
        request.tool_choice = Some(ToolChoice::Tool {
            name: "Bash".into(),
        });

        let dto = CreateMessageRequestV1::from_request(&request).unwrap();
        assert_eq!(dto.model, "demo-model");
        assert_eq!(dto.messages.len(), 1);
        assert!(matches!(dto.messages[0].role, RoleV1::User));
        assert_eq!(dto.tools[0].name, "Bash");
        assert!(matches!(dto.tool_choice, Some(ToolChoiceV1::Tool { .. })));
        assert!(dto.extensions.is_empty());
    }

    #[test]
    fn prompt_cache_routing_travels_in_extensions() {
        let mut request = CreateMessageRequest::simple("demo-model", "hello");
        request.cache_trace_context = Some(crate::CacheTraceContext {
            prompt_cache_key: Some("rebon-session-abc-def".into()),
            prompt_cache_retention: Some("session".into()),
            ..Default::default()
        });

        let dto = CreateMessageRequestV1::from_request(&request).unwrap();
        let prompt_cache = dto
            .extensions
            .get("promptCache")
            .expect("cache trace context should surface as the promptCache extension");
        assert_eq!(prompt_cache["key"], "rebon-session-abc-def");
        assert_eq!(prompt_cache["retention"], "session");

        // The serialized frame keeps camelCase and omits empty extensions, so
        // plugins that predate the field never see it.
        let value = serde_json::to_value(&dto).unwrap();
        assert_eq!(
            value["extensions"]["promptCache"]["key"],
            "rebon-session-abc-def"
        );
        let plain = CreateMessageRequestV1::from_request(&CreateMessageRequest::simple(
            "demo-model",
            "hello",
        ))
        .unwrap();
        let plain = serde_json::to_value(&plain).unwrap();
        assert!(plain.get("extensions").is_none());
    }

    #[test]
    fn converts_stream_event_v1_to_internal_event() {
        let dto = StreamEventV1::MessageDelta {
            delta: MessageDeltaFieldsV1 {
                stop_reason: Some(StopReasonV1::ToolUse),
                usage: UsageV1 {
                    input_tokens: 7,
                    output_tokens: 3,
                    ..UsageV1::default()
                },
            },
        };
        let event = dto.into_stream_event().unwrap();
        match event {
            StreamEvent::MessageDelta { delta } => {
                assert_eq!(delta.stop_reason, Some(StopReason::ToolUse));
                assert_eq!(delta.usage.input_tokens, 7);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Pins the exact frame shapes emitted by the in-repo
    /// `runtimes/node/plugins/deepseek-responses/provider.mjs` plugin. With
    /// `deny_unknown_fields` on every DTO, a field-name drift between the
    /// plugin and these types is a fatal runtime protocol error — keep this
    /// test and the plugin's selftest in sync when evolving either side.
    #[test]
    fn deepseek_plugin_frame_shapes_deserialize() {
        let events = [
            serde_json::json!({"type":"message_start","message_id":"resp_1","model":"deepseek-v4-flash","usage":{}}),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}),
            serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hi"}}),
            serde_json::json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_9","name":"Bash"}}),
            serde_json::json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}),
            serde_json::json!({"type":"content_block_start","index":3,"content_block":{"type":"server_tool_use","id":"ws_1","name":"web_search","input":{"query":"rust"}}}),
            serde_json::json!({"type":"content_block_stop","index":0}),
            serde_json::json!({"type":"message_delta","delta":{"stopReason":"tool_use","usage":{"inputTokens":10,"outputTokens":20,"promptCacheHitTokens":4,"promptCacheMissTokens":6,"reasoningTokens":7}}}),
            serde_json::json!({"type":"message_stop"}),
            serde_json::json!({"type":"error","error_type":"rate_limit_exceeded","message":"slow down"}),
        ];
        for event in events {
            let parsed: StreamEventV1 = serde_json::from_value(event.clone())
                .unwrap_or_else(|err| panic!("plugin frame should deserialize: {event} — {err}"));
            parsed
                .into_stream_event()
                .unwrap_or_else(|err| panic!("plugin frame should convert: {event} — {err}"));
        }

        let delta: MessageDeltaFieldsV1 = serde_json::from_value(serde_json::json!({
            "stopReason":"tool_use",
            "usage":{"inputTokens":10,"outputTokens":20,"promptCacheHitTokens":4,"promptCacheMissTokens":6,"reasoningTokens":7}
        }))
        .unwrap();
        assert_eq!(delta.stop_reason, Some(StopReasonV1::ToolUse));
        assert_eq!(delta.usage.reasoning_tokens, 7);
        // DeepSeek's input_tokens includes cached tokens, so the plugin uses
        // the hit/miss convention; billed input must stay input_tokens.
        assert_eq!(delta.usage.into_usage().billed_input_tokens(), 10);
    }

    #[test]
    fn rejects_unknown_fields_at_boundary() {
        let raw = serde_json::json!({
            "protocolVersion": 1,
            "streamId": "s1",
            "event": {"type": "message_stop"},
            "unexpected": true
        });
        let err =
            serde_json::from_value::<ModelProviderStreamEventNotificationV1>(raw).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }
}
