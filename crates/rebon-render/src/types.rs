//! Core data shapes shared with the message renderer, and the parts of the
//! message union the dispatcher reads.

use crate::{AttachmentInput, StopHookSummaryDisplay};

/// The top-level row union the renderer dispatches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRow {
    /// `type: 'user'`
    User(UserMessage),
    /// `type: 'assistant'`
    Assistant(AssistantMessage),
    /// `type: 'attachment'`
    Attachment(AttachmentMessage),
    /// `type: 'system'`
    System(SystemMessage),
}

impl MessageRow {
    /// Stable row uuid used by the memo comparator and latest-bash gate.
    pub fn uuid(&self) -> &str {
        match self {
            Self::User(m) => &m.uuid,
            Self::Assistant(m) => &m.uuid,
            Self::Attachment(m) => &m.uuid,
            Self::System(m) => &m.uuid,
        }
    }
}

/// Minimal user-message shape the renderer reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessage {
    /// Message uuid.
    pub uuid: String,
    /// `isCompactSummary` branch gate.
    pub is_compact_summary: bool,
    /// The message's content block array, iterated when the row is drawn.
    pub content: Vec<UserContentBlock>,
    /// Optional `imagePasteIds`.
    pub image_paste_ids: Vec<Option<String>>,
    /// Optional plan override, passed only to the user-text row.
    pub plan_content: Option<String>,
    /// Optional timestamp, passed only to the user-text row.
    pub timestamp: Option<String>,
}

/// Minimal assistant-message shape the renderer reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantMessage {
    /// Message uuid.
    pub uuid: String,
    /// The message's content block array, iterated when the row is drawn.
    pub content: Vec<AssistantContentBlock>,
    /// Optional `advisorModel`, threaded only to advisor rows.
    pub advisor_model: Option<String>,
    /// True when this row is the visual continuation of the previous
    /// assistant text row (the inline overflow flush split one streaming
    /// text block across scrollback commits). Text blocks render without
    /// the gutter dot so the halves read as a single message; margins are
    /// suppressed by the caller via `add_margin`.
    pub is_stream_continuation: bool,
}

/// Attachment row. The renderer forwards the attachment payload without
/// looking inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMessage {
    /// Row uuid.
    pub uuid: String,
    /// Optional child attachment payload for composition-phase rendering.
    pub attachment: Option<Box<AttachmentInput>>,
}

/// Minimal system row shape used by the outer switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemMessage {
    /// Row uuid.
    pub uuid: String,
    /// `subtype` branch.
    pub subtype: SystemSubtype,
    /// The raw subtype string, kept for subtypes the enum does not cover.
    pub raw_subtype: Option<String>,
    /// Optional level string (`info`, `warning`, `error`).
    pub level: Option<String>,
    /// Text payload, used by `local_command` rows and generic system text.
    pub content: String,
    /// Optional `stop_hook_summary` composition payload picked up by the
    /// widget subtree when present.
    pub stop_hook_summary: Option<Box<StopHookSummaryDisplay>>,
}

/// The system subtypes the renderer distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemSubtype {
    /// `compact_boundary`
    CompactBoundary,
    /// `microcompact_boundary`
    MicrocompactBoundary,
    /// `local_command`
    LocalCommand,
    /// Any other subtype.
    Other,
}

/// User content blocks, switched on by their `type` discriminant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserContentBlock {
    /// `type: 'text'`
    Text {
        /// Text payload.
        text: String,
    },
    /// `type: 'image'`
    Image {
        /// Optional image label/source hint for a fallback rendering path.
        source_hint: Option<String>,
    },
    /// `type: 'tool_result'`
    ToolResult {
        /// Optional `tool_use_id`. Not read by the dispatcher itself, but
        /// needed by rendering and list-level expansion keys.
        tool_use_id: Option<String>,
        /// Optional plain-text body for fallback rendering.
        content: Option<String>,
        /// Optional error flag.
        is_error: bool,
    },
}

/// Assistant content blocks, switched on by their `type` discriminant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantContentBlock {
    /// `type: 'tool_use'`
    ToolUse {
        /// Optional tool-use id.
        id: Option<String>,
        /// Optional tool name.
        name: Option<String>,
        /// Optional compact input summary for fallback rendering.
        input_summary: Option<String>,
        /// Optional diff data for Edit tool results. When present,
        /// the widget renders a colored diff instead of the generic
        /// tool-use summary.
        diff: Option<(String, Option<String>, String)>,
        /// Optional detail rows rendered below the tool header.
        body_lines: Vec<String>,
    },
    /// `type: 'text'`
    Text {
        /// Text payload.
        text: String,
    },
    /// `type: 'redacted_thinking'`
    RedactedThinking {
        /// Opaque redacted payload.
        data: Option<String>,
    },
    /// `type: 'thinking'`
    Thinking {
        /// Thinking body.
        thinking: Option<String>,
    },
    /// Connector-authored text, recognised before the block-type switch.
    ConnectorText {
        /// Connector-authored text rendered as assistant text.
        connector_text: String,
    },
    /// A `server_tool_use` or `advisor_tool_result` block the caller
    /// already classified as an advisor block.
    AdvisorBlock {
        /// The raw block `type` string.
        raw_type: String,
    },
    /// A `server_tool_use` or `advisor_tool_result` block the caller says
    /// is not an advisor block; it is logged and draws nothing.
    NonAdvisorServerBlock {
        /// The raw block `type` string.
        raw_type: String,
    },
    /// Any other unrenderable assistant block.
    Unknown {
        /// The raw block `type` string.
        raw_type: String,
    },
}

/// Input for the top-level row dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderMessageInput {
    /// Row being projected.
    pub message: MessageRow,
    /// Width of the container the row is drawn into, when known.
    pub container_width: Option<u16>,
    /// Whether the standard top margin is added above the row.
    pub add_margin: bool,
    /// Whether the verbose view is on.
    pub verbose: bool,
    /// Whether the condensed style is in use.
    pub style_condensed: bool,
    /// Whether the transcript view, rather than the compact view, is active.
    pub is_transcript_mode: bool,
    /// Whether this row is the active member of a collapsed group.
    pub is_active_collapsed_group: bool,
    /// Whether this row continues the previous user message.
    pub is_user_continuation: bool,
    /// Id of the last thinking block, used to place the thinking footer.
    pub last_thinking_block_id: Option<String>,
    /// Uuid of the most recent Bash output row, used by the live-output gate.
    pub latest_bash_output_uuid: Option<String>,
    /// Terminal column count, needed for tool-result width.
    pub terminal_columns: u16,
    /// Whether the fullscreen environment flag is set.
    pub fullscreen_env_enabled: bool,
    /// Render thinking as a compact first-line markdown title instead of the
    /// full body.
    pub compact_thinking_preview: bool,
    /// Show the `Ctrl+O to expand` hint on compact thinking title rows.
    pub show_thinking_expand_hint: bool,
    /// Current animation clock used by streaming tool indicators.
    pub frame_time_ms: u64,
    /// Tool-use ids that should be rendered as active/pending.
    pub in_progress_tool_use_ids: Vec<String>,
    /// Tool-use ids that should be rendered as failed.
    pub errored_tool_use_ids: Vec<String>,
    /// Show trailing compact-mode tool expansion hint.
    pub show_tool_expand_hint: bool,
}
