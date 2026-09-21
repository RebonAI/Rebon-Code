//! The transcript row as it comes off disk — the **input** end of the row
//! stream.
//!
//! Message graph used by the Rust renderer for transcript rows. The current
//! model covers user, assistant, attachment, system, and fallback rows.
//!
//! ## Input rows and output rows are two different types
//!
//! [`Message`] here is a serde mirror of a persisted transcript line: one
//! row per JSONL entry, tool use and tool result still living as content
//! blocks inside their parent message, nothing grouped and nothing
//! collapsed. [`crate::types::MessageRow`] is the **output** end: the same
//! four kinds, dispatched for drawing.
//!
//! Folding is the third thing, and it is neither end: [`crate::fold_rows`]
//! answers *which rows group together* as a plan over indices, so a folded
//! group never becomes a row of its own. The names overlap
//! (`UserMessage`,
//! `AssistantMessage`, `SystemMessage`, `UserContentBlock`,
//! `AssistantContentBlock` exist on both sides), so neither module is
//! re-exported at the crate root: reach them as
//! `rebon_render::transcript_row::Message` and
//! `rebon_render::MessageRow`.
//!
//! ## Why the types sit in this crate
//!
//! Nothing here knows what a terminal cell is — it is serde plus two
//! string sentinels — and the projection it feeds has to be reachable from
//! every surface without any of them growing a ratatui edge.
//!
//! ## Why a row is a whole message
//!
//! The row union has exactly four members:
//!
//! ```text
//! message:
//!   | NormalizedUserMessage
//!   | AssistantMessage
//!   | AttachmentMessage
//!   | SystemMessage
//! ```
//!
//! …where user and assistant messages carry an inner Anthropic-SDK
//! content block array (`message.message.content`) and the renderer
//! iterates that array inside a single row. Tool use and tool result are
//! **content blocks inside their parent message**, not standalone rows.
//! Expansion correlation happens by walking the whole message list for a
//! tool-use block whose `id` matches the `tool_use_id` of a tool-result
//! block.
//!
//! This module captures that shape with content block inner unions
//! following the Anthropic Beta SDK discriminants used by the current
//! transcript data.
//!
//! ## Modelled transcript scope
//!
//! Typed variants are implemented for the shapes the current renderer
//! actually needs to render a coherent transcript:
//!
//! * `user` — text + image + tool_result content blocks.
//! * `assistant` — text + thinking + redacted_thinking + tool_use
//!   content blocks. The feature-
//!   flagged `connector_text` / `server_tool_use` / `advisor_tool_result`
//!   variants intentionally route to
//!   the `Other` open-world fallback on `AssistantContentBlock`.
//! * `attachment` — kept as an opaque JSON payload. Attachment
//!   rendering lives in [`crate::attachment`], over the payload this
//!   row carries.
//! * `system` — modelled with `subtype` + `content` + `level`, which
//!   is enough to re-route `local_command` to the user-prompt gutter
//!   and to render system rows as
//!   muted text. The richer subtype-specific payloads (api_error,
//!   compact_boundary payload, file_snapshot, etc.) continue to live
//!   in the engine's own system-message type and are **not** duplicated
//!   here.
//! * `grouped_tool_use` / `collapsed_read_search` — **never rows here.**
//!   Grouping and read/search collapse are answered by
//!   [`crate::fold_rows`] as a plan over the rows above; a persisted line
//!   carrying either `type` falls through to `Unknown`.
//!
//! ## Deserialization strategy
//!
//! Every variant implements `serde::Deserialize` so Rust tests can
//! load JSON transcript fixtures without any translation layer. The
//! field names use the wire camelCase (`isCompactSummary`,
//! `isMeta`, `isVisibleInTranscriptOnly`, `imagePasteIds`,
//! `advisorModel`, `toolUseID`, `planContent`, …) so round-tripping
//! a captured transcript through the Rust types is lossless for the
//! modelled fields.
//!
//! Fields this module does not model (the bulk of `AssistantMessage`
//! — `apiError`, `requestId`, `usage`, `stop_reason`, `id`, `model`,
//! `container`, `context_management`, `isVirtual`) are deliberately
//! left off the struct. They'll be added as the engine integration
//! grows. Extra fields in incoming JSON are tolerated because the
//! inner Anthropic content block is deserialized via
//! `#[serde(tag = "type")]` which ignores unknown sibling keys.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Single transcript row type matched by the Rust renderer.
///
/// `#[serde(tag = "type")]` maps directly to the wire `type` field —
/// so `{"type":"user", ...}` deserializes as `Message::User(_)`
/// for each row in a JSONL transcript file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    /// `type: 'attachment'` row. Kept as opaque JSON; the richer variant
    /// modelling lives with the engine's own attachment message type and is
    /// not modelled here.
    Attachment(AttachmentRow),
    System(SystemMessage),
    // `grouped_tool_use` and `collapsed_read_search` are deliberately
    // absent: folding is answered by `crate::fold_rows` over these rows,
    // not stored as a row kind. Either `type` on a persisted line falls
    // through to the open-world fallback below.
    /// Open-world fallback — any top-level `type` value not listed
    /// above (including `grouped_tool_use` / `collapsed_read_search`,
    /// plus legacy `progress` rows
    /// bridged from legacy data). The raw JSON is preserved byte-for-byte
    /// so round-trips are lossless.
    #[serde(other, skip_serializing)]
    Unknown,
}

impl Message {
    /// Stable uuid for the row. Every known message row has a uuid. For the
    /// open-world `Unknown` fallback there is no uuid, so callers
    /// that key by uuid should treat `Unknown` rows as
    /// non-identifiable; unknown row types render nothing and are
    /// not added to the message lookup map.
    pub fn uuid(&self) -> Option<&str> {
        match self {
            Self::User(m) => Some(&m.uuid),
            Self::Assistant(m) => Some(&m.uuid),
            Self::Attachment(m) => Some(&m.uuid),
            Self::System(m) => Some(&m.uuid),
            Self::Unknown => None,
        }
    }

    pub fn sticky_anchor_preview_text(&self) -> Option<String> {
        let Self::User(message) = self else {
            return None;
        };
        if message.is_meta == Some(true) {
            return None;
        }
        for block in &message.message.content {
            match block {
                UserContentBlock::Text(text) => {
                    let preview = text.text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !preview.is_empty() {
                        return Some(preview);
                    }
                }
                UserContentBlock::Image(_) => return Some("[image]".to_string()),
                UserContentBlock::ToolResult(_) => {}
            }
        }
        None
    }

    /// Extract the `tool_use_id` of the first tool-result content
    /// block on a user message, if any.
    ///
    /// Returns `None` for every message kind that does not carry a
    /// tool_result block in its content array, including user
    /// messages whose first block is text/image.
    pub fn tool_result_id(&self) -> Option<&str> {
        if let Self::User(u) = self {
            for block in &u.message.content {
                if let UserContentBlock::ToolResult(t) = block {
                    return Some(&t.tool_use_id);
                }
            }
        }
        None
    }

    /// Collect the tool-use ids present in this message (assistant
    /// messages only — user messages carry tool_result, not
    /// tool_use). Scans the assistant message content array for
    /// `tool_use` blocks.
    pub fn tool_use_ids(&self) -> Vec<&str> {
        let mut out = Vec::new();
        if let Self::Assistant(a) = self {
            for block in &a.message.content {
                if let AssistantContentBlock::ToolUse(t) = block {
                    out.push(t.id.as_str());
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Pipeline primitives
// ---------------------------------------------------------------------------

/// First transcript visibility filter for message rows.
/// It decides whether a message is worth showing at all using a
/// 6-rule decision tree:
///
/// 1. a `progress`, `attachment` or `system` type → visible
/// 2. string content → visible when it does not trim to empty
/// 3. empty array content → hidden
/// 4. array content with more than one block → visible
/// 5. first block is not text → visible
/// 6. first block is text → visible only when all three hold:
///    - the text does not trim to empty
///    - text ≠ `NO_CONTENT_MESSAGE`
///    - text ≠ `INTERRUPT_MESSAGE_FOR_TOOL_USE`
///
/// Note the deliberate asymmetry at rule 6: plain `INTERRUPT_MESSAGE`
/// (`[Request interrupted by user]`) is kept, but
/// `INTERRUPT_MESSAGE_FOR_TOOL_USE` is dropped. That asymmetry is
/// load-bearing and is pinned by
/// `tests::is_not_empty_message_table`.
///
/// ## String-content content path
///
/// Rule 2 describes a `message.content` that is a bare string, which the
/// Rust `Message` type as modelled does not accept — our
/// `UserMessageInner.content` and `AssistantMessageInner.content`
/// are always `Vec<...Block>`. Legacy wire rows that carry a bare
/// string for `message.content` would fail to deserialize into the
/// current Rust `Message` type; they would land in
/// `Message::Unknown` instead and be handled by the top-level
/// `Unknown → true` branch below. A future runtime integration that adds an
/// untagged-enum `StringOrBlocks` wrapper can preserve rule 2
/// unchanged.
pub fn is_not_empty_message(msg: &Message) -> bool {
    use crate::tool_results::INTERRUPT_MESSAGE_FOR_TOOL_USE;
    use crate::user_text::NO_CONTENT_MESSAGE;

    // Rule 1 — progress / attachment / system rows stay visible.
    // `Message::Unknown` covers legacy `progress` rows and any other
    // top-level type the `Message` enum doesn't model. Keeping them on
    // the visible side preserves legacy progress rows.
    match msg {
        Message::Attachment(_) | Message::System(_) | Message::Unknown => return true,
        Message::User(_) | Message::Assistant(_) => {}
    }

    // Extract the content block array for user / assistant.
    //
    // Rule 2 cannot fire here:
    // The string-content branch is unreachable from the current Rust
    // type — `UserMessageInner.content` and
    // `AssistantMessageInner.content` are both `Vec<...Block>`.
    // Legacy string-content wire rows fail to deserialize into
    // User/Assistant and land in Message::Unknown, which we've
    // already returned true for above.
    let content_len = match msg {
        Message::User(u) => u.message.content.len(),
        Message::Assistant(a) => a.message.content.len(),
        _ => unreachable!("handled by early return above"),
    };

    // Rule 3 — empty array → drop.
    if content_len == 0 {
        return false;
    }

    // Rule 4 — length > 1 → keep.
    if content_len > 1 {
        return true;
    }

    // Length == 1 — inspect the single block.
    let is_text_block = match msg {
        Message::User(u) => matches!(u.message.content[0], UserContentBlock::Text(_)),
        Message::Assistant(a) => matches!(a.message.content[0], AssistantContentBlock::Text(_)),
        _ => unreachable!(),
    };

    // Rule 5 — non-text first block → keep.
    if !is_text_block {
        return true;
    }

    // Rule 6 — text block — keep only if all three conditions hold:
    //   - the text does not trim to empty
    //   - the text is not `NO_CONTENT_MESSAGE`
    //   - the text is not `INTERRUPT_MESSAGE_FOR_TOOL_USE`
    //
    // Note the asymmetry: plain INTERRUPT_MESSAGE is NOT in this
    // filter list. Only the for-tool-use variant is. Pinned by
    // `is_not_empty_interrupt_message_asymmetry` in tests.
    let text: &str = match msg {
        Message::User(u) => match &u.message.content[0] {
            UserContentBlock::Text(t) => &t.text,
            _ => unreachable!("guarded by is_text_block"),
        },
        Message::Assistant(a) => match &a.message.content[0] {
            AssistantContentBlock::Text(t) => &t.text,
            _ => unreachable!("guarded by is_text_block"),
        },
        _ => unreachable!(),
    };

    !text.trim().is_empty() && text != NO_CONTENT_MESSAGE && text != INTERRUPT_MESSAGE_FOR_TOOL_USE
}

// ---------------------------------------------------------------------------
// user
// ---------------------------------------------------------------------------

/// Normalized user message fields consumed by the Rust renderer.
///
/// * `uuid`, `timestamp` — row identity.
/// * `message.content` — the Anthropic content block array.
/// * `isCompactSummary` — branch to compact-summary rendering.
/// * `isMeta`, `isVisibleInTranscriptOnly` — filter hints used by
///   user-message visibility and sticky-prompt handling.
/// * `imagePasteIds` — paste ids for `<image>` blocks.
/// * `planContent` — optional plan override path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UserMessage {
    pub uuid: String,
    pub timestamp: String,
    pub message: UserMessageInner,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "isCompactSummary")]
    pub is_compact_summary: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "isMeta")]
    pub is_meta: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "isVisibleInTranscriptOnly")]
    pub is_visible_in_transcript_only: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "imagePasteIds")]
    pub image_paste_ids: Option<Vec<u32>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "planContent")]
    pub plan_content: Option<String>,
}

/// The `message` sub-object on a user row. Matches the shape that the
/// Anthropic SDK's `MessageParam` wraps — a `role: 'user'` string and
/// a `content` array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UserMessageInner {
    /// Absent on a hand-authored or older line; a row that says nothing
    /// about its role is the role its parent row already declared, and
    /// dropping it whole would lose a message a person really sent.
    #[serde(default)]
    pub role: UserRole,
    pub content: Vec<UserContentBlock>,
}

/// The `role` literal on a user message. Always `"user"` on the
/// wire; modelled as a single-variant enum so deserialization
/// rejects a *wrong* role instead of silently accepting it as `Other`.
/// A *missing* one is tolerated — see [`UserMessageInner::role`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    #[default]
    User,
}

/// A single content block inside a user message. Dispatched by
/// the `type` discriminant: `text`, `image` or `tool_result`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContentBlock {
    Text(UserTextBlock),
    Image(UserImageBlock),
    ToolResult(UserToolResultBlock),
}

/// `{ type: 'text', text: '...' }` content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UserTextBlock {
    pub text: String,
}

/// `{ type: 'image', source: { ... } }` content block. The source
/// shape is Anthropic-SDK defined and not consumed by the renderer
/// directly here — preserved as opaque JSON so wire
/// round-trip is lossless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UserImageBlock {
    #[serde(default)]
    pub source: JsonValue,
}

/// `{ type: 'tool_result', tool_use_id, content, is_error? }`.
///
/// `content` on the wire can be a string OR an array of content
/// blocks (Anthropic's nested content format). This implementation
/// accepts both via an untagged helper: strings become
/// `ToolResultContent::Text(_)`, arrays become
/// `ToolResultContent::Blocks(_)`. The renderer currently only
/// formats the string form; the Blocks form falls through to a
/// JSON-stringify display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub struct UserToolResultBlock {
    #[serde(rename = "tool_use_id")]
    pub tool_use_id: String,
    pub content: ToolResultContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// Union of the two valid shapes for a tool_result `content` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<JsonValue>),
}

impl ToolResultContent {
    /// Flatten into a single string for display. The block form
    /// currently serializes each block back to JSON; richer rendering
    /// lands in a future renderer extension.
    pub fn as_display_string(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .map(|b| {
                    b.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| b.to_string())
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

// ---------------------------------------------------------------------------
// assistant
// ---------------------------------------------------------------------------

/// One assistant row as persisted in a transcript line.
///
/// Only the fields the renderer reads are modelled; the engine-side
/// bookkeeping (`requestId`, `apiError`, `error`, `errorDetails`,
/// `isVirtual`, `usage`, `message.id`, `message.model`,
/// `message.stop_reason`, `message.container`,
/// `message.context_management`) is not duplicated here — it will
/// land as part of the engine bridge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AssistantMessage {
    pub uuid: String,
    pub timestamp: String,
    pub message: AssistantMessageInner,

    /// `isApiErrorMessage` — keeps API error rows visible even in
    /// brief-only mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "isApiErrorMessage")]
    pub is_api_error_message: Option<bool>,

    /// `advisorModel` — threaded to advisor rows. Advisor blocks
    /// are not rendered yet but the field is preserved so round-trip
    /// stays lossless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "advisorModel")]
    pub advisor_model: Option<String>,

    /// `Some(true)` on a row minted when the inline overflow flush split
    /// one still-streaming text block across scrollback commits. The
    /// renderer joins such a row to the previous one — no leading margin,
    /// no gutter dot — so the split halves read as a single message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "isStreamContinuation")]
    pub is_stream_continuation: Option<bool>,
}

/// The `message` sub-object on an assistant row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AssistantMessageInner {
    /// Tolerated when absent, for the same reason as
    /// [`UserMessageInner::role`].
    #[serde(default)]
    pub role: AssistantRole,
    pub content: Vec<AssistantContentBlock>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum AssistantRole {
    #[default]
    Assistant,
}

/// A single content block inside an assistant message. Dispatched
/// by the `type` discriminant.
///
/// The deferred variants (`server_tool_use`, `advisor_tool_result`,
/// `connector_text`, plus any future Anthropic content block
/// variants) fall through to [`AssistantContentBlock::Other`] via
/// `#[serde(other)]`, preserving the raw JSON so legacy rows don't
/// break parsing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContentBlock {
    Text(AssistantTextBlock),
    Thinking(AssistantThinkingBlock),
    RedactedThinking(AssistantRedactedThinkingBlock),
    ToolUse(AssistantToolUseBlock),
    GeneratedImage(AssistantGeneratedImageBlock),
    #[serde(other, skip_serializing)]
    Other,
}

/// `{ type: 'text', text: '...' }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AssistantTextBlock {
    pub text: String,
}

/// `{ type: 'thinking', thinking: '...' }` — note the inner field is
/// `thinking`, not `text`, per the Anthropic SDK schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AssistantThinkingBlock {
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// `{ type: 'redacted_thinking', data: '...' }` — the renderer shows
/// a `[redacted thinking]` placeholder and never reveals `data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AssistantRedactedThinkingBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// `{ type: 'tool_use', id, name, input }`. The `input` is opaque
/// Anthropic-SDK JSON — this code doesn't introspect it; per-tool
/// rendering lands in future renderer extensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AssistantToolUseBlock {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub input: JsonValue,
    /// Tool call content payloads (e.g. `Diff` for Edit tools).
    ///
    /// Populated from the streaming overlay when the tool completes, and
    /// on replay by [`crate::transcript_replay::enrich_tool_use_blocks`]
    /// from the matching `tool_result`. Serialized when present so a
    /// surface reading rows off the wire sees the tool output the
    /// terminal sees; absent when there is none, so a row that was never
    /// enriched serializes to exactly the bytes it did before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_content: Option<Vec<rebon_types::ToolCallContent>>,
    /// Raw tool output payload, retained in-memory so rewind can
    /// restore file contents for the current session.
    ///
    /// Also on the wire, and not redundant with `tool_call_content`:
    /// `enrich_tool_use_blocks` leaves that `None` for a result with no
    /// structured update content, and this is then the only carrier of
    /// the output a card has to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<JsonValue>,
    /// Optional streaming title/progress label retained for current-session rendering.
    #[serde(skip)]
    pub title: Option<String>,
    /// Optional file/line locations retained for current-session rendering.
    #[serde(skip)]
    pub locations: Option<Vec<rebon_types::ToolCallLocation>>,
    /// Optional lifecycle status. Set live during a turn and on replay
    /// from the matching `tool_result`; serialized when present so a
    /// card off the wire can tell a completed call from a failed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<rebon_types::ToolCallStatus>,
}

impl PartialEq for AssistantToolUseBlock {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.name == other.name && self.input == other.input
    }
}

/// `{ type: 'generated_image', id, status?, revised_prompt?, media_type,
/// data, saved_path? }` — the assistant-side image payload produced by
/// the OpenAI Responses `image_generation` built-in tool. Kept aligned
/// with `rebon_api::GeneratedImageBlock` so transcripts round-trip.
///
/// Rebon no longer requests that tool (image generation is the `ImageGen`
/// tool now); the block stays so transcripts written before still render.
/// `data` is optional because the engine of that time stripped the base64
/// payload once it had saved the image, leaving `saved_path` as the
/// load-bearing field — the renderer surfaces that and the revised prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AssistantGeneratedImageBlock {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
    #[serde(default = "default_generated_image_media_type")]
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_path: Option<String>,
}

fn default_generated_image_media_type() -> String {
    "image/png".to_string()
}

// ---------------------------------------------------------------------------
// attachment
// ---------------------------------------------------------------------------

/// `{ type: 'attachment', attachment: ... }` row wrapper. The inner
/// `attachment` payload is kept as opaque JSON: the rich typed attachment
/// shape lives with the engine, and keeping it opaque here avoids
/// cross-crate type bleed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachmentRow {
    pub uuid: String,
    pub timestamp: String,
    pub attachment: JsonValue,
}

// ---------------------------------------------------------------------------
// system
// ---------------------------------------------------------------------------

/// `{ type: 'system', subtype, level, content, ... }`.
///
/// Subtype-specific payloads (API error, file snapshot, compact boundary,
/// local command, …) exist on the wire but are not typed here.
/// This type only exposes the common fields the renderer reads:
/// `subtype`, `content`, `level`, `isMeta`. The rest stay on the wire
/// as extra JSON fields — they're not dropped, they just aren't
/// surfaced as typed fields here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SystemMessage {
    pub uuid: String,
    pub timestamp: String,
    pub subtype: String,

    /// Present on most typed subtypes; optional so legacy rows
    /// don't fail to parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<SystemLevel>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "isMeta")]
    pub is_meta: Option<bool>,
}

/// `SystemMessageLevel` — wire union `'info' | 'warning' | 'error'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum SystemLevel {
    Info,
    Warning,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- fixtures ------------------------------------------------
    //
    // Each fixture is a hand-authored JSON value in the expected wire shape.
    // The tests deserialize the fixture into the Rust type, assert field-by-field
    // equality, then re-serialize and assert the fixture is preserved.

    /// Assistant fixture stripped down to the fields these structs carry.
    /// Omitted fields (`apiError`, `usage`,
    /// `requestId`, `id`, `stop_reason`, `container`, `model`,
    /// `context_management`, `isVirtual`) may be present on the wire
    /// but are not consumed here, so the Rust struct
    /// tolerates their absence and round-trips without them.
    fn assistant_text_fixture(uuid: &str, text: &str) -> JsonValue {
        json!({
            "type": "assistant",
            "uuid": uuid,
            "timestamp": "2026-04-07T12:34:56.000Z",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": text }
                ]
            },
            "isApiErrorMessage": false
        })
    }

    fn user_text_fixture(uuid: &str, text: &str) -> JsonValue {
        json!({
            "type": "user",
            "uuid": uuid,
            "timestamp": "2026-04-07T12:34:56.000Z",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": text }
                ]
            }
        })
    }

    #[test]
    fn assistant_text_fixture_round_trips() {
        let fixture = assistant_text_fixture("a-1", "hello there");
        let msg: Message = serde_json::from_value(fixture.clone()).unwrap();
        match &msg {
            Message::Assistant(a) => {
                assert_eq!(a.uuid, "a-1");
                assert_eq!(a.timestamp, "2026-04-07T12:34:56.000Z");
                assert_eq!(a.is_api_error_message, Some(false));
                assert_eq!(a.message.content.len(), 1);
                match &a.message.content[0] {
                    AssistantContentBlock::Text(t) => assert_eq!(t.text, "hello there"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn user_text_fixture_round_trips() {
        let fixture = user_text_fixture("u-1", "please help");
        let msg: Message = serde_json::from_value(fixture.clone()).unwrap();
        match &msg {
            Message::User(u) => {
                assert_eq!(u.uuid, "u-1");
                assert_eq!(u.message.content.len(), 1);
                match &u.message.content[0] {
                    UserContentBlock::Text(t) => assert_eq!(t.text, "please help"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    /// Assistant message with a text block followed by a tool_use
    /// block — the common "I'll run this command" shape for a Bash invocation.
    #[test]
    fn assistant_text_then_tool_use_matches_content_array_order() {
        let fixture = json!({
            "type": "assistant",
            "uuid": "a-2",
            "timestamp": "2026-04-07T12:34:56.000Z",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Let me list the files." },
                    {
                        "type": "tool_use",
                        "id": "toolu_01",
                        "name": "Bash",
                        "input": { "command": "ls -la" }
                    }
                ]
            }
        });
        let msg: Message = serde_json::from_value(fixture.clone()).unwrap();
        if let Message::Assistant(a) = &msg {
            assert_eq!(a.message.content.len(), 2);
            assert!(matches!(
                &a.message.content[0],
                AssistantContentBlock::Text(_)
            ));
            match &a.message.content[1] {
                AssistantContentBlock::ToolUse(t) => {
                    assert_eq!(t.id, "toolu_01");
                    assert_eq!(t.name, "Bash");
                    assert_eq!(t.input["command"], "ls -la");
                }
                other => panic!("expected ToolUse, got {other:?}"),
            }
            // tool_use_ids helper must list the single tool use id.
            let ids = msg.tool_use_ids();
            assert_eq!(ids, vec!["toolu_01"]);
        } else {
            panic!("expected Assistant");
        }
    }

    /// User message carrying a tool_result — produced for the "bash
    /// output" turn after an assistant tool_use. Shape per the Anthropic SDK
    /// `ToolResultBlockParam`.
    #[test]
    fn user_tool_result_with_string_content_round_trips() {
        let fixture = json!({
            "type": "user",
            "uuid": "u-2",
            "timestamp": "2026-04-07T12:34:56.000Z",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_01",
                        "content": "drwxr-xr-x  5 user user  160 Apr  7 12:34 .",
                        "is_error": false
                    }
                ]
            }
        });
        let msg: Message = serde_json::from_value(fixture.clone()).unwrap();
        if let Message::User(u) = &msg {
            match &u.message.content[0] {
                UserContentBlock::ToolResult(t) => {
                    assert_eq!(t.tool_use_id, "toolu_01");
                    assert_eq!(t.is_error, Some(false));
                    match &t.content {
                        ToolResultContent::Text(s) => assert!(s.contains("drwxr-xr-x")),
                        other => panic!("expected Text content, got {other:?}"),
                    }
                }
                other => panic!("expected ToolResult, got {other:?}"),
            }
        } else {
            panic!("expected User");
        }
        // `tool_result_id` helper finds the correlation key.
        assert_eq!(msg.tool_result_id(), Some("toolu_01"));
    }

    /// Tool result with an ARRAY content payload — Anthropic's nested
    /// content-block form. Must deserialize into the Blocks arm.
    #[test]
    fn user_tool_result_with_array_content_deserializes_as_blocks() {
        let fixture = json!({
            "type": "user",
            "uuid": "u-3",
            "timestamp": "2026-04-07T12:34:56.000Z",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_02",
                        "content": [
                            { "type": "text", "text": "first block" },
                            { "type": "text", "text": "second block" }
                        ]
                    }
                ]
            }
        });
        let msg: Message = serde_json::from_value(fixture).unwrap();
        if let Message::User(u) = msg {
            if let UserContentBlock::ToolResult(t) = &u.message.content[0] {
                if let ToolResultContent::Blocks(blocks) = &t.content {
                    assert_eq!(blocks.len(), 2);
                    assert_eq!(t.content.as_display_string(), "first block\nsecond block");
                } else {
                    panic!("expected Blocks content");
                }
            }
        }
    }

    #[test]
    fn assistant_thinking_block_uses_thinking_field_not_text() {
        // This is a load-bearing detail: the Anthropic SDK's thinking
        // block has `thinking` as the content field, not `text`. An
        // earlier cut of this crate incorrectly used `text` — the
        // fixture below would have failed to parse. Kept as a
        // regression guard.
        let fixture = json!({
            "type": "assistant",
            "uuid": "a-3",
            "timestamp": "2026-04-07T12:34:56.000Z",
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "type": "thinking",
                        "thinking": "Let me reason about this...",
                        "signature": "sig123"
                    }
                ]
            }
        });
        let msg: Message = serde_json::from_value(fixture).unwrap();
        if let Message::Assistant(a) = msg {
            match &a.message.content[0] {
                AssistantContentBlock::Thinking(t) => {
                    assert_eq!(t.thinking, "Let me reason about this...");
                    assert_eq!(t.signature.as_deref(), Some("sig123"));
                }
                other => panic!("expected Thinking, got {other:?}"),
            }
        }
    }

    #[test]
    fn assistant_redacted_thinking_has_data_field_not_text() {
        let fixture = json!({
            "type": "assistant",
            "uuid": "a-4",
            "timestamp": "2026-04-07T12:34:56.000Z",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "redacted_thinking", "data": "opaque-blob" }
                ]
            }
        });
        let msg: Message = serde_json::from_value(fixture).unwrap();
        if let Message::Assistant(a) = msg {
            match &a.message.content[0] {
                AssistantContentBlock::RedactedThinking(r) => {
                    assert_eq!(r.data.as_deref(), Some("opaque-blob"));
                }
                other => panic!("expected RedactedThinking, got {other:?}"),
            }
        }
    }

    /// Assistant-side `generated_image` content blocks must decode
    /// into the typed variant with all fields preserved so a renderer can
    /// surface the saved path + revised prompt after a transcript reload.
    /// Exercises every optional field path.
    #[test]
    fn assistant_generated_image_block_round_trips() {
        let fixture = json!({
            "type": "assistant",
            "uuid": "a-img-1",
            "timestamp": "2026-04-07T12:34:56.000Z",
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "type": "generated_image",
                        "id": "ig_abc",
                        "status": "completed",
                        "revised_prompt": "a tranquil koi pond at dawn",
                        "media_type": "image/png",
                        "data": "",
                        "saved_path": "C:/tmp/rebon/generated_images/sess1/ig_abc.png"
                    }
                ]
            }
        });
        let msg: Message = serde_json::from_value(fixture.clone()).unwrap();
        if let Message::Assistant(a) = &msg {
            match &a.message.content[0] {
                AssistantContentBlock::GeneratedImage(gi) => {
                    assert_eq!(gi.id, "ig_abc");
                    assert_eq!(gi.status.as_deref(), Some("completed"));
                    assert_eq!(
                        gi.revised_prompt.as_deref(),
                        Some("a tranquil koi pond at dawn")
                    );
                    assert_eq!(gi.media_type, "image/png");
                    assert_eq!(gi.data.as_deref(), Some(""));
                    assert_eq!(
                        gi.saved_path.as_deref(),
                        Some("C:/tmp/rebon/generated_images/sess1/ig_abc.png")
                    );
                }
                other => panic!("expected GeneratedImage, got {other:?}"),
            }
        } else {
            panic!("expected Assistant");
        }

        // Round-trip: the serialized form must retain the non-None
        // fields so the transcript is durable.
        let reserialized = serde_json::to_value(&msg).unwrap();
        let round: Message = serde_json::from_value(reserialized).unwrap();
        if let Message::Assistant(a) = round {
            match &a.message.content[0] {
                AssistantContentBlock::GeneratedImage(gi) => {
                    assert_eq!(gi.id, "ig_abc");
                    assert_eq!(
                        gi.saved_path.as_deref(),
                        Some("C:/tmp/rebon/generated_images/sess1/ig_abc.png")
                    );
                }
                other => panic!("expected GeneratedImage after round-trip, got {other:?}"),
            }
        }
    }

    /// A `generated_image` block without `saved_path` / `status` /
    /// `revised_prompt` — e.g. the raw stream snapshot before
    /// persistence runs — must still decode and defer those fields
    /// to `None` so legacy or partial transcripts don't break.
    #[test]
    fn assistant_generated_image_block_accepts_minimal_shape() {
        let fixture = json!({
            "type": "assistant",
            "uuid": "a-img-2",
            "timestamp": "2026-04-07T12:34:56.000Z",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "generated_image", "id": "ig_min", "data": "iVBORw0=" }
                ]
            }
        });
        let msg: Message = serde_json::from_value(fixture).unwrap();
        if let Message::Assistant(a) = msg {
            match &a.message.content[0] {
                AssistantContentBlock::GeneratedImage(gi) => {
                    assert_eq!(gi.id, "ig_min");
                    assert!(gi.status.is_none());
                    assert!(gi.revised_prompt.is_none());
                    assert!(gi.saved_path.is_none());
                    assert_eq!(gi.media_type, "image/png");
                    assert_eq!(gi.data.as_deref(), Some("iVBORw0="));
                }
                other => panic!("expected GeneratedImage, got {other:?}"),
            }
        }
    }

    /// Unknown assistant content block variants (`server_tool_use`,
    /// `advisor_tool_result`, `connector_text`, future types) must
    /// not blow up — they fall through to the open-world `Other`
    /// variant so transcript replay stays resilient.
    #[test]
    fn unknown_assistant_content_block_type_falls_to_other() {
        let fixture = json!({
            "type": "assistant",
            "uuid": "a-5",
            "timestamp": "2026-04-07T12:34:56.000Z",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "server_tool_use", "id": "srv_1", "name": "WebSearch", "input": {} }
                ]
            }
        });
        let msg: Message = serde_json::from_value(fixture).unwrap();
        if let Message::Assistant(a) = msg {
            assert!(matches!(a.message.content[0], AssistantContentBlock::Other));
        }
    }

    #[test]
    fn user_is_compact_summary_field_is_preserved() {
        // The renderer branches on `isCompactSummary` to route to
        // CompactSummary. Must round-trip.
        let fixture = json!({
            "type": "user",
            "uuid": "u-4",
            "timestamp": "2026-04-07T12:34:56.000Z",
            "isCompactSummary": true,
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "compact summary" }]
            }
        });
        let msg: Message = serde_json::from_value(fixture.clone()).unwrap();
        if let Message::User(u) = &msg {
            assert_eq!(u.is_compact_summary, Some(true));
        } else {
            panic!("expected User");
        }
        // Re-serialize and verify key preserved.
        let re = serde_json::to_value(&msg).unwrap();
        assert_eq!(re["isCompactSummary"], true);
    }

    #[test]
    fn system_subtype_and_level_round_trip() {
        // Shape for the fields surfaced from a system API error.
        let fixture = json!({
            "type": "system",
            "uuid": "s-1",
            "timestamp": "2026-04-07T12:34:56.000Z",
            "subtype": "api_error",
            "content": "overloaded",
            "level": "error",
            "isMeta": false
        });
        let msg: Message = serde_json::from_value(fixture.clone()).unwrap();
        if let Message::System(s) = &msg {
            assert_eq!(s.subtype, "api_error");
            assert_eq!(s.content.as_deref(), Some("overloaded"));
            assert_eq!(s.level, Some(SystemLevel::Error));
            assert_eq!(s.is_meta, Some(false));
        } else {
            panic!("expected System");
        }
    }

    #[test]
    fn attachment_payload_is_preserved_as_opaque_json() {
        // Attachment rendering has its own rich union elsewhere. These
        // structs keep the inner `attachment` field opaque.
        let fixture = json!({
            "type": "attachment",
            "uuid": "att-1",
            "timestamp": "2026-04-07T12:34:56.000Z",
            "attachment": {
                "type": "hook_success",
                "content": "ok",
                "hookName": "user-script",
                "toolUseID": "toolu_01",
                "hookEvent": "PreToolUse"
            }
        });
        let msg: Message = serde_json::from_value(fixture.clone()).unwrap();
        if let Message::Attachment(a) = &msg {
            assert_eq!(a.uuid, "att-1");
            assert_eq!(a.attachment["type"], "hook_success");
            assert_eq!(a.attachment["toolUseID"], "toolu_01");
        } else {
            panic!("expected Attachment");
        }
        // Round-trip lossless.
        let re = serde_json::to_value(&msg).unwrap();
        assert_eq!(re["attachment"]["toolUseID"], "toolu_01");
    }

    // ---------------------------------------------------------------
    // is_not_empty_message — decision tree
    // Rules pinned by the cases below.
    // ---------------------------------------------------------------

    /// Build a user message with the given content array.
    fn user_with_content(content: serde_json::Value) -> Message {
        serde_json::from_value(json!({
            "type": "user",
            "uuid": "u-0",
            "timestamp": "t",
            "message": {
                "role": "user",
                "content": content
            }
        }))
        .unwrap()
    }

    /// Build an assistant message with the given content array.
    fn asst_with_content(content: serde_json::Value) -> Message {
        serde_json::from_value(json!({
            "type": "assistant",
            "uuid": "a-0",
            "timestamp": "t",
            "message": {
                "role": "assistant",
                "content": content
            }
        }))
        .unwrap()
    }

    /// Build a system message.
    fn system_msg() -> Message {
        serde_json::from_value(json!({
            "type": "system",
            "uuid": "s-0",
            "timestamp": "t",
            "subtype": "api_error",
            "content": "overloaded",
            "level": "error"
        }))
        .unwrap()
    }

    /// Build an attachment message.
    fn attachment_msg() -> Message {
        serde_json::from_value(json!({
            "type": "attachment",
            "uuid": "att-0",
            "timestamp": "t",
            "attachment": { "type": "hook_success", "content": "ok" }
        }))
        .unwrap()
    }

    /// Build a legacy `progress` row, which the Rust side models as
    /// `Message::Unknown`. The first rule of the visibility filter keeps
    /// `progress` rows, and the `Message::Unknown` fallback matches that.
    fn progress_msg() -> Message {
        serde_json::from_value(json!({
            "type": "progress",
            "uuid": "p-0",
            "timestamp": "t",
            "content": "legacy"
        }))
        .unwrap()
    }

    /// The full 19-case decision table. Each row is:
    /// `(label, input, expected, rule-name)`.
    ///
    /// Each row names the rule it exercises; that name is repeated in
    /// the test message, so any future drift points at the exact rule to
    /// audit.
    #[test]
    fn is_not_empty_message_table() {
        struct Case {
            label: &'static str,
            input: Message,
            expected: bool,
            ts_rule: &'static str,
        }
        let cases = vec![
            // Rule 1 — progress / attachment / system → true
            Case {
                label: "attachment always keeps",
                input: attachment_msg(),
                expected: true,
                ts_rule: "type==='attachment'",
            },
            Case {
                label: "system always keeps",
                input: system_msg(),
                expected: true,
                ts_rule: "type==='system'",
            },
            Case {
                label: "progress legacy row keeps (via Unknown fallback)",
                input: progress_msg(),
                expected: true,
                ts_rule: "type==='progress'",
            },
            // Rule 3 — empty array content → false
            Case {
                label: "user empty content array drops",
                input: user_with_content(json!([])),
                expected: false,
                ts_rule: "content.length === 0",
            },
            Case {
                label: "assistant empty content array drops",
                input: asst_with_content(json!([])),
                expected: false,
                ts_rule: "content.length === 0",
            },
            // Rule 4 — multi-block content → true
            Case {
                label: "user two text blocks keeps",
                input: user_with_content(json!([
                    { "type": "text", "text": "a" },
                    { "type": "text", "text": "b" }
                ])),
                expected: true,
                ts_rule: "content.length > 1",
            },
            Case {
                label: "assistant text + tool_use keeps",
                input: asst_with_content(json!([
                    { "type": "text", "text": "Let me run this." },
                    { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {} }
                ])),
                expected: true,
                ts_rule: "content.length > 1",
            },
            // Rule 5 — single non-text block → true
            Case {
                label: "user single tool_result keeps",
                input: user_with_content(json!([
                    { "type": "tool_result", "tool_use_id": "t1", "content": "out" }
                ])),
                expected: true,
                ts_rule: "content[0].type !== 'text'",
            },
            Case {
                label: "user single image keeps",
                input: user_with_content(json!([
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "..." } }
                ])),
                expected: true,
                ts_rule: "content[0].type !== 'text'",
            },
            Case {
                label: "assistant single thinking keeps",
                input: asst_with_content(json!([
                    { "type": "thinking", "thinking": "reasoning" }
                ])),
                expected: true,
                ts_rule: "content[0].type !== 'text'",
            },
            Case {
                label: "assistant single tool_use keeps",
                input: asst_with_content(json!([
                    { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {} }
                ])),
                expected: true,
                ts_rule: "content[0].type !== 'text'",
            },
            // Rule 6 — single text block → conditional
            Case {
                label: "user empty text drops",
                input: user_with_content(json!([
                    { "type": "text", "text": "" }
                ])),
                expected: false,
                ts_rule: "trim().length > 0",
            },
            Case {
                label: "user whitespace-only text drops",
                input: user_with_content(json!([
                    { "type": "text", "text": "   \n\t  " }
                ])),
                expected: false,
                ts_rule: "trim().length > 0",
            },
            Case {
                label: "user NO_CONTENT_MESSAGE drops",
                input: user_with_content(json!([
                    { "type": "text", "text": "(no content)" }
                ])),
                expected: false,
                ts_rule: "!== NO_CONTENT_MESSAGE",
            },
            Case {
                label: "user INTERRUPT_MESSAGE_FOR_TOOL_USE drops",
                input: user_with_content(json!([
                    { "type": "text", "text": "[Request interrupted by user for tool use]" }
                ])),
                expected: false,
                ts_rule: "!== INTERRUPT_MESSAGE_FOR_TOOL_USE",
            },
            Case {
                // ASYMMETRY — plain INTERRUPT_MESSAGE is NOT in the
                // filter list. Only the
                // for-tool-use variant is. This case pins that
                // asymmetry; a future refactor that "simplifies" the
                // check by filtering both would break the test.
                label: "user plain INTERRUPT_MESSAGE keeps (asymmetry vs for-tool-use)",
                input: user_with_content(json!([
                    { "type": "text", "text": "[Request interrupted by user]" }
                ])),
                expected: true,
                ts_rule: "plain interrupt NOT in filter",
            },
            Case {
                label: "user normal text keeps",
                input: user_with_content(json!([
                    { "type": "text", "text": "hello world" }
                ])),
                expected: true,
                ts_rule: "trim().length > 0",
            },
            Case {
                label: "assistant empty text drops",
                input: asst_with_content(json!([
                    { "type": "text", "text": "" }
                ])),
                expected: false,
                ts_rule: "trim().length > 0",
            },
            Case {
                label: "assistant NO_CONTENT_MESSAGE drops",
                input: asst_with_content(json!([
                    { "type": "text", "text": "(no content)" }
                ])),
                expected: false,
                ts_rule: "!== NO_CONTENT_MESSAGE",
            },
        ];

        let mut failures: Vec<String> = Vec::new();
        for case in &cases {
            let got = is_not_empty_message(&case.input);
            if got != case.expected {
                failures.push(format!(
                    "case {:?} failed: expected {} got {} (original rule: {})",
                    case.label, case.expected, got, case.ts_rule
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "is_not_empty_message drift:\n{}",
            failures.join("\n")
        );
    }

    /// Pinning the plain `INTERRUPT_MESSAGE` vs
    /// `INTERRUPT_MESSAGE_FOR_TOOL_USE` asymmetry on its own — the
    /// load-bearing detail that distinguishes the two interrupt
    /// literals. A reviewer should see this as a standalone test
    /// that's obviously about the asymmetry, not hidden inside a
    /// 19-row table.
    #[test]
    fn is_not_empty_interrupt_message_asymmetry() {
        // Only INTERRUPT_MESSAGE_FOR_TOOL_USE is in the filter list,
        // NOT plain INTERRUPT_MESSAGE.
        let plain = user_with_content(json!([
            { "type": "text", "text": "[Request interrupted by user]" }
        ]));
        let for_tool = user_with_content(json!([
            { "type": "text", "text": "[Request interrupted by user for tool use]" }
        ]));
        assert!(
            is_not_empty_message(&plain),
            "plain INTERRUPT_MESSAGE must keep — not in the filter list"
        );
        assert!(
            !is_not_empty_message(&for_tool),
            "INTERRUPT_MESSAGE_FOR_TOOL_USE must drop"
        );
    }

    #[test]
    fn tool_result_id_correlates_with_tool_use_id() {
        // Pairs an assistant tool_use with a user tool_result by matching
        // the tool-use id against the result's tool_use_id.
        let asst = serde_json::from_value::<Message>(json!({
            "type": "assistant",
            "uuid": "a-6",
            "timestamp": "t",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "toolu_42", "name": "Read", "input": {} }
                ]
            }
        }))
        .unwrap();
        let usr = serde_json::from_value::<Message>(json!({
            "type": "user",
            "uuid": "u-6",
            "timestamp": "t",
            "message": {
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_42", "content": "file body" }
                ]
            }
        }))
        .unwrap();

        assert_eq!(asst.tool_use_ids(), vec!["toolu_42"]);
        assert_eq!(usr.tool_result_id(), Some("toolu_42"));
        // Identity check: the correlation key is the same across the two messages.
        assert_eq!(asst.tool_use_ids().first().copied(), usr.tool_result_id());
    }
    #[test]
    fn a_row_that_omits_its_role_still_parses_and_a_wrong_one_still_does_not() {
        let asst = serde_json::from_value::<Message>(json!({
            "type": "assistant",
            "uuid": "a-7",
            "timestamp": "t",
            "message": { "content": [{ "type": "text", "text": "hi" }] }
        }))
        .expect("a missing role is the one the row type already declares");
        let Message::Assistant(asst) = asst else {
            panic!("expected an assistant row");
        };
        assert_eq!(asst.message.role, AssistantRole::Assistant);

        let usr = serde_json::from_value::<Message>(json!({
            "type": "user",
            "uuid": "u-7",
            "timestamp": "t",
            "message": { "content": [{ "type": "text", "text": "hi" }] }
        }))
        .expect("same on the user side");
        let Message::User(usr) = usr else {
            panic!("expected a user row");
        };
        assert_eq!(usr.message.role, UserRole::User);

        // A role that contradicts the row type is still a parse failure: the
        // tolerance is for an absent field, not a wrong one. `Message` has an
        // open-world fallback, so the row lands on `Unknown` rather than
        // erroring — either way it is not read as a user row.
        let wrong = serde_json::from_value::<Message>(json!({
            "type": "user",
            "uuid": "u-8",
            "timestamp": "t",
            "message": { "role": "assistant", "content": [] }
        }));
        assert!(!matches!(wrong, Ok(Message::User(_))));
    }
}
