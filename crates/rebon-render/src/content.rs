//! Render one `ToolCallContent` payload to a display string.
//!
//! This is the per-variant switch every tool body goes through — Diff (summary
//! line), Terminal (handle), and the ACP `ContentBlock` variants (Text / Image /
//! Audio / Resource / ResourceLink) — so no surface ever falls back to a raw
//! JSON blob.

use rebon_types::{ContentBlock, ToolCallContent};

use crate::diff::{diff_summary, generate_diff_lines};
use crate::text::expand_tabs_for_tui;

pub fn render_tool_call_content(content: &ToolCallContent) -> String {
    let raw = match content {
        ToolCallContent::Diff(diff) => {
            let diff_lines = generate_diff_lines(diff.old_text.as_deref(), &diff.new_text);
            let summary = diff_summary(&diff_lines);
            format!("{} — {summary}", diff.path)
        }
        ToolCallContent::Terminal(terminal) => format!("terminal {}", terminal.terminal_id),
        ToolCallContent::Content(regular) => match &regular.content {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Image(_) => "[image]".into(),
            ContentBlock::Audio(_) => "[audio]".into(),
            ContentBlock::Resource(_) => "[resource]".into(),
            ContentBlock::ResourceLink(_) => "[resource link]".into(),
        },
    };
    expand_tabs_for_tui(&raw)
}
