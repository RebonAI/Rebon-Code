//! Wire types for the Anthropic Messages API.
//!
//! These are the canonical shapes both providers normalise to. The
//! OpenAI path translates these into chat-completions requests on
//! write and back into these on read, so downstream consumers only
//! ever see these types.

use serde::{Deserialize, Serialize};
use std::borrow::Borrow;

pub use rebon_types::Usage;

/// Conversation role.
///
/// Matches the Anthropic Messages API `role` discriminant. `System`
/// is not a real message role — it's carried separately on
/// [`crate::CreateMessageRequest::system`] — but we include it here
/// as a convenience for callers building transcripts from mixed
/// history shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// User-authored message.
    User,
    /// Assistant-authored message.
    Assistant,
    /// System instruction. Sent as the dedicated `system` field on
    /// the outbound request, not as a message.
    System,
}

/// A single chat message in the request / response history.
///
/// Matches the Anthropic `{role, content}` shape. `content` is
/// always represented as a block list on the Rust side — both
/// providers accept plain-string `content` too, but normalising to
/// blocks makes the accumulator and tool-use round-trip logic
/// simpler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Role of the speaker.
    pub role: Role,
    /// Ordered list of content blocks.
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Convenience constructor for a single-text user message.
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text(TextBlock { text: text.into() })],
        }
    }

    /// Convenience constructor for a single-text assistant message.
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text(TextBlock { text: text.into() })],
        }
    }
}

/// Wrap text in the `<system-reminder>…</system-reminder>` envelope and
/// return the user-role, meta [`Message`] every attachment producer sends
/// it in. Five of them built this by hand, identically.
pub fn make_meta_user_message(body: &str) -> Message {
    let wrapped = format!("<system-reminder>\n{body}\n</system-reminder>");
    Message {
        role: Role::User,
        content: vec![ContentBlock::Text(TextBlock { text: wrapped })],
    }
}

/// Content block union.
///
/// Matches the Anthropic `ContentBlock` discriminated union.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text block.
    Text(TextBlock),
    /// User-supplied image block.
    Image(ImageBlock),
    /// Tool use requested by the assistant.
    ToolUse(ToolUseBlock),
    /// Tool result being sent back to the assistant. Only valid on
    /// user-role messages.
    ToolResult(ToolResultBlock),
    /// Extended-thinking block (`thinking`/`redacted_thinking`).
    Thinking(ThinkingBlock),
    /// Server-side tool use (e.g. web search). Executed by the API
    /// provider, NOT dispatched client-side. The engine's tool loop
    /// must skip these blocks.
    ServerToolUse(ServerToolUseBlock),
    /// Result from a server-side tool (e.g. web search results).
    /// Paired with a preceding [`ServerToolUse`] block.
    WebSearchResult(WebSearchResultBlock),
    /// Anthropic server-side compaction summary block. This must be
    /// replayed on subsequent requests so the API can drop all content
    /// before the compacted summary.
    Compaction(CompactionBlock),
    /// Image generated server-side by the OpenAI Responses API
    /// `image_generation` built-in tool. The block carries the final
    /// base64-encoded image bytes plus metadata. Unlike [`ImageBlock`]
    /// (which represents a user-supplied image), this variant is
    /// emitted by the assistant and must NOT be dispatched through
    /// the engine's tool loop.
    GeneratedImage(GeneratedImageBlock),
}

impl ContentBlock {
    /// Return `Some(text)` if this is a [`TextBlock`].
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(&t.text),
            _ => None,
        }
    }

    /// A compaction block only its minting backend can read: the
    /// OpenAI Responses `compaction` item's opaque `encrypted_content`,
    /// as opposed to Anthropic's plaintext summary.
    ///
    /// Every wire format other than OpenAI Responses must drop it —
    /// it says nothing to them and several would reject it outright.
    /// Dropping it loses the history it stood in for, so the drop sites
    /// say so in the log rather than shrinking the context in silence.
    pub fn is_backend_bound_compaction(&self) -> bool {
        matches!(self, Self::Compaction(block) if block.encrypted_content.is_some())
    }

    /// Return `Some(&ToolUseBlock)` if this is a tool use.
    pub fn as_tool_use(&self) -> Option<&ToolUseBlock> {
        match self {
            Self::ToolUse(tu) => Some(tu),
            _ => None,
        }
    }
}

/// Text content block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    /// Accumulated text.
    pub text: String,
}

/// User image content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageBlock {
    pub source: ImageSource,
}

impl ImageBlock {
    pub fn base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            source: ImageSource {
                kind: String::from("base64"),
                media_type: media_type.into(),
                data: data.into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: String,
    pub data: String,
}

/// Server-side compaction content block.
///
/// Two providers produce one, and they are not interchangeable:
///
/// * **Anthropic** returns a plaintext summary in [`Self::content`].
/// * **OpenAI Responses** (`compaction` output item, remote
///   compaction v2) returns an opaque blob in
///   [`Self::encrypted_content`]. Only the backend that minted it can
///   read it back, so replaying it to a different provider — or even a
///   different OpenAI host — silently loses the compacted history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Opaque server-side payload standing in for the summarised
    /// history. Never rendered, never token-counted locally (its size
    /// on the wire says nothing about what it decrypts to) — it exists
    /// only to be replayed verbatim on the next request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
}

/// Tool-use content block emitted by the assistant.
///
/// Matches the Anthropic `{type:'tool_use', id, name, input}` shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUseBlock {
    /// Server-assigned tool-use id (`toolu_…`).
    pub id: String,
    /// Registered tool name.
    pub name: String,
    /// Parsed JSON input object. During streaming this is built up
    /// from `input_json_delta` fragments and parsed at block-stop.
    pub input: serde_json::Value,
}

/// Tool-result content carried on a user-role message.
///
/// Anthropic accepts either a plain string or an array of nested
/// content blocks. The array form is required for multimodal tool
/// results such as `Read` returning an image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultContentBlock>),
}

impl ToolResultContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    pub fn blocks(blocks: Vec<ToolResultContentBlock>) -> Self {
        Self::Blocks(blocks)
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Blocks(_) => None,
        }
    }

    pub fn to_plain_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    ToolResultContentBlock::Text(text) => Some(text.text.clone()),
                    ToolResultContentBlock::Image(_) | ToolResultContentBlock::Document(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    pub fn approx_len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Blocks(blocks) => blocks
                .iter()
                .map(|block| match block {
                    ToolResultContentBlock::Text(text) => text.text.len(),
                    ToolResultContentBlock::Image(_) => 8_000,
                    ToolResultContentBlock::Document(_) => 16_000,
                })
                .sum(),
        }
    }

    pub fn len(&self) -> usize {
        self.approx_len()
    }

    pub fn as_str(&self) -> &str {
        self.as_text().unwrap_or("")
    }

    pub fn starts_with(&self, needle: &str) -> bool {
        self.as_text()
            .map(|text| text.starts_with(needle))
            .unwrap_or(false)
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.to_plain_text().contains(needle)
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) => text.is_empty(),
            Self::Blocks(blocks) => blocks.is_empty(),
        }
    }
}

impl From<String> for ToolResultContent {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&String> for ToolResultContent {
    fn from(value: &String) -> Self {
        Self::Text(value.clone())
    }
}

impl From<&str> for ToolResultContent {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl Borrow<str> for ToolResultContent {
    fn borrow(&self) -> &str {
        self.as_text().unwrap_or("")
    }
}

impl PartialEq<str> for ToolResultContent {
    fn eq(&self, other: &str) -> bool {
        self.as_text() == Some(other)
    }
}

impl PartialEq<&str> for ToolResultContent {
    fn eq(&self, other: &&str) -> bool {
        self.as_text() == Some(*other)
    }
}

impl PartialEq<String> for ToolResultContent {
    fn eq(&self, other: &String) -> bool {
        self.as_text() == Some(other.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContentBlock {
    Text(TextBlock),
    Image(ImageBlock),
    Document(DocumentBlock),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentBlock {
    pub source: DocumentSource,
}

impl DocumentBlock {
    pub fn base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            source: DocumentSource {
                kind: String::from("base64"),
                media_type: media_type.into(),
                data: data.into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentSource {
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: String,
    pub data: String,
}

/// Tool-result content block carried on a user-role message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultBlock {
    /// Matches the originating [`ToolUseBlock::id`].
    pub tool_use_id: String,
    /// Tool result content. Usually text; `Read` can return nested
    /// text + image blocks so the model can inspect image files.
    pub content: ToolResultContent,
    /// Whether the tool errored.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

/// Extended-thinking content block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    /// Accumulated thinking text.
    pub thinking: String,
    /// Server-provided signature for ordinary extended-thinking blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Encrypted payload of an Anthropic `redacted_thinking` block.
    /// When set, the block's reasoning lives entirely here (no
    /// `thinking` text, no `signature`) and it must be replayed on the
    /// wire as a `redacted_thinking` block, not a `thinking` block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// Server-side tool use content block (e.g. web search invocation).
///
/// On **Anthropic** this maps to `{ type: "server_tool_use", id, name, input }`.
/// On **OpenAI Responses** this maps to a `web_search_call` output item.
///
/// Unlike [`ToolUseBlock`], these are executed server-side and must
/// NOT be dispatched through the engine's tool loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerToolUseBlock {
    /// Server-assigned tool-use id.
    pub id: String,
    /// Server tool name (e.g. `"web_search"`, `"web_search_20250305"`).
    pub name: String,
    /// Input the server passed to the tool (e.g. `{ "query": "..." }`).
    #[serde(default)]
    pub input: serde_json::Value,
}

/// A single search result entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResultEntry {
    /// Page title.
    pub title: String,
    /// Page URL.
    pub url: String,
    /// Optional snippet / summary text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// Web search result content block.
///
/// On **Anthropic** this maps to `{ type: "web_search_tool_result", ... }`.
/// On **OpenAI Responses** the search results are embedded inline in
/// the `web_search_call` item's `output` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebSearchResultBlock {
    /// Matches the originating [`ServerToolUseBlock::id`].
    pub tool_use_id: String,
    /// Structured search results, if available.
    #[serde(default)]
    pub results: Vec<SearchResultEntry>,
    /// Raw content from the provider, preserved for pass-through.
    /// Anthropic sends an array of `{ type, url, title, ... }` blocks;
    /// OpenAI embeds results differently. This field captures the
    /// provider's original shape for downstream consumers that need it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_content: Option<serde_json::Value>,
}

/// Image generated by the OpenAI Responses API `image_generation`
/// built-in tool.
///
/// Carries the final base64 bytes plus metadata. For streaming
/// (`partial_images > 0`), intermediate partial frames arrive via
/// `ContentBlockDelta::ImageDataDelta` but only the final image is
/// captured here by the [`crate::MessageAccumulator`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneratedImageBlock {
    /// Server-assigned id (e.g. `"ig_…"`). Used as the block's
    /// stable identifier across multi-turn edits.
    pub id: String,
    /// Server-reported status (`"completed"`, `"in_progress"`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Revised prompt the mainline model actually sent to
    /// `gpt-image-2`. Only present on the final image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
    /// MIME type, e.g. `"image/png"`, `"image/jpeg"`, `"image/webp"`.
    #[serde(default = "default_generated_image_media_type")]
    pub media_type: String,
    /// Base64-encoded final image bytes.
    pub data: String,
    /// Absolute path the image was persisted to, if the engine
    /// decided to save it to disk. `None` when the block is
    /// memory-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_path: Option<String>,
}

fn default_generated_image_media_type() -> String {
    "image/png".to_string()
}

/// Tool definition the caller wants the model to be able to call.
///
/// Matches the Anthropic `Tool` shape. Callers typically derive
/// `input_schema` from `rebon_tools_core::ToolInputSchema` (which is
/// the same `serde_json::Value` alias).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// Registered tool name.
    pub name: String,
    /// Human-readable description the model sees.
    pub description: String,
    /// JSON-schema-shaped input specification.
    pub input_schema: serde_json::Value,
}

/// Tool-choice directive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Let the model choose whether to call a tool.
    Auto,
    /// Force the model to call any tool.
    Any,
    /// Force the model to call a specific tool.
    Tool {
        /// Required tool name.
        name: String,
    },
    /// Disable tool use entirely.
    None,
}

/// Top-level stop reason for an assistant turn.
///
/// Matches the Anthropic `stop_reason` union. Unknown values fall
/// through to [`StopReason::Other`] so we don't lose information on
/// forward-compatible providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Natural end of turn.
    EndTurn,
    /// Hit `max_tokens`.
    MaxTokens,
    /// Hit a stop sequence.
    StopSequence,
    /// Assistant emitted a `tool_use` block — caller should dispatch
    /// the tool and call again.
    ToolUse,
    /// Pause turn (server-side feature).
    PauseTurn,
    /// Model refused the request.
    Refusal,
    /// Anthropic paused after writing a server-side compaction block.
    Compaction,
    /// Generation reached the model context window before normal completion.
    ModelContextWindowExceeded,
    /// Unknown / forward-compatible value.
    #[serde(untagged)]
    Other(String),
}

/// Final assistant message synthesised by the [`MessageAccumulator`].
///
/// This is what `create_message` (non-streaming) returns, and what
/// `QueryEngine` typically hands back to its caller after consuming
/// a stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// Server-assigned message id.
    pub id: String,
    /// Model name that served the response.
    pub model: String,
    /// Content blocks in display order.
    pub content: Vec<ContentBlock>,
    /// Final stop reason, or `None` if the stream ended without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// Final usage totals.
    pub usage: Usage,
}

impl AssistantMessage {
    /// Whether this turn asks the caller to dispatch any tool_use
    /// blocks.
    pub fn has_tool_use(&self) -> bool {
        self.content
            .iter()
            .any(|c| matches!(c, ContentBlock::ToolUse(_)))
    }

    /// Borrow every tool-use block in the message.
    pub fn tool_uses(&self) -> impl Iterator<Item = &ToolUseBlock> {
        self.content.iter().filter_map(ContentBlock::as_tool_use)
    }

    /// Concatenate every text block into a single string. Useful for
    /// quick assertions in tests and for UI surfaces that only need
    /// the body.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("")
    }

    /// Cut the reply at the first requested stop sequence, the way a
    /// provider that supports them natively already would have.
    ///
    /// `stop_sequences` is part of the request contract, but only the
    /// Anthropic wire format carries it: the Responses API has no
    /// `stop` parameter and the chat-completions path never sends one,
    /// so on those providers the field is silently dropped and the
    /// model keeps writing. A caller with a strict output grammar then
    /// rejects a perfectly good answer because of what trails it —
    /// which is how the auto-mode classifier came to block every tool
    /// call on a Codex-backed session.
    ///
    /// The sequence itself is dropped along with everything after it,
    /// and `stop_reason` becomes [`StopReason::StopSequence`], mirroring
    /// Anthropic. Returns whether anything was cut — `false` on a
    /// provider that already honoured the request, since the sequence
    /// is then absent. This cannot refund the tokens the model spent
    /// past the stop sequence; it only makes the result match what was
    /// asked for.
    pub fn apply_stop_sequences(&mut self, stop_sequences: &[String]) -> bool {
        let sequences: Vec<&str> = stop_sequences
            .iter()
            .map(String::as_str)
            .filter(|sequence| !sequence.is_empty())
            .collect();
        if sequences.is_empty() {
            return false;
        }

        let hit = self.content.iter().enumerate().find_map(|(index, block)| {
            let ContentBlock::Text(text) = block else {
                return None;
            };
            sequences
                .iter()
                .filter_map(|sequence| text.text.find(sequence))
                .min()
                .map(|cut| (index, cut))
        });
        let Some((index, cut)) = hit else {
            return false;
        };

        if let Some(ContentBlock::Text(text)) = self.content.get_mut(index) {
            text.text.truncate(cut);
        }
        // Everything after the cut is output the model would never
        // have produced — later blocks included.
        self.content.truncate(index + 1);
        self.stop_reason = Some(StopReason::StopSequence);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_message(content: Vec<ContentBlock>) -> AssistantMessage {
        AssistantMessage {
            id: "msg".to_owned(),
            model: "model".to_owned(),
            content,
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
        }
    }

    #[test]
    fn stop_sequence_cuts_the_sequence_and_everything_after_it() {
        let mut message = assistant_message(vec![
            ContentBlock::Text(TextBlock {
                text: "<block>no</block>\n\nThis only reads state.".to_owned(),
            }),
            ContentBlock::Text(TextBlock {
                text: "and more".to_owned(),
            }),
        ]);

        assert!(message.apply_stop_sequences(&["</block>".to_owned()]));
        assert_eq!(message.text(), "<block>no");
        assert_eq!(message.content.len(), 1);
        assert_eq!(message.stop_reason, Some(StopReason::StopSequence));
    }

    #[test]
    fn stop_sequence_application_is_a_no_op_when_the_provider_honoured_it() {
        let mut message = assistant_message(vec![ContentBlock::Text(TextBlock {
            text: "<block>no".to_owned(),
        })]);

        assert!(!message.apply_stop_sequences(&["</block>".to_owned()]));
        assert_eq!(message.text(), "<block>no");
        assert_eq!(message.stop_reason, Some(StopReason::EndTurn));
        // No sequences, and empty ones, leave the message alone too.
        assert!(!message.apply_stop_sequences(&[]));
        assert!(!message.apply_stop_sequences(&[String::new()]));
        assert_eq!(message.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn the_earliest_stop_sequence_wins_across_blocks_and_candidates() {
        let mut message = assistant_message(vec![
            ContentBlock::Thinking(ThinkingBlock {
                thinking: "STOP inside reasoning is not output".to_owned(),
                signature: None,
                data: None,
            }),
            ContentBlock::Text(TextBlock {
                text: "keep END drop STOP drop".to_owned(),
            }),
        ]);

        assert!(message.apply_stop_sequences(&["STOP".to_owned(), "END".to_owned()]));
        assert_eq!(message.text(), "keep ");
        assert_eq!(message.content.len(), 2, "reasoning before the cut stays");
    }

    #[test]
    fn message_user_text_builds_single_text_block() {
        let m = Message::user_text("hi");
        assert_eq!(m.role, Role::User);
        assert_eq!(m.content.len(), 1);
        assert_eq!(m.content[0].as_text(), Some("hi"));
    }

    #[test]
    fn content_block_serializes_with_type_discriminator() {
        let block = ContentBlock::Text(TextBlock {
            text: "hello".into(),
        });
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        assert!(json.contains("\"text\":\"hello\""));
    }

    #[test]
    fn tool_use_block_round_trips() {
        let block = ContentBlock::ToolUse(ToolUseBlock {
            id: "toolu_1".into(),
            name: "Read".into(),
            input: serde_json::json!({ "path": "foo.rs" }),
        });
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn stop_reason_round_trips_known_and_unknown() {
        assert_eq!(
            serde_json::from_str::<StopReason>("\"end_turn\"").unwrap(),
            StopReason::EndTurn
        );
        assert_eq!(
            serde_json::from_str::<StopReason>("\"tool_use\"").unwrap(),
            StopReason::ToolUse
        );
        // Forward-compatible value should land in Other.
        let forward: StopReason = serde_json::from_str("\"content_filter\"").unwrap();
        assert_eq!(forward, StopReason::Other("content_filter".into()));
    }

    #[test]
    fn assistant_message_text_concatenates_blocks() {
        let msg = AssistantMessage {
            id: "msg_1".into(),
            model: "claude-sonnet-4-6".into(),
            content: vec![
                ContentBlock::Text(TextBlock {
                    text: "hello ".into(),
                }),
                ContentBlock::ToolUse(ToolUseBlock {
                    id: "toolu_1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                }),
                ContentBlock::Text(TextBlock {
                    text: "world".into(),
                }),
            ],
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage::default(),
        };
        assert_eq!(msg.text(), "hello world");
        assert!(msg.has_tool_use());
        assert_eq!(msg.tool_uses().count(), 1);
    }

    #[test]
    fn server_tool_use_block_round_trips() {
        let block = ContentBlock::ServerToolUse(ServerToolUseBlock {
            id: "srv_1".into(),
            name: "web_search".into(),
            input: serde_json::json!({ "query": "rust async" }),
        });
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"server_tool_use\""));
        assert!(json.contains("\"name\":\"web_search\""));
        assert!(json.contains("\"query\":\"rust async\""));
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn web_search_result_block_round_trips() {
        let block = ContentBlock::WebSearchResult(WebSearchResultBlock {
            tool_use_id: "srv_1".into(),
            results: vec![
                SearchResultEntry {
                    title: "Async Rust".into(),
                    url: "https://example.com/async".into(),
                    snippet: Some("A guide to async programming".into()),
                },
                SearchResultEntry {
                    title: "Tokio".into(),
                    url: "https://tokio.rs".into(),
                    snippet: None,
                },
            ],
            raw_content: None,
        });
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"web_search_result\""));
        assert!(json.contains("\"tool_use_id\":\"srv_1\""));
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn server_tool_use_excluded_from_tool_uses_and_has_tool_use() {
        let msg = AssistantMessage {
            id: "msg_1".into(),
            model: "claude-sonnet-4-6".into(),
            content: vec![
                ContentBlock::Text(TextBlock {
                    text: "Let me search.".into(),
                }),
                ContentBlock::ServerToolUse(ServerToolUseBlock {
                    id: "srv_1".into(),
                    name: "web_search".into(),
                    input: serde_json::json!({ "query": "test" }),
                }),
                ContentBlock::WebSearchResult(WebSearchResultBlock {
                    tool_use_id: "srv_1".into(),
                    results: vec![],
                    raw_content: None,
                }),
                ContentBlock::Text(TextBlock {
                    text: "Here are the results.".into(),
                }),
            ],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
        };
        // Server tool use blocks must NOT be reported as tool_use
        // — the engine must not try to dispatch them.
        assert!(!msg.has_tool_use());
        assert_eq!(msg.tool_uses().count(), 0);
        assert_eq!(msg.text(), "Let me search.Here are the results.");
    }

    #[test]
    fn mixed_tool_use_and_server_tool_use() {
        let msg = AssistantMessage {
            id: "msg_2".into(),
            model: "m".into(),
            content: vec![
                ContentBlock::ToolUse(ToolUseBlock {
                    id: "toolu_1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                }),
                ContentBlock::ServerToolUse(ServerToolUseBlock {
                    id: "srv_1".into(),
                    name: "web_search".into(),
                    input: serde_json::json!({}),
                }),
            ],
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage::default(),
        };
        // Only the client-side ToolUse should be counted.
        assert!(msg.has_tool_use());
        assert_eq!(msg.tool_uses().count(), 1);
        assert_eq!(msg.tool_uses().next().unwrap().name, "Read");
    }
}
