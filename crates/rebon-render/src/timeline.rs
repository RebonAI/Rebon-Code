//! Minimal timeline/message shapes read by the transcript list and row
//! logic ([`crate::row_logic`], [`crate::row_projection`],
//! [`crate::render_plan`], [`crate::messages_memo`]).

use std::collections::BTreeSet;

/// Which screen the transcript list and its rows are drawn on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineScreen {
    /// Prompt / main-screen mode.
    Prompt,
    /// Transcript mode.
    Transcript,
}

/// Minimal lookup tables read by the staticity helpers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TimelineLookups {
    /// Tool-use ids that already have a result.
    pub resolved_tool_use_ids: BTreeSet<String>,
    /// Tool uses that still have unresolved `PostToolUse` hooks.
    pub unresolved_post_tool_use_ids: BTreeSet<String>,
}

/// Top-level renderable row union read by the transcript list and row logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineMessage {
    /// `type: 'user'`
    User(UserTimelineMessage),
    /// `type: 'assistant'`
    Assistant(AssistantTimelineMessage),
    /// `type: 'attachment'`
    Attachment(AttachmentTimelineMessage),
    /// `type: 'system'`
    System(SystemTimelineMessage),
    /// `type: 'grouped_tool_use'`
    GroupedToolUse(GroupedToolUseTimelineMessage),
    /// `type: 'collapsed_read_search'`
    CollapsedReadSearch(CollapsedReadSearchTimelineMessage),
}

impl TimelineMessage {
    /// Stable row UUID.
    pub fn uuid(&self) -> &str {
        match self {
            Self::User(message) => &message.uuid,
            Self::Assistant(message) => &message.uuid,
            Self::Attachment(message) => &message.uuid,
            Self::System(message) => &message.uuid,
            Self::GroupedToolUse(message) => &message.uuid,
            Self::CollapsedReadSearch(message) => &message.uuid,
        }
    }

    /// The first content block.
    pub fn first_block(&self) -> Option<&TimelineContentBlock> {
        match self {
            Self::User(message) => message.content.first(),
            Self::Assistant(message) => message.content.first(),
            Self::Attachment(_)
            | Self::System(_)
            | Self::GroupedToolUse(_)
            | Self::CollapsedReadSearch(_) => None,
        }
    }

    /// The tool-use id a row belongs to: an attachment's own id, the leading
    /// tool use or server tool use of an assistant row, a user row's source
    /// tool-use id or leading tool result, and an informational system row's
    /// id. Grouped and collapsed rows have none.
    pub fn tool_use_id(&self) -> Option<&str> {
        match self {
            Self::Attachment(message) => message.tool_use_id.as_deref(),
            Self::Assistant(message) => match message.content.first() {
                Some(TimelineContentBlock::ToolUse(block)) => Some(block.id.as_str()),
                Some(TimelineContentBlock::ServerToolUse(block)) => Some(block.id.as_str()),
                _ => None,
            },
            Self::User(message) => {
                if let Some(source_tool_use_id) = message.source_tool_use_id.as_deref() {
                    return Some(source_tool_use_id);
                }
                match message.content.first() {
                    Some(TimelineContentBlock::ToolResult(block)) => block.tool_use_id.as_deref(),
                    _ => None,
                }
            }
            Self::System(message) => match message.subtype {
                TimelineSystemSubtype::Informational => message.tool_use_id.as_deref(),
                TimelineSystemSubtype::ApiError
                | TimelineSystemSubtype::ApiMetrics
                | TimelineSystemSubtype::Other => None,
            },
            Self::GroupedToolUse(_) | Self::CollapsedReadSearch(_) => None,
        }
    }
}

/// User-message fields that the row logic reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTimelineMessage {
    /// Row UUID.
    pub uuid: String,
    /// `isMeta`
    pub is_meta: bool,
    /// Id of the tool use this row was produced for, if any.
    pub source_tool_use_id: Option<String>,
    /// `message.content`
    pub content: Vec<TimelineContentBlock>,
}

/// Assistant-message fields that the row logic reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantTimelineMessage {
    /// Row UUID.
    pub uuid: String,
    /// `isApiErrorMessage`
    pub is_api_error_message: bool,
    /// `timestamp`
    pub timestamp: Option<String>,
    /// `message.model`
    pub model: Option<String>,
    /// `message.content`
    pub content: Vec<TimelineContentBlock>,
}

impl AssistantTimelineMessage {
    /// True when any content block is a text block.
    pub fn has_text_block(&self) -> bool {
        self.content
            .iter()
            .any(|block| matches!(block, TimelineContentBlock::Text(_)))
    }
}

/// Opaque attachment row shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentTimelineMessage {
    /// Row UUID.
    pub uuid: String,
    /// `attachment.type`
    pub attachment_type: String,
    /// `attachment.isMeta`
    pub attachment_is_meta: bool,
    /// The attachment's command mode, if any.
    pub command_mode: Option<String>,
    /// Whether the attachment has a defined `origin`.
    pub has_origin: bool,
    /// Hook attachment `toolUseID`, when present.
    pub tool_use_id: Option<String>,
}

/// System row shape used by the row logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemTimelineMessage {
    /// Row UUID.
    pub uuid: String,
    /// `subtype`
    pub subtype: TimelineSystemSubtype,
    /// Optional `toolUseID` for informational rows.
    pub tool_use_id: Option<String>,
}

/// System subtypes that matter to the row logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineSystemSubtype {
    /// `api_error`
    ApiError,
    /// `api_metrics`
    ApiMetrics,
    /// `informational`
    Informational,
    /// Any other subtype.
    Other,
}

/// `grouped_tool_use` row shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupedToolUseTimelineMessage {
    /// Row UUID.
    pub uuid: String,
    /// `toolName`
    pub tool_name: String,
    /// Grouped assistant tool-use messages.
    pub messages: Vec<AssistantTimelineMessage>,
}

/// `collapsed_read_search` row shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedReadSearchTimelineMessage {
    /// Row UUID.
    pub uuid: String,
    /// Flattened `tool_use` IDs in the collapsed group.
    pub tool_use_ids: Vec<String>,
}

/// The subset of content blocks the row logic reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineContentBlock {
    /// `type: 'text'`
    Text(TextBlock),
    /// `type: 'tool_use'`
    ToolUse(ToolUseBlock),
    /// `type: 'tool_result'`
    ToolResult(ToolResultBlock),
    /// `type: 'thinking'`
    Thinking,
    /// `type: 'redacted_thinking'`
    RedactedThinking,
    /// `type: 'server_tool_use'`
    ServerToolUse(ServerToolUseBlock),
    /// Any other block not read by these helpers.
    Other,
}

/// `text` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBlock {
    /// `text`
    pub text: String,
}

/// `tool_use` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseBlock {
    /// `id`
    pub id: String,
    /// `name`
    pub name: Option<String>,
    /// Whether the tool is a search or read that may fold into a collapsed
    /// group, precomputed by the caller.
    pub is_collapsible: bool,
}

/// `tool_result` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultBlock {
    /// `tool_use_id`
    pub tool_use_id: Option<String>,
}

/// `server_tool_use` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerToolUseBlock {
    /// `id`
    pub id: String,
}
