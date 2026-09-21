//! Ratatui renderer for transcript and message dispatch.
//!
//! ## Dispatch shape
//!
//! `render_message` normalises a `Message` into a projected row and then
//! renders it through `RenderedMessageWidget`, so the rendering
//! decisions live in the Rust projection helpers rather than in a
//! per-message-type widget switch.
//!
//! The outer dispatch is on the `Message` variant:
//!
//! * `Message::User` — dropped from the projection entirely when the row
//!   is a meta message (`is_meta == Some(true)`); those attachment
//!   injections (plan-mode reminders, `date_change`, skill listings) go
//!   to the API but are never shown. Otherwise it projects to
//!   `RmMessageRow::User`, mapping each `UserContentBlock` on its own:
//!   `Text` to a text block, `Image` to an image block with no source
//!   hint, and `ToolResult` through `user_tool_result_content_block`.
//! * `Message::Assistant` — projects to `RmMessageRow::Assistant`,
//!   mapping each `AssistantContentBlock`: `Text`, `Thinking`,
//!   `RedactedThinking`, `ToolUse` and `GeneratedImage` each have their
//!   own arm. `ToolUse` resolves the deferred-tool display name from the
//!   inner `tool_name` argument and carries a summary, an optional diff
//!   and body lines; `GeneratedImage` becomes `Generated image (…)`,
//!   `Saved to: …` and `Prompt: …` lines. `AssistantContentBlock::Other`
//!   has no arm and is dropped.
//! * `Message::System` — projects to `RmMessageRow::System`, mapping the
//!   subtype string to `CompactBoundary`, `MicrocompactBoundary`,
//!   `LocalCommand` or `Other`; the raw subtype is kept alongside.
//! * `Message::Attachment` — projects to `RmMessageRow::Attachment`;
//!   only a `directory` attachment produces an `AttachmentInput`.
//! * `Message::Unknown` — projects to `None`.
//!
//! Every known branch therefore has a dedicated render path, and a
//! message with no projection at all falls back to a single-line
//! affordance — `[attachment]` for an attachment row, `[unknown message
//! type]` for anything else — so review can see it rather than silently
//! dropping content.

use std::collections::HashMap;
use std::path::Path;

mod agent_activity;
/// The animated asterisk the landing screen and the header draw. Public
/// because the terminal binary renders it directly; every other module here
/// is reached through this one's own functions.
pub mod animated_asterisk;
mod async_agent_launch;
pub mod buffer_util;
mod committed_tool;
mod edit_diff;
mod failed_tool_render;
mod file_url;
mod gutter_style;
mod input_summary;
mod math_image;
mod memory_notification;
mod read_image_output;
mod system_message;
mod tab_expansion;
mod text_util;
mod tool_body_lines;
mod tool_format;
mod tool_preview;
mod user_tool_result;
mod workflow_progress;
mod wrap;

use agent_activity::extract_agent_activity_lines;
use async_agent_launch::{
    agent_display_name, async_agent_launch_display, async_agent_launch_header_summary,
    format_agent_token_count,
};
use buffer_util::clear_buffer_area;
use committed_tool::{
    committed_tool_to_streaming, committed_tool_to_streaming_with_terminal_status,
};
use edit_diff::{
    build_context_diff, diff_line_counts, diff_summary_from_counts, extract_diff_content,
    measure_diff_block_height, render_diff_block_at,
};
use failed_tool_render::tool_status_style;
use gutter_style::{tool_gutter_glyph, tool_gutter_style};
use input_summary::{compact_json_map_value_for_tool, compact_json_object};
use math_image::{apply_math_image_layers, markdown_render_options};
pub use math_image::{MathDisplayMode, MathGraphicsProtocol};
use memory_notification::extract_memory_notification;
use read_image_output::{is_read_image_object_output, is_read_image_raw_output};
use rebon_render::plan_ledger::{
    plan_ledger_error_lines, plan_ledger_requirement_lines, plan_ledger_result_from_text,
    plan_ledger_status_line, plan_ledger_summary, PLAN_LEDGER_TOOL_NAME,
};
use text_util::last_non_empty_line;
use tool_body_lines::{
    agent_description, agent_full_detail_lines, collect_shell_tool_body_lines,
    collect_tool_body_lines, render_tool_call_content,
};
use tool_format::{
    compact_json_map, deferred_tool_verbose_summary, streaming_tool_display_name,
    streaming_tool_summary, truncate_param_value, SUMMARY_MAX_CHARS,
};
use tool_preview::{
    compact_preview_lines, hard_wrap_text_lines, live_shell_preview_lines,
    normalize_tool_body_lines, tool_content_preview_lines, visual_preview_lines,
    TOOL_PREVIEW_MAX_LINES,
};
use user_tool_result::user_tool_result_content_block;
use workflow_progress::{render_workflow_tool, workflow_tool_summary};
pub use wrap::wrap_height;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use rebon_design_system::shortcut_hint::format_shortcut_hint;
use rebon_design_system::theme::{Theme, ThemeName};
use rebon_message_tui::{MessageRenderTheme, RenderedMessageWidget};
use rebon_render::{
    classify_tool_use, project_collapsed_read_search, Aggregator,
    AssistantContentBlock as RmAssistantContentBlock, AssistantMessage as RmAssistantMessage,
    AttachmentInput, AttachmentMessage as RmAttachmentMessage, ClassifyOptions,
    CollapsedReadSearchDisplay, CollapsedReadSearchProjection, FinalizeParams, MemoryPathPolicy,
    MessageRow as RmMessageRow, RenderMessageInput, ResultStatus, SystemMessage as RmSystemMessage,
    SystemSubtype as RmSystemSubtype, ToolClass, UserContentBlock as RmUserContentBlock,
    UserMessage as RmUserMessage,
};
#[cfg(test)]
use rebon_types::ContentBlock;
use rebon_types::{ToolCallContent, ToolCallLocation, ToolCallStatus, ToolKind};
use rebon_width::WidthStr;
use serde_json::Value;

use crate::measure::MeasureCache;
use crate::message::{
    AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
    AssistantTextBlock, AssistantToolUseBlock, Message, UserContentBlock,
};
use crate::state::AppState;
use crate::streaming::{StreamingOverlay, StreamingThinking, StreamingToolUse};

fn plan_ledger_input<'a>(
    tool_name: &str,
    input: &'a Value,
) -> Option<&'a serde_json::Map<String, Value>> {
    if tool_name == PLAN_LEDGER_TOOL_NAME {
        return input.as_object();
    }
    (tool_name == "InvokeDeferredTool"
        && input.get("tool_name").and_then(Value::as_str) == Some(PLAN_LEDGER_TOOL_NAME))
    .then(|| input.get("arguments").and_then(Value::as_object))
    .flatten()
}

fn plan_ledger_result_from_content(content: Option<&[ToolCallContent]>) -> Option<Value> {
    content?
        .iter()
        .map(render_tool_call_content)
        .find_map(|text| plan_ledger_result_from_text(&text))
}

mod cache;
mod extras;
mod measure_render;
mod message_render;
mod primitives;
mod projection;
mod stream_segments;
mod streaming;
mod theme;
mod tool_cards;
mod tool_helpers;
mod transcript;
mod transcript_segments;

use cache::*;
pub use cache::{ToolOutputVerbosity, TranscriptMeasureCache};
pub use extras::{LiveAgentToolActivity, LiveAgentToolStatus, TranscriptRenderExtras};
use measure_render::*;
pub use message_render::render_message;
use message_render::*;
use primitives::*;
use projection::*;
use stream_segments::*;
pub use streaming::render_streaming_overlay;
use streaming::*;
use theme::*;
pub use theme::{parse_theme_color, RenderTheme};
use tool_cards::*;
use tool_helpers::*;
#[cfg(test)]
use transcript::render_transcript_cached_with_running_hints_at;
pub use transcript::{
    render_transcript, render_transcript_cached, render_transcript_cached_with_running_hints,
    TranscriptRenderResult, TranscriptStickyAnchor,
};
pub use transcript_segments::trailing_collapsible_tool_run_start;
use transcript_segments::*;
#[cfg(test)]
mod tests;
