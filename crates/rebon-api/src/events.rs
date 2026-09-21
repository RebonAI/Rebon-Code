//! Streaming events + message accumulator.
//!
//! The Anthropic Messages API streams per-block deltas as a sequence
//! of SSE events. `rebon-api` models them unchanged as
//! [`StreamEvent`]s and every real provider (Anthropic, OpenAI-compat)
//! normalises to this enum. The [`MessageAccumulator`] walks a
//! stream and produces a final [`crate::AssistantMessage`], the
//! non-streaming view of a fully streamed response.
//!
//! Reference mapping (Anthropic SDK event type → Rust variant):
//!
//! | SDK `type` | Rust |
//! |---|---|
//! | `message_start` | [`StreamEvent::MessageStart`] |
//! | `content_block_start` | [`StreamEvent::ContentBlockStart`] |
//! | `content_block_delta` | [`StreamEvent::ContentBlockDelta`] |
//! | `content_block_stop` | [`StreamEvent::ContentBlockStop`] |
//! | `message_delta` | [`StreamEvent::MessageDelta`] |
//! | `message_stop` | [`StreamEvent::MessageStop`] |
//! | `ping` | — (silently dropped in the parser) |
//! | `error` | [`StreamEvent::Error`] |

use std::pin::Pin;

use futures_util::Stream;
use serde::{Deserialize, Serialize};

use crate::error::{ModelError, ModelResult};
use crate::types::{
    AssistantMessage, CompactionBlock, ContentBlock, GeneratedImageBlock, SearchResultEntry,
    ServerToolUseBlock, StopReason, TextBlock, ThinkingBlock, ToolUseBlock, Usage,
    WebSearchResultBlock,
};

/// Provider-agnostic streaming event.
///
/// Every variant matches one SDK event type. Fields hold only the
/// data the accumulator / consumer needs — provider-specific extras
/// are stripped by the parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// `message_start` — carries the message id, model, and initial
    /// usage snapshot.
    MessageStart {
        /// Server-assigned message id.
        message_id: String,
        /// Model name the provider actually served.
        model: String,
        /// Initial usage snapshot (typically input tokens).
        usage: Usage,
    },
    /// `content_block_start` — announces a new content block.
    ContentBlockStart {
        /// Block index within the message.
        index: usize,
        /// Starting content block shape. Text/thinking start empty;
        /// tool_use carries id + name (input streams in as deltas).
        content_block: ContentBlockStart,
    },
    /// `content_block_delta` — incremental update to the block at
    /// `index`.
    ContentBlockDelta {
        /// Block index within the message.
        index: usize,
        /// Delta payload.
        delta: ContentBlockDelta,
    },
    /// `content_block_stop` — no more deltas for this block.
    ContentBlockStop {
        /// Block index within the message.
        index: usize,
    },
    /// `message_delta` — updates to the top-level message: final
    /// stop reason + cumulative usage.
    MessageDelta {
        /// Message-level delta fields.
        delta: MessageDeltaFields,
    },
    /// `message_stop` — end of the streaming turn.
    MessageStop,
    /// `error` — provider emitted an error event on the stream.
    Error {
        /// Wire error type (e.g. `overloaded_error`).
        error_type: String,
        /// Human-visible message.
        message: String,
    },
}

/// Shape of a `content_block_start` block payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockStart {
    /// Plain text block (starts empty).
    Text {
        /// Initial text (usually empty).
        #[serde(default)]
        text: String,
    },
    /// Tool use block. `input` arrives later as `input_json_delta`
    /// fragments; this variant only carries the id + name.
    ToolUse {
        /// Tool use id.
        id: String,
        /// Registered tool name.
        name: String,
    },
    /// Extended-thinking block.
    Thinking {
        /// Initial thinking text (usually empty).
        #[serde(default)]
        thinking: String,
        /// Encrypted payload for `redacted_thinking` blocks; `None`
        /// for ordinary extended thinking.
        #[serde(default)]
        data: Option<String>,
    },
    /// Server-side tool use (e.g. web search). Executed by the
    /// provider, not dispatched client-side.
    ServerToolUse {
        /// Server-assigned tool use id.
        id: String,
        /// Server tool name (e.g. `"web_search"`).
        name: String,
        /// Input for the server tool (streams in via deltas, or
        /// arrives fully formed).
        #[serde(default)]
        input: serde_json::Value,
    },
    /// Web search result block. Carries structured search results
    /// from a server-side web search.
    WebSearchResult {
        /// Matches the originating server tool use id.
        tool_use_id: String,
        /// Raw content from the provider.
        #[serde(default)]
        content: serde_json::Value,
    },
    /// Server-side compaction block. Anthropic fills `content` with a
    /// plaintext summary; the OpenAI Responses `compaction` output item
    /// fills `encrypted_content` with an opaque blob instead.
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    /// OpenAI Responses `image_generation_call` output block.
    /// Bytes arrive later as [`ContentBlockDelta::ImageDataDelta`]
    /// events — this variant only announces the id + initial status.
    ImageGeneration {
        /// Server-assigned id (e.g. `"ig_…"`).
        id: String,
        /// Initial status reported by the provider
        /// (`"in_progress"` / `"generating"` / …). May be absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
}

/// Shape of a `content_block_delta` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockDelta {
    /// Incremental text.
    TextDelta {
        /// Text fragment to append.
        text: String,
    },
    /// Incremental JSON for a tool-use input. The accumulator
    /// concatenates every `partial_json` and parses the result at
    /// block-stop time.
    InputJsonDelta {
        /// JSON fragment to append to the tool's raw input string.
        partial_json: String,
    },
    /// Incremental thinking text.
    ThinkingDelta {
        /// Thinking fragment to append.
        thinking: String,
    },
    /// Encrypted thinking payload carried by providers that require
    /// opaque reasoning round-trip on replay.
    ThinkingDataDelta {
        /// Encrypted payload to preserve in [`ThinkingBlock::data`].
        data: String,
    },
    /// Thinking signature (only for redacted thinking).
    SignatureDelta {
        /// Full signature string.
        signature: String,
    },
    /// Complete Anthropic compaction summary content.
    CompactionDelta { content: String },
    /// Base64-encoded image-generation frame.
    ///
    /// For `image_generation_call.partial_image` events the
    /// `partial_index` is `Some(n)` and the `b64_json` is a **full
    /// preview frame**, not a fragment. For the final image the
    /// `partial_index` is `None` and the `b64_json` is the complete
    /// result. The accumulator always overwrites — partials are not
    /// concatenated.
    ImageDataDelta {
        /// Complete base64 payload for this frame.
        b64_json: String,
        /// `None` for the final image; `Some(n)` where `n` is the
        /// 0-based partial index otherwise.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial_index: Option<u32>,
        /// Model-revised prompt. Only present on the final image.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revised_prompt: Option<String>,
        /// MIME type of the bytes (e.g. `"image/png"`). Only present
        /// on the final image.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
    },
}

/// Top-level `message_delta` fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDeltaFields {
    /// Final stop reason. May be `None` until `message_stop`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// Cumulative usage at stream end.
    #[serde(default)]
    pub usage: Usage,
}

/// Boxed stream alias used by [`crate::ModelClient`] implementations.
pub type StreamEventStream = Pin<Box<dyn Stream<Item = ModelResult<StreamEvent>> + Send>>;

/// Walks a stream of [`StreamEvent`]s and builds a final
/// [`AssistantMessage`].
///
/// Pulled into a reusable struct so both the default
/// `ModelClient::create_message` path and the engine-side consumer
/// can share the logic.
#[derive(Debug, Default)]
pub struct MessageAccumulator {
    message_id: Option<String>,
    model: Option<String>,
    blocks: Vec<PartialBlock>,
    stop_reason: Option<StopReason>,
    usage: Usage,
    finished: bool,
}

#[derive(Debug, Clone)]
enum PartialBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
        data: Option<String>,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        input_json: String,
    },
    WebSearchResult {
        tool_use_id: String,
        content: serde_json::Value,
    },
    Compaction {
        content: Option<String>,
        encrypted_content: Option<String>,
    },
    GeneratedImage {
        id: String,
        status: Option<String>,
        revised_prompt: Option<String>,
        media_type: String,
        data: String,
    },
}

impl PartialBlock {
    fn into_content(self) -> ContentBlock {
        match self {
            Self::Text(text) => ContentBlock::Text(TextBlock { text }),
            Self::ToolUse {
                id,
                name,
                input_json,
            } => {
                // `input_json` is the concatenated partial_json. If it
                // parses, carry the parsed value; otherwise carry a
                // string so callers can inspect the raw input the
                // model sent.
                let input: serde_json::Value = if input_json.trim().is_empty() {
                    serde_json::Value::Object(Default::default())
                } else {
                    serde_json::from_str(&input_json)
                        .unwrap_or_else(|_| serde_json::Value::String(input_json.clone()))
                };
                ContentBlock::ToolUse(ToolUseBlock { id, name, input })
            }
            Self::Thinking {
                thinking,
                signature,
                data,
            } => ContentBlock::Thinking(ThinkingBlock {
                thinking,
                signature,
                data,
            }),
            Self::ServerToolUse {
                id,
                name,
                mut input,
                input_json,
            } => {
                // If input arrived via deltas, parse the accumulated
                // JSON; otherwise keep the value set at block start.
                if !input_json.is_empty() {
                    input = serde_json::from_str(&input_json)
                        .unwrap_or_else(|_| serde_json::Value::String(input_json));
                }
                ContentBlock::ServerToolUse(ServerToolUseBlock { id, name, input })
            }
            Self::WebSearchResult {
                tool_use_id,
                content,
            } => {
                // Parse structured search results from the raw content.
                let results = extract_search_results(&content);
                ContentBlock::WebSearchResult(WebSearchResultBlock {
                    tool_use_id,
                    results,
                    raw_content: Some(content),
                })
            }
            Self::Compaction {
                content,
                encrypted_content,
            } => ContentBlock::Compaction(CompactionBlock {
                content,
                encrypted_content,
            }),
            Self::GeneratedImage {
                id,
                status,
                revised_prompt,
                media_type,
                data,
            } => ContentBlock::GeneratedImage(GeneratedImageBlock {
                id,
                status,
                revised_prompt,
                media_type,
                data,
                saved_path: None,
            }),
        }
    }
}

/// Extract structured [`SearchResultEntry`] items from raw provider
/// content.
///
/// Anthropic sends `[{ type: "web_search_result", url, title,
/// page_age, ... }, ...]`. We pull out the `title` + `url` +
/// optional snippet from each entry.
fn extract_search_results(content: &serde_json::Value) -> Vec<SearchResultEntry> {
    let Some(arr) = content.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let title = item.get("title")?.as_str()?.to_string();
            let url = item.get("url")?.as_str()?.to_string();
            let snippet = item
                .get("encrypted_content")
                .or_else(|| item.get("snippet"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(SearchResultEntry {
                title,
                url,
                snippet,
            })
        })
        .collect()
}

impl MessageAccumulator {
    /// Create a fresh accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the accumulator has seen a `message_stop`.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Apply a single event. Events out of order (e.g. a delta
    /// arriving before the corresponding start) are logged and
    /// dropped instead of panicking — real streams are almost always
    /// well-formed, but the state-machine layer should degrade
    /// gracefully on forward-compatible providers.
    pub fn apply(&mut self, event: &StreamEvent) -> ModelResult<()> {
        match event {
            StreamEvent::MessageStart {
                message_id,
                model,
                usage,
            } => {
                self.message_id = Some(message_id.clone());
                self.model = Some(model.clone());
                self.usage = *usage;
            }
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let block = match content_block {
                    ContentBlockStart::Text { text } => PartialBlock::Text(text.clone()),
                    ContentBlockStart::ToolUse { id, name } => PartialBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input_json: String::new(),
                    },
                    ContentBlockStart::Thinking { thinking, data } => PartialBlock::Thinking {
                        thinking: thinking.clone(),
                        signature: None,
                        data: data.clone(),
                    },
                    ContentBlockStart::ServerToolUse { id, name, input } => {
                        PartialBlock::ServerToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                            input_json: String::new(),
                        }
                    }
                    ContentBlockStart::WebSearchResult {
                        tool_use_id,
                        content,
                    } => PartialBlock::WebSearchResult {
                        tool_use_id: tool_use_id.clone(),
                        content: content.clone(),
                    },
                    ContentBlockStart::Compaction {
                        content,
                        encrypted_content,
                    } => PartialBlock::Compaction {
                        content: content.clone(),
                        encrypted_content: encrypted_content.clone(),
                    },
                    ContentBlockStart::ImageGeneration { id, status } => {
                        PartialBlock::GeneratedImage {
                            id: id.clone(),
                            status: status.clone(),
                            revised_prompt: None,
                            media_type: "image/png".to_string(),
                            data: String::new(),
                        }
                    }
                };
                if *index == self.blocks.len() {
                    self.blocks.push(block);
                } else if *index < self.blocks.len() {
                    self.blocks[*index] = block;
                } else {
                    // Pad any missing slots with empty text blocks so
                    // out-of-order indices are still placed correctly.
                    while self.blocks.len() < *index {
                        self.blocks.push(PartialBlock::Text(String::new()));
                    }
                    self.blocks.push(block);
                }
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(block) = self.blocks.get_mut(*index) {
                    match (block, delta) {
                        (PartialBlock::Text(buf), ContentBlockDelta::TextDelta { text }) => {
                            buf.push_str(text);
                        }
                        (
                            PartialBlock::ToolUse { input_json, .. },
                            ContentBlockDelta::InputJsonDelta { partial_json },
                        ) => {
                            input_json.push_str(partial_json);
                        }
                        (
                            PartialBlock::ServerToolUse { input_json, .. },
                            ContentBlockDelta::InputJsonDelta { partial_json },
                        ) => {
                            input_json.push_str(partial_json);
                        }
                        (
                            PartialBlock::Thinking { thinking, .. },
                            ContentBlockDelta::ThinkingDelta { thinking: fragment },
                        ) => {
                            thinking.push_str(fragment);
                        }
                        (
                            PartialBlock::Thinking { data, .. },
                            ContentBlockDelta::ThinkingDataDelta { data: encrypted },
                        ) => {
                            *data = Some(encrypted.clone());
                        }
                        (
                            PartialBlock::Thinking { signature, .. },
                            ContentBlockDelta::SignatureDelta { signature: sig },
                        ) => {
                            *signature = Some(sig.clone());
                        }
                        (
                            PartialBlock::Compaction { content, .. },
                            ContentBlockDelta::CompactionDelta { content: fragment },
                        ) => match content {
                            Some(buf) => buf.push_str(fragment),
                            None => *content = Some(fragment.clone()),
                        },
                        (
                            PartialBlock::GeneratedImage {
                                data,
                                revised_prompt,
                                media_type,
                                ..
                            },
                            ContentBlockDelta::ImageDataDelta {
                                b64_json,
                                partial_index: _,
                                revised_prompt: rp,
                                media_type: mt,
                            },
                        ) => {
                            // Each frame replaces the prior one — partials
                            // are full preview images, not fragments.
                            *data = b64_json.clone();
                            if let Some(p) = rp.as_deref() {
                                *revised_prompt = Some(p.to_string());
                            }
                            if let Some(m) = mt.as_deref() {
                                *media_type = m.to_string();
                            }
                        }
                        _ => {
                            tracing::debug!(
                                index = *index,
                                "mismatched content_block_delta; dropping"
                            );
                        }
                    }
                } else {
                    tracing::debug!(
                        index = *index,
                        "content_block_delta for unknown block; dropping"
                    );
                }
            }
            StreamEvent::ContentBlockStop { .. } => {
                // Block is kept open until `finish()` lowers it.
                // Nothing to do here — the delta stream is naturally
                // append-only and the block vector is already sized.
            }
            StreamEvent::MessageDelta { delta } => {
                if let Some(reason) = &delta.stop_reason {
                    self.stop_reason = Some(reason.clone());
                }
                self.usage.merge(&delta.usage);
            }
            StreamEvent::MessageStop => {
                self.finished = true;
            }
            StreamEvent::Error {
                error_type,
                message,
            } => {
                return Err(ModelError::Permanent(format!("{error_type}: {message}")));
            }
        }
        Ok(())
    }

    /// Consume the accumulator and produce the final assistant
    /// message.
    ///
    /// Even if `message_stop` was not observed (provider cut the
    /// stream early), the call still produces a message with
    /// whatever blocks were accumulated so far.
    pub fn finish(self) -> AssistantMessage {
        AssistantMessage {
            id: self.message_id.unwrap_or_default(),
            model: self.model.unwrap_or_default(),
            content: self
                .blocks
                .into_iter()
                .map(PartialBlock::into_content)
                .collect(),
            stop_reason: self.stop_reason,
            usage: self.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Usage;

    fn msg_start(id: &str, model: &str) -> StreamEvent {
        StreamEvent::MessageStart {
            message_id: id.into(),
            model: model.into(),
            usage: Usage {
                input_tokens: 5,
                ..Default::default()
            },
        }
    }

    fn text_start(index: usize) -> StreamEvent {
        StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        }
    }

    fn text_delta(index: usize, text: &str) -> StreamEvent {
        StreamEvent::ContentBlockDelta {
            index,
            delta: ContentBlockDelta::TextDelta { text: text.into() },
        }
    }

    fn tool_start(index: usize, id: &str, name: &str) -> StreamEvent {
        StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::ToolUse {
                id: id.into(),
                name: name.into(),
            },
        }
    }

    fn tool_json_delta(index: usize, fragment: &str) -> StreamEvent {
        StreamEvent::ContentBlockDelta {
            index,
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: fragment.into(),
            },
        }
    }

    fn block_stop(index: usize) -> StreamEvent {
        StreamEvent::ContentBlockStop { index }
    }

    fn msg_delta(stop: StopReason, output_tokens: u32) -> StreamEvent {
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(stop),
                usage: Usage {
                    output_tokens,
                    ..Default::default()
                },
            },
        }
    }

    fn msg_stop() -> StreamEvent {
        StreamEvent::MessageStop
    }

    #[test]
    fn accumulator_builds_simple_text_message() {
        let events = vec![
            msg_start("msg_1", "claude-sonnet-4-6"),
            text_start(0),
            text_delta(0, "Hello"),
            text_delta(0, ", world"),
            block_stop(0),
            msg_delta(StopReason::EndTurn, 7),
            msg_stop(),
        ];
        let mut acc = MessageAccumulator::new();
        for e in &events {
            acc.apply(e).unwrap();
        }
        assert!(acc.is_finished());
        let msg = acc.finish();
        assert_eq!(msg.id, "msg_1");
        assert_eq!(msg.model, "claude-sonnet-4-6");
        assert_eq!(msg.text(), "Hello, world");
        assert_eq!(msg.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(msg.usage.input_tokens, 5);
        assert_eq!(msg.usage.output_tokens, 7);
    }

    #[test]
    fn accumulator_parses_tool_use_input_json() {
        let events = vec![
            msg_start("msg_2", "claude-sonnet-4-6"),
            text_start(0),
            text_delta(0, "I'll read that file."),
            block_stop(0),
            tool_start(1, "toolu_1", "Read"),
            tool_json_delta(1, "{\"path\":"),
            tool_json_delta(1, "\"foo.rs\"}"),
            block_stop(1),
            msg_delta(StopReason::ToolUse, 12),
            msg_stop(),
        ];
        let mut acc = MessageAccumulator::new();
        for e in &events {
            acc.apply(e).unwrap();
        }
        let msg = acc.finish();
        assert_eq!(msg.stop_reason, Some(StopReason::ToolUse));
        assert!(msg.has_tool_use());
        let tu = msg.tool_uses().next().unwrap();
        assert_eq!(tu.id, "toolu_1");
        assert_eq!(tu.name, "Read");
        assert_eq!(tu.input, serde_json::json!({ "path": "foo.rs" }));
    }

    #[test]
    fn accumulator_keeps_malformed_tool_json_as_string() {
        let events = vec![
            msg_start("msg_3", "claude-sonnet-4-6"),
            tool_start(0, "toolu_1", "Glob"),
            tool_json_delta(0, "{\"pat"),
            block_stop(0),
            msg_delta(StopReason::ToolUse, 1),
            msg_stop(),
        ];
        let mut acc = MessageAccumulator::new();
        for e in &events {
            acc.apply(e).unwrap();
        }
        let msg = acc.finish();
        let tu = msg.tool_uses().next().unwrap();
        // Malformed JSON falls through to a string so callers can
        // inspect the partial fragment.
        assert_eq!(tu.input, serde_json::Value::String("{\"pat".into()));
    }

    #[test]
    fn accumulator_error_event_surfaces_as_permanent() {
        let mut acc = MessageAccumulator::new();
        let err = acc
            .apply(&StreamEvent::Error {
                error_type: "overloaded_error".into(),
                message: "try later".into(),
            })
            .unwrap_err();
        assert!(matches!(err, crate::error::ModelError::Permanent(_)));
    }

    #[test]
    fn accumulator_pads_out_of_order_block_index() {
        let events = vec![
            msg_start("msg_4", "m"),
            // Skip index 0 — start directly at 2. The accumulator
            // should pad index 0 and 1 with empty text blocks.
            StreamEvent::ContentBlockStart {
                index: 2,
                content_block: ContentBlockStart::Text {
                    text: "late".into(),
                },
            },
            block_stop(2),
            msg_stop(),
        ];
        let mut acc = MessageAccumulator::new();
        for e in &events {
            acc.apply(e).unwrap();
        }
        let msg = acc.finish();
        assert_eq!(msg.content.len(), 3);
        assert_eq!(msg.content[0].as_text(), Some(""));
        assert_eq!(msg.content[1].as_text(), Some(""));
        assert_eq!(msg.content[2].as_text(), Some("late"));
    }

    #[test]
    fn accumulator_merges_initial_and_final_usage_snapshots() {
        let events = vec![
            msg_start("m", "model"),
            text_start(0),
            text_delta(0, "hi"),
            block_stop(0),
            msg_delta(StopReason::EndTurn, 10),
            msg_stop(),
        ];
        let mut acc = MessageAccumulator::new();
        for e in &events {
            acc.apply(e).unwrap();
        }
        let msg = acc.finish();
        assert_eq!(msg.usage.input_tokens, 5);
        assert_eq!(msg.usage.output_tokens, 10);
    }

    #[test]
    fn stream_event_serializes_with_type_discriminator() {
        let e = msg_delta(StopReason::ToolUse, 42);
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"message_delta\""));
        assert!(json.contains("\"stop_reason\":\"tool_use\""));
    }

    // -- Server tool use / web search accumulator tests -----------

    fn server_tool_start(
        index: usize,
        id: &str,
        name: &str,
        input: serde_json::Value,
    ) -> StreamEvent {
        StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::ServerToolUse {
                id: id.into(),
                name: name.into(),
                input,
            },
        }
    }

    fn web_search_result_start(
        index: usize,
        tool_use_id: &str,
        content: serde_json::Value,
    ) -> StreamEvent {
        StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::WebSearchResult {
                tool_use_id: tool_use_id.into(),
                content,
            },
        }
    }

    #[test]
    fn accumulator_builds_server_tool_use_block() {
        let events = vec![
            msg_start("msg_ws", "claude-sonnet-4-6"),
            text_start(0),
            text_delta(0, "Let me search."),
            block_stop(0),
            server_tool_start(
                1,
                "srv_1",
                "web_search",
                serde_json::json!({"query": "rust async"}),
            ),
            block_stop(1),
            msg_delta(StopReason::EndTurn, 10),
            msg_stop(),
        ];
        let mut acc = MessageAccumulator::new();
        for e in &events {
            acc.apply(e).unwrap();
        }
        let msg = acc.finish();
        assert_eq!(msg.content.len(), 2);
        assert_eq!(msg.content[0].as_text(), Some("Let me search."));
        match &msg.content[1] {
            crate::types::ContentBlock::ServerToolUse(stu) => {
                assert_eq!(stu.id, "srv_1");
                assert_eq!(stu.name, "web_search");
                assert_eq!(stu.input, serde_json::json!({"query": "rust async"}));
            }
            other => panic!("expected ServerToolUse, got {other:?}"),
        }
        // Server tool use must NOT register as tool_use
        assert!(!msg.has_tool_use());
        assert_eq!(msg.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn accumulator_builds_web_search_result_block() {
        let results_content = serde_json::json!([
            {
                "type": "web_search_result",
                "title": "Async Rust Guide",
                "url": "https://example.com/async",
                "page_age": "2 days ago"
            },
            {
                "type": "web_search_result",
                "title": "Tokio Runtime",
                "url": "https://tokio.rs"
            }
        ]);
        let events = vec![
            msg_start("msg_wsr", "claude-sonnet-4-6"),
            server_tool_start(
                0,
                "srv_1",
                "web_search_20250305",
                serde_json::json!({"query": "async rust"}),
            ),
            block_stop(0),
            web_search_result_start(1, "srv_1", results_content),
            block_stop(1),
            text_start(2),
            text_delta(2, "Here are the results."),
            block_stop(2),
            msg_delta(StopReason::EndTurn, 20),
            msg_stop(),
        ];
        let mut acc = MessageAccumulator::new();
        for e in &events {
            acc.apply(e).unwrap();
        }
        let msg = acc.finish();
        assert_eq!(msg.content.len(), 3);
        match &msg.content[1] {
            crate::types::ContentBlock::WebSearchResult(wsr) => {
                assert_eq!(wsr.tool_use_id, "srv_1");
                assert_eq!(wsr.results.len(), 2);
                assert_eq!(wsr.results[0].title, "Async Rust Guide");
                assert_eq!(wsr.results[0].url, "https://example.com/async");
                assert_eq!(wsr.results[1].title, "Tokio Runtime");
                assert_eq!(wsr.results[1].url, "https://tokio.rs");
                assert!(wsr.raw_content.is_some());
            }
            other => panic!("expected WebSearchResult, got {other:?}"),
        }
        assert_eq!(msg.content[2].as_text(), Some("Here are the results."));
    }

    #[test]
    fn accumulator_handles_server_tool_use_with_input_json_deltas() {
        // Server tool uses can receive input via JSON deltas
        // (same mechanism as client tool use).
        let events = vec![
            msg_start("msg_delta_stu", "m"),
            server_tool_start(0, "srv_2", "web_search", serde_json::json!({})),
            // Input arrives via delta
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::InputJsonDelta {
                    partial_json: "{\"query\":".into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::InputJsonDelta {
                    partial_json: "\"hello world\"}".into(),
                },
            },
            block_stop(0),
            msg_delta(StopReason::EndTurn, 5),
            msg_stop(),
        ];
        let mut acc = MessageAccumulator::new();
        for e in &events {
            acc.apply(e).unwrap();
        }
        let msg = acc.finish();
        match &msg.content[0] {
            crate::types::ContentBlock::ServerToolUse(stu) => {
                assert_eq!(stu.input, serde_json::json!({"query": "hello world"}));
            }
            other => panic!("expected ServerToolUse, got {other:?}"),
        }
    }

    #[test]
    fn extract_search_results_parses_anthropic_format() {
        let content = serde_json::json!([
            {
                "type": "web_search_result",
                "title": "Result 1",
                "url": "https://one.example.com",
                "snippet": "A snippet"
            },
            {
                "type": "web_search_result",
                "title": "Result 2",
                "url": "https://two.example.com"
            },
            {
                "type": "text",
                "text": "no title or url"
            }
        ]);
        let results = super::extract_search_results(&content);
        // Only entries with both title and url are extracted
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Result 1");
        assert_eq!(results[0].url, "https://one.example.com");
        assert_eq!(results[0].snippet, Some("A snippet".into()));
        assert_eq!(results[1].title, "Result 2");
        assert!(results[1].snippet.is_none());
    }

    #[test]
    fn extract_search_results_returns_empty_for_non_array() {
        assert!(super::extract_search_results(&serde_json::json!("not an array")).is_empty());
        assert!(super::extract_search_results(&serde_json::json!(null)).is_empty());
        assert!(super::extract_search_results(&serde_json::json!(42)).is_empty());
    }

    // -- Image generation accumulator tests ----------------------

    fn img_start(index: usize, id: &str, status: Option<&str>) -> StreamEvent {
        StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::ImageGeneration {
                id: id.into(),
                status: status.map(String::from),
            },
        }
    }

    fn img_delta(
        index: usize,
        b64: &str,
        partial_index: Option<u32>,
        revised: Option<&str>,
        media: Option<&str>,
    ) -> StreamEvent {
        StreamEvent::ContentBlockDelta {
            index,
            delta: ContentBlockDelta::ImageDataDelta {
                b64_json: b64.into(),
                partial_index,
                revised_prompt: revised.map(String::from),
                media_type: media.map(String::from),
            },
        }
    }

    #[test]
    fn accumulator_builds_generated_image_block_from_final_delta() {
        let events = vec![
            msg_start("msg_ig1", "gpt-5.4"),
            img_start(0, "ig_abc", Some("in_progress")),
            img_delta(0, "ZmluYWw=", None, Some("a red fox"), Some("image/png")),
            block_stop(0),
            msg_delta(StopReason::EndTurn, 0),
            msg_stop(),
        ];
        let mut acc = MessageAccumulator::new();
        for e in &events {
            acc.apply(e).unwrap();
        }
        let msg = acc.finish();
        assert_eq!(msg.content.len(), 1);
        match &msg.content[0] {
            crate::types::ContentBlock::GeneratedImage(gi) => {
                assert_eq!(gi.id, "ig_abc");
                assert_eq!(gi.status.as_deref(), Some("in_progress"));
                assert_eq!(gi.revised_prompt.as_deref(), Some("a red fox"));
                assert_eq!(gi.media_type, "image/png");
                assert_eq!(gi.data, "ZmluYWw=");
                assert!(gi.saved_path.is_none());
            }
            other => panic!("expected GeneratedImage, got {other:?}"),
        }
        // image_generation must NOT register as tool_use
        assert!(!msg.has_tool_use());
    }

    #[test]
    fn accumulator_partial_frames_replace_not_concat() {
        // Emulate `response.image_generation_call.partial_image`
        // events — each frame is a complete preview image.
        let events = vec![
            msg_start("msg_ig2", "gpt-5.4"),
            img_start(0, "ig_xyz", None),
            img_delta(0, "cGFydGlhbC0x", Some(0), None, None),
            img_delta(0, "cGFydGlhbC0y", Some(1), None, None),
            img_delta(0, "ZmluYWw=", None, Some("a cat"), Some("image/webp")),
            block_stop(0),
            msg_delta(StopReason::EndTurn, 0),
            msg_stop(),
        ];
        let mut acc = MessageAccumulator::new();
        for e in &events {
            acc.apply(e).unwrap();
        }
        let msg = acc.finish();
        match &msg.content[0] {
            crate::types::ContentBlock::GeneratedImage(gi) => {
                // The last frame wins — partials don't accumulate.
                assert_eq!(gi.data, "ZmluYWw=");
                assert_eq!(gi.media_type, "image/webp");
                assert_eq!(gi.revised_prompt.as_deref(), Some("a cat"));
            }
            other => panic!("expected GeneratedImage, got {other:?}"),
        }
    }

    #[test]
    fn accumulator_generated_image_without_final_metadata_keeps_defaults() {
        // If only partial frames arrived and no final delta, media_type
        // should default to image/png and revised_prompt stays None.
        let events = vec![
            msg_start("msg_ig3", "gpt-5.4"),
            img_start(0, "ig_q", None),
            img_delta(0, "cHJldmlldw==", Some(0), None, None),
            block_stop(0),
            msg_stop(),
        ];
        let mut acc = MessageAccumulator::new();
        for e in &events {
            acc.apply(e).unwrap();
        }
        let msg = acc.finish();
        match &msg.content[0] {
            crate::types::ContentBlock::GeneratedImage(gi) => {
                assert_eq!(gi.data, "cHJldmlldw==");
                assert_eq!(gi.media_type, "image/png");
                assert!(gi.revised_prompt.is_none());
            }
            other => panic!("expected GeneratedImage, got {other:?}"),
        }
    }

    #[test]
    fn stream_event_serializes_image_generation_start_and_delta() {
        let start = img_start(0, "ig_ser", Some("generating"));
        let start_json = serde_json::to_string(&start).unwrap();
        assert!(start_json.contains("\"content_block\""));
        assert!(start_json.contains("\"type\":\"image_generation\""));
        assert!(start_json.contains("\"id\":\"ig_ser\""));
        assert!(start_json.contains("\"status\":\"generating\""));

        let delta = img_delta(0, "YWFh", Some(2), None, None);
        let delta_json = serde_json::to_string(&delta).unwrap();
        assert!(delta_json.contains("\"type\":\"image_data_delta\""));
        assert!(delta_json.contains("\"b64_json\":\"YWFh\""));
        assert!(delta_json.contains("\"partial_index\":2"));
    }
}
