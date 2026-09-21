//! # rebon-message-tui — the ratatui half of the message stack
//!
//! Takes the display state [`rebon_render`] projects and paints it: markdown
//! into styled `Text`, message rows into `Line`s, and the typed widget
//! subtree (`AttachmentBodyWidget`, `AssistantTextBodyWidget` and friends)
//! into real row/column `Layout` splits with bordered blocks.
//!
//! The split with [`rebon_render`] is one-way and load-bearing: everything a
//! non-terminal front-end could also want is a projection and lives over
//! there, so a front-end with no terminal never pulls this crate in. A type
//! that grows a `ratatui::` field belongs on this side; one that does not
//! belongs on the other.
//!
//! ## What is here
//!
//! * [`markdown_render`] — markdown to `Text` with themes, hyperlink ranges,
//!   and inline formula bitmaps.
//! * [`streaming_markdown`] — the committed/live split for a message still
//!   arriving, so a fence opened in the stable prefix is closed and reopened
//!   rather than inverting code and prose styling.
//! * [`render`] — the message row itself: metadata lines, the two-column
//!   side-by-side file-edit diff (removals left, additions right, paired
//!   row-wise across a U+2502 divider), and the fallback branches.
//! * [`projection_render`] — the per-projection formatters (assistant text,
//!   thinking, tool use, attachments, collapsed read/search, plan approvals,
//!   teammate messages) and the theme they read.
//! * [`widget`] — widget-level layout composition and the hyperlink paint
//!   layer.
//! * [`widget_subtree`] — typed body widgets. Every bordered block carries a
//!   right-aligned `title_bottom` status (`+N -M`, `N lines`, a summary).
//!
//! ## Not implemented here
//!
//! * any painting beyond the projections listed above
//! * the rest of the runtime wiring — nothing here reads the session,
//!   the engine, or an event stream
//! * richer tool/lookup/command rendering payloads

#![deny(missing_docs)]

pub mod markdown_render;
pub mod projection_render;
pub mod render;
pub mod streaming_markdown;
pub mod widget;
pub mod widget_subtree;

pub use markdown_render::{
    render_markdown, render_markdown_annotated, render_markdown_annotated_with_options,
    render_markdown_annotated_with_width, render_markdown_annotated_with_width_and_options,
    render_markdown_blocks, render_markdown_blocks_annotated,
    render_markdown_blocks_annotated_with_options, render_markdown_blocks_annotated_with_width,
    render_markdown_blocks_annotated_with_width_and_options, render_markdown_blocks_with_options,
    render_markdown_blocks_with_width, render_markdown_blocks_with_width_and_options,
    render_markdown_with_options, render_markdown_with_width,
    render_markdown_with_width_and_options, FormulaAsset, FormulaBitmap, FormulaDisplayMode,
    FormulaRange, HyperlinkRange, MarkdownRenderOptions, MarkdownTheme, RenderedFormula,
    RenderedMarkdown, BLOCKQUOTE_BAR, DEFAULT_MARKDOWN_TERMINAL_WIDTH, MAX_FORMULA_BITMAP_PIXELS,
    MAX_FORMULA_SOURCE_BYTES, MAX_FORMULA_TERMINAL_ROWS,
};
pub use projection_render::{
    fold_separator_style, parse_theme_color, render_assistant_text_projection,
    render_assistant_thinking_projection, render_assistant_tool_use_projection,
    render_attachment_projection, render_compact_summary_display,
    render_highlighted_thinking_projection, render_system_text_projection,
    render_user_text_projection, render_user_tool_result_projection, MessagesRenderTheme,
};
pub use render::{
    render_fallback_tool_use_error, render_file_edit_rejected, render_file_edit_updated,
    render_message, render_metadata_line, render_notebook_edit_rejected, MessageRenderTheme,
};
pub use streaming_markdown::{
    cmark_block_lex, render_streaming_message, render_streaming_message_with_options,
    StreamingMarkdownRenderer, StreamingSplit,
};
pub use widget::{HyperlinkPaintLayer, MessageBorderDecoration, RenderedMessageWidget};
pub use widget_subtree::{
    accent_theme as attachment_accent_theme, child_theme_for as widget_child_theme,
    diff_lines_to_text, render_inline_diff, spinner_glyph, AssistantTextBodyWidget,
    AssistantThinkingBodyWidget, AttachmentBodyWidget, FallbackBodyWidget, FileEditBodyKind,
    FileEditBodyWidget, MessageBodyKind, SystemTextBodyWidget, UserTextBodyWidget,
    UserTextPlanBodyWidget, UserToolResultBodyWidget, SPINNER_FRAMES,
};

#[cfg(test)]
mod compatibility {
    /// Dependency canary — this crate is the ratatui side, so it may name a
    /// terminal framework, but it must not grow a `rebon-*` dependency
    /// without an explicit decision. `rebon-render` is the projection it
    /// paints; the other four are the primitives that painting needs (width
    /// policy, theme colors, shell output formatting, offline formula
    /// rasterization).
    const ALLOWED: &[&str] = &[
        "rebon-render",
        "rebon-width",
        "rebon-design-system",
        "rebon-shell",
        "rebon-math",
    ];

    #[test]
    fn only_approved_rebon_deps_are_allowed_in_cargo_toml() {
        let cargo = include_str!("../Cargo.toml");
        for line in cargo.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            if trimmed.starts_with("rebon-") {
                assert!(
                    ALLOWED.iter().any(|allowed| trimmed.starts_with(allowed)),
                    "rebon-message-tui allows only {ALLOWED:?}; found: {line}"
                );
            }
        }
    }
}
