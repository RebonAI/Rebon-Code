use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::request::CreateMessageRequest;
use crate::types::{Message, Tool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMissReason {
    None,
    Unknown,
    SystemChanged,
    ToolsChanged,
    ModelChanged,
    ReasoningChanged,
    DynamicContextChanged,
    CompactReplacedMessages,
    HardContextGuardTruncated,
    OverflowInvalidatedPreviousResponseId,
    RetryWithoutPreviousResponseId,
    PreviousResponseNotFound,
    ProviderFingerprintChanged,
    BaselineMissing,
    BaselineMismatch,
    PreviousResponseIdMissing,
}

impl CacheMissReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Unknown => "unknown",
            Self::SystemChanged => "system_changed",
            Self::ToolsChanged => "tools_changed",
            Self::ModelChanged => "model_changed",
            Self::ReasoningChanged => "reasoning_changed",
            Self::DynamicContextChanged => "dynamic_context_changed",
            Self::CompactReplacedMessages => "compact_replaced_messages",
            Self::HardContextGuardTruncated => "hard_context_guard_truncated",
            Self::OverflowInvalidatedPreviousResponseId => {
                "overflow_invalidated_previous_response_id"
            }
            Self::RetryWithoutPreviousResponseId => "retry_without_previous_response_id",
            Self::PreviousResponseNotFound => "previous_response_not_found",
            Self::ProviderFingerprintChanged => "provider_fingerprint_changed",
            Self::BaselineMissing => "baseline_missing",
            Self::BaselineMismatch => "baseline_mismatch",
            Self::PreviousResponseIdMissing => "previous_response_id_missing",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestShapeTrace {
    pub model_hash: String,
    pub system_hash: String,
    pub tools_hash: String,
    pub messages_prefix_hash: String,
    pub reasoning_hash: String,
    pub dynamic_context_hash: Option<String>,
}

impl RequestShapeTrace {
    pub fn from_request(request: &CreateMessageRequest) -> Self {
        let effective_system = request
            .system
            .as_deref()
            .unwrap_or("You are a helpful assistant.");
        let effective_messages = request.messages_with_transient_context();
        Self {
            model_hash: stable_hash_str(&request.model),
            system_hash: stable_hash_str(effective_system),
            tools_hash: stable_hash_value(&tools_shape_value(&request.tools)),
            messages_prefix_hash: stable_hash_value(&messages_prefix_shape_value(
                &effective_messages,
            )),
            reasoning_hash: stable_hash_str(&format!(
                "thinking={:?};reasoning_effort={:?};reasoning_summary={:?}",
                request.thinking, request.reasoning_effort, request.reasoning_summary
            )),
            dynamic_context_hash: request
                .transient_context
                .as_deref()
                .filter(|context| !context.is_empty())
                .map(stable_hash_str),
        }
    }
}

pub fn cache_trace_enabled() -> bool {
    std::env::var("REBON_CACHE_TRACE").is_ok_and(|value| value == "1")
}

pub fn stable_hash_value(value: &Value) -> String {
    stable_hash_str(&serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}")))
}

pub fn stable_hash_str(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn tools_shape_value(tools: &[Tool]) -> Value {
    Value::Array(tools.iter().map(tool_shape_value).collect())
}

pub fn tools_hash(tools: &[Tool]) -> String {
    stable_hash_value(&tools_shape_value(tools))
}

pub fn schema_hash(tools: &[Tool]) -> String {
    let schemas = Value::Array(
        tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "input_schema": tool.input_schema,
                })
            })
            .collect(),
    );
    stable_hash_value(&schemas)
}

fn tool_shape_value(tool: &Tool) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    })
}

fn messages_prefix_shape_value(messages: &[Message]) -> Value {
    const PREFIX_MESSAGES: usize = 12;
    json!({
        "total_messages": messages.len(),
        "prefix": messages.iter().take(PREFIX_MESSAGES).collect::<Vec<_>>(),
        "truncated": messages.len() > PREFIX_MESSAGES,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentBlock, Role, TextBlock};

    fn request_with(system: Option<&str>, text: &str) -> CreateMessageRequest {
        CreateMessageRequest {
            model: "model-a".into(),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text(TextBlock { text: text.into() })],
            }],
            system: system.map(str::to_string),
            transient_context: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
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

    #[test]
    fn cache_miss_reason_strings_include_context_boundaries() {
        assert_eq!(
            CacheMissReason::CompactReplacedMessages.as_str(),
            "compact_replaced_messages"
        );
        assert_eq!(
            CacheMissReason::HardContextGuardTruncated.as_str(),
            "hard_context_guard_truncated"
        );
    }

    #[test]
    fn stable_hash_str_uses_sha256_hex() {
        assert_eq!(
            stable_hash_str("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn request_shape_trace_is_stable_for_same_shape() {
        let a = RequestShapeTrace::from_request(&request_with(Some("system"), "hello"));
        let b = RequestShapeTrace::from_request(&request_with(Some("system"), "hello"));

        assert_eq!(a, b);
    }

    #[test]
    fn request_shape_trace_changes_when_system_changes() {
        let a = RequestShapeTrace::from_request(&request_with(Some("system-a"), "hello"));
        let b = RequestShapeTrace::from_request(&request_with(Some("system-b"), "hello"));

        assert_ne!(a.system_hash, b.system_hash);
    }
}
