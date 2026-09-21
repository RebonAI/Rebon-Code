//! Typed widget subtree for a message row.
//!
//! This module extends the flat `Paragraph<Text>` body used by
//! [`crate::widget::RenderedMessageWidget`] with richer per-row widgets that
//! paint directly into a `ratatui::buffer::Buffer` via real
//! `Layout::horizontal` / `Layout::vertical` splits. The goal is to move
//! visual concerns (gutters, indented sub-rows, status badges, colored
//! headers) off the text-projection path and into a dedicated widget
//! subtree so composition callers can reason about layout without having
//! to reverse-engineer the text output.
//!
//! The module imports `ratatui`, `rebon-render`, `rebon-design-system`,
//! and the in-crate render helpers.

use crate::projection_render::{
    fold_separator_style, parse_theme_color, render_assistant_text_projection,
    render_assistant_thinking_projection, render_user_text_projection, MessagesRenderTheme,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Widget, Wrap},
};
use rebon_design_system::{format_shortcut_hint, theme};
use rebon_render::ask_user_answers::AnsweredQuestion;
use rebon_render::{
    calculate_word_diff, format_diff_lines, FormatOptions, LineColor, LineSegment as DiffSegment,
    RenderedLine, WordColor,
};
use rebon_render::{
    format_user_prompt_hidden_separator, project_assistant_text_message,
    project_assistant_thinking_message, project_assistant_tool_use, project_attachment_message,
    project_user_prompt_display_lines, project_user_text, project_user_tool_result,
    AssistantMarkdownDisplay, AssistantTextMessageInput, AssistantTextMessageProjection,
    AssistantThinkingMessageInput, AssistantThinkingMessageProjection, AssistantToolDefinition,
    AssistantToolRenderOutputs, AssistantToolSecondaryDisplay, AssistantToolUseInput,
    AssistantToolUseInvocation, AssistantToolUseProjection, AttachmentInput, AttachmentLineDisplay,
    AttachmentMessageInput, AttachmentProjection, AttachmentRelevantMemoriesDisplay,
    AttachmentTaskStatusDisplay, AttachmentTeammateMailboxDisplay,
    AttachmentTeammateMailboxItemDisplay, AttachmentTone, DiagnosticsProjection,
    PlanApprovalRenderable, PlanApprovalResponseDisplay, StopHookSummaryDisplay,
    SystemTextProjection, SystemVisualMarker, TeammateMessageContentDisplay, UserPromptDisplayLine,
    UserTextInput, UserTextProjection, UserToolErrorProjection, UserToolResultInput,
    UserToolResultProjection,
};

use rebon_render::UserToolSuccessProjection;
use rebon_shell::{
    parse_bash_tool_result_json, project_bash_tool_result_message, BashToolResultBlock,
    BashToolResultInput, ExpandShellOutputContextValue, ParsedBashToolResult,
    EMPTY_OUTPUT_PLACEHOLDER, IMAGE_PLACEHOLDER,
};

use rebon_render::{
    fallback::FallbackToolUseErrorProjection,
    file_edit::{
        fold_long_diff_runs, FileEditRejectedProjection, FileEditUpdatedProjection,
        NotebookEditRejectedProjection, StructuredPatchHunk,
    },
};

mod layout;
mod shared;

pub use layout::GUTTER_WIDTH;
use layout::{
    gutter_label, line_row_height, measure_text_height, paint_line_into, paint_text_into,
    split_gutter,
};
pub(crate) use shared::assistant_text_child_theme_for;
pub use shared::{accent_theme, child_theme_for};
use shared::{bordered_text_block, BodyRow, BorderedBlock, BorderedBlockBody, RuledBlock};

/// Spinner glyphs cycled through when an animated row is unresolved.
pub const SPINNER_FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

/// Pick the spinner glyph for the given frame counter.
pub fn spinner_glyph(frame_counter: u64) -> &'static str {
    SPINNER_FRAMES[(frame_counter as usize) % SPINNER_FRAMES.len()]
}

fn tool_name_style(theme: &MessagesRenderTheme) -> Style {
    theme.accent.add_modifier(Modifier::BOLD)
}

/// The body half of [`crate::widget::RenderedMessageWidget`], now either a
/// flat text body (the input `Paragraph<Text>` path) or a typed sub-widget
/// that paints itself with real layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageBodyKind {
    /// Hidden body that consumes no transcript rows.
    Hidden,
    /// Flat text body rendered via `Paragraph<Text>` (fallback path).
    Text(Text<'static>),
    /// Single assistant-text body with one guttered child projection.
    AssistantText(AssistantTextBodyWidget),
    /// Single assistant-thinking body with transcript/expand-hint layout.
    AssistantThinking(AssistantThinkingBodyWidget),
    /// Single assistant tool-use body with richer loader/secondary layout.
    AssistantToolUse(AssistantToolUseBodyWidget),
    /// Attachment body, painted with explicit gutter/indent layout.
    Attachment(AttachmentBodyWidget),
    /// File-edit body (updated / rejected / notebook-rejected) painted with a
    /// bordered diff block.
    FileEdit(FileEditBodyWidget),
    /// Fallback tool-use error body with a bordered error block.
    Fallback(FallbackBodyWidget),
    /// System stop-hook-summary body with a bordered block around the
    /// prevented/error/detail lines.
    SystemText(SystemTextBodyWidget),
    /// User plain-text body with gutter/content split for proper wrapping.
    UserText(UserTextBodyWidget),
    /// User plan-content body with a ruled "Plan to implement" block.
    UserTextPlan(UserTextPlanBodyWidget),
    /// Answered-`AskUserQuestion` body with a ruled question/answer card.
    UserAskAnswers(UserAskAnswersBodyWidget),
    /// User tool-result body with structured success/error layouts.
    UserToolResult(UserToolResultBodyWidget),
    /// Composite body of multiple individually-guttered blocks, used for
    /// multi-block messages so each block retains gutter/content split.
    CompositeBlocks(Vec<MessageBodyKind>),
}

impl MessageBodyKind {
    /// Height in rows at the given width.
    pub fn height(&self, width: u16) -> u16 {
        match self {
            Self::Hidden => 0,
            Self::Text(text) => measure_text_height(text, width).max(1),
            Self::AssistantText(widget) => widget.height(width),
            Self::AssistantThinking(widget) => widget.height(width),
            Self::AssistantToolUse(widget) => widget.height(width),
            Self::Attachment(widget) => widget.height(width),
            Self::FileEdit(widget) => widget.height(width),
            Self::Fallback(widget) => widget.height(width),
            Self::SystemText(widget) => widget.height(width),
            Self::UserText(widget) => widget.height(width),
            Self::UserTextPlan(widget) => widget.height(width),
            Self::UserAskAnswers(widget) => widget.height(width),
            Self::UserToolResult(widget) => widget.height(width),
            Self::CompositeBlocks(blocks) => blocks.iter().map(|b| b.height(width)).sum(),
        }
    }

    /// Paint the body into `area` on `buf`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let mut discarded_layers = Vec::new();
        self.render_to_buffer_with_hyperlinks(area, buf, &mut discarded_layers);
    }

    pub(crate) fn render_to_buffer_with_hyperlinks(
        self,
        area: Rect,
        buf: &mut Buffer,
        layers: &mut Vec<crate::widget::HyperlinkPaintLayer>,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        match self {
            Self::Hidden => {}
            Self::Text(text) => {
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .render(area, buf);
            }
            Self::AssistantText(widget) => {
                widget.render_to_buffer_with_hyperlinks(area, buf, layers)
            }
            Self::AssistantThinking(widget) => widget.render_to_buffer(area, buf),
            Self::AssistantToolUse(widget) => widget.render_to_buffer(area, buf),
            Self::Attachment(widget) => widget.render_to_buffer(area, buf),
            Self::FileEdit(widget) => widget.render_to_buffer(area, buf),
            Self::Fallback(widget) => widget.render_to_buffer(area, buf),
            Self::SystemText(widget) => widget.render_to_buffer(area, buf),
            Self::UserText(widget) => widget.render_to_buffer(area, buf),
            Self::UserTextPlan(widget) => widget.render_to_buffer(area, buf),
            Self::UserAskAnswers(widget) => widget.render_to_buffer(area, buf),
            Self::UserToolResult(widget) => widget.render_to_buffer(area, buf),
            Self::CompositeBlocks(blocks) => {
                let mut y = area.y;
                let bottom = area.y.saturating_add(area.height);
                for block in blocks {
                    if y >= bottom {
                        break;
                    }
                    let h = block.height(area.width);
                    let sub = Rect {
                        x: area.x,
                        y,
                        width: area.width,
                        height: h.min(bottom.saturating_sub(y)),
                    };
                    block.render_to_buffer_with_hyperlinks(sub, buf, layers);
                    y = y.saturating_add(h);
                }
            }
        }
    }
}

/// Typed attachment body widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentBodyWidget {
    /// Owned projection — either an already-projected value or one we compute
    /// from an `AttachmentInput` on construction.
    pub projection: AttachmentProjection,
    /// Owned theme (copied from the parent message theme).
    pub theme: MessagesRenderTheme,
    /// Gutter prefix label such as `"ATT"`.
    pub prefix: &'static str,
    /// Gutter label style.
    pub label_style: Style,
}

impl AttachmentBodyWidget {
    /// Build from a raw attachment input, using the same projection inputs as
    /// the flat text render path.
    pub fn from_input(
        attachment: &AttachmentInput,
        verbose: bool,
        is_transcript_mode: bool,
        add_margin: bool,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        let projection = project_attachment_message(&AttachmentMessageInput {
            add_margin,
            verbose,
            is_transcript_mode,
            background: None,
            dot_glyph: "\u{25CF}".into(),
            path_separator: std::path::MAIN_SEPARATOR_STR.into(),
            attachment: attachment.clone(),
        });
        Self {
            projection,
            theme,
            prefix: attachment_prefix(attachment),
            label_style,
        }
    }

    /// Height in rows at `width`.
    pub fn height(&self, width: u16) -> u16 {
        let rows = attachment_rows(&self.projection, &self.theme);
        if rows.is_empty() {
            return 1;
        }
        let content_width = width.saturating_sub(GUTTER_WIDTH).max(1);
        rows.iter().map(|row| row.height(content_width)).sum()
    }

    /// Paint the widget into `area`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let Self {
            projection,
            theme,
            prefix,
            label_style,
        } = self;
        let rows = attachment_rows(&projection, &theme);
        if rows.is_empty() {
            return;
        }
        let (gutter_area, content_area) = split_gutter(area);
        if gutter_area.width > 0 && gutter_area.height > 0 {
            paint_line_into(
                Rect {
                    x: gutter_area.x,
                    y: gutter_area.y,
                    width: gutter_area.width,
                    height: 1,
                },
                buf,
                gutter_label(prefix, label_style),
            );
        }
        if content_area.width == 0 || content_area.height == 0 {
            return;
        }

        let mut y = content_area.y;
        let bottom = content_area.y.saturating_add(content_area.height);
        for row in rows {
            if y >= bottom {
                break;
            }
            let row_h = row.height(content_area.width);
            let slot = Rect {
                x: content_area.x,
                y,
                width: content_area.width,
                height: row_h.min(bottom.saturating_sub(y)),
            };
            row.paint(slot, buf);
            y = y.saturating_add(row_h);
        }
    }
}

fn attachment_prefix(attachment: &AttachmentInput) -> &'static str {
    match attachment {
        AttachmentInput::Directory { .. } => "⎿",
        _ => "●",
    }
}

/// Typed widget for a single assistant tool-use row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantToolUseBodyWidget {
    /// Projected child display.
    pub projection: AssistantToolUseProjection,
    /// Detail rows rendered below the tool header.
    pub body_lines: Vec<String>,
    /// Theme.
    pub theme: MessagesRenderTheme,
    /// Gutter prefix.
    pub prefix: &'static str,
    /// Gutter label style.
    pub label_style: Style,
    /// Whether to add a 1-row top margin.
    add_margin: bool,
}

impl AssistantToolUseBodyWidget {
    /// Build from a minimal raw tool-use block.
    pub fn from_raw(
        id: Option<&str>,
        name: Option<&str>,
        input_summary: Option<&str>,
        body_lines: &[String],
        add_margin: bool,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        let id = id.unwrap_or("tool_use").to_string();
        let name = name.unwrap_or("tool_use").to_string();
        let projection = project_assistant_tool_use(&AssistantToolUseInput {
            tool_use: AssistantToolUseInvocation {
                id: id.clone(),
                name: name.clone(),
            },
            tools_available: true,
            tool: Some(AssistantToolDefinition {
                name: name.clone(),
                user_facing_name: name,
                user_facing_name_background_color: None,
                is_transparent_wrapper: false,
                renders: AssistantToolRenderOutputs {
                    message: input_summary.map(str::to_owned),
                    tag: None,
                    progress_message: None,
                    queued_message: None,
                    hook_progress_message: None,
                },
            }),
            add_margin,
            in_progress_tool_use_ids: Default::default(),
            resolved_tool_use_ids: std::iter::once(id).collect(),
            errored_tool_use_ids: Default::default(),
            should_animate: false,
            should_show_dot: false,
            background: None,
            pending_worker_tool_use_id: None,
            dot_glyph: "●".into(),
        });
        Self {
            projection,
            body_lines: body_lines.to_vec(),
            theme,
            prefix: "●",
            label_style,
            add_margin,
        }
    }

    /// Height in rows at `width`.
    pub fn height(&self, width: u16) -> u16 {
        let content_width = width.saturating_sub(GUTTER_WIDTH).max(1);
        let mut rows = assistant_tool_use_rows(&self.projection, &self.theme);
        rows.extend(self.body_lines.iter().enumerate().map(|(idx, line)| {
            let mut spans = Vec::new();
            if idx == 0 {
                spans.push(Span::styled(
                    format!("{SUBTREE_CONNECTOR}  "),
                    self.theme.dim,
                ));
            } else {
                spans.push(Span::raw("   "));
            }
            spans.push(Span::styled(line.clone(), self.theme.dim));
            BodyRow::Line(Line::from(spans))
        }));
        if rows.is_empty() {
            return 1;
        }
        let body: u16 = rows.iter().map(|row| row.height(content_width)).sum();
        body.saturating_add(u16::from(self.add_margin))
    }

    /// Paint the widget into `area`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let mut rows = assistant_tool_use_rows(&self.projection, &self.theme);
        rows.extend(self.body_lines.iter().enumerate().map(|(idx, line)| {
            let mut spans = Vec::new();
            if idx == 0 {
                spans.push(Span::styled(
                    format!("{SUBTREE_CONNECTOR}  "),
                    self.theme.dim,
                ));
            } else {
                spans.push(Span::raw("   "));
            }
            spans.push(Span::styled(line.clone(), self.theme.dim));
            BodyRow::Line(Line::from(spans))
        }));
        if rows.is_empty() {
            return;
        }
        let (gutter_area, content_area) = split_gutter(area);
        if content_area.width == 0 || content_area.height == 0 {
            return;
        }
        let y = if self.add_margin {
            gutter_area.y.saturating_add(1)
        } else {
            gutter_area.y
        };
        if y >= gutter_area.y.saturating_add(gutter_area.height) {
            return;
        }
        if gutter_area.width > 0 {
            paint_line_into(
                Rect {
                    x: gutter_area.x,
                    y,
                    width: gutter_area.width,
                    height: 1,
                },
                buf,
                gutter_label(self.prefix, self.label_style),
            );
        }
        let mut content_y = if self.add_margin {
            content_area.y.saturating_add(1)
        } else {
            content_area.y
        };
        let bottom = content_area.y.saturating_add(content_area.height);
        for row in rows {
            if content_y >= bottom {
                break;
            }
            let row_h = row.height(content_area.width);
            let slot = Rect {
                x: content_area.x,
                y: content_y,
                width: content_area.width,
                height: row_h.min(bottom.saturating_sub(content_y)),
            };
            row.paint(slot, buf);
            content_y = content_y.saturating_add(row_h);
        }
    }
}

/// Shared helper: render the assistant-text projection, but swap the
/// naive line-per-line `Markdown` branch for the pulldown-cmark-backed
/// renderer in [`crate::markdown_render`]. Used by both the static and
/// streaming constructors of [`AssistantTextBodyWidget`].
fn render_assistant_projection_with_markdown(
    projection: &AssistantTextMessageProjection,
    theme: &MessagesRenderTheme,
    markdown_width: u16,
    render_options: crate::markdown_render::MarkdownRenderOptions,
) -> crate::markdown_render::RenderedMarkdown {
    match projection {
        AssistantTextMessageProjection::Markdown(display) => {
            let md_theme = crate::markdown_render::MarkdownTheme::from_messages(theme);
            crate::markdown_render::render_markdown_blocks_annotated_with_width_and_options(
                &display.markdown,
                &md_theme,
                markdown_width as usize,
                render_options,
            )
        }
        other => crate::markdown_render::RenderedMarkdown {
            text: render_assistant_text_projection(other, theme),
            hyperlinks: Vec::new(),
            formulas: Vec::new(),
        },
    }
}

/// Streaming variant: advance the caller-owned renderer with the
/// cumulative markdown body, then concatenate the memoized stable
/// prefix and the freshly-parsed unstable suffix.
fn render_assistant_markdown_streaming(
    display: &AssistantMarkdownDisplay,
    theme: &MessagesRenderTheme,
    markdown_width: u16,
    renderer: &mut crate::streaming_markdown::StreamingMarkdownRenderer,
) -> crate::markdown_render::RenderedMarkdown {
    let md_theme = crate::markdown_render::MarkdownTheme::from_messages(theme);
    let split = renderer.advance_with_width(&display.markdown, &md_theme, markdown_width as usize);
    let stable_line_count = split.stable.lines.len();
    let mut lines = split.stable.lines;
    lines.extend(split.unstable.lines);
    let mut hyperlinks = split.stable_hyperlinks;
    hyperlinks.extend(split.unstable_hyperlinks.into_iter().map(|mut range| {
        range.line = range.line.saturating_add(stable_line_count);
        range
    }));
    let mut formulas = split.stable_formulas;
    formulas.extend(split.unstable_formulas.into_iter().map(|mut formula| {
        for range in &mut formula.ranges {
            range.line = range.line.saturating_add(stable_line_count);
        }
        formula
    }));
    crate::markdown_render::RenderedMarkdown {
        text: Text::from(lines),
        hyperlinks,
        formulas,
    }
}

/// Typed widget for a single assistant-text row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantTextBodyWidget {
    rendered: crate::markdown_render::RenderedMarkdown,
    theme: MessagesRenderTheme,
    prefix: &'static str,
    label_style: Style,
    add_margin: bool,
}

impl AssistantTextBodyWidget {
    /// Build from a raw assistant-text payload using the child projection.
    ///
    /// The `Markdown` projection branch routes through
    /// [`crate::render_markdown_blocks`] (pulldown-cmark plus the
    /// `rebon-render` markdown token formatter) so headings, code blocks, lists,
    /// emphasis, etc. render with real styled spans instead of the renderer's
    /// line-per-line fallback.
    pub fn from_raw(
        text: &str,
        add_margin: bool,
        verbose: bool,
        show_gutter_dot: bool,
        theme: MessagesRenderTheme,
        label_style: Style,
        markdown_width: u16,
    ) -> Self {
        Self::from_raw_with_options(
            text,
            add_margin,
            verbose,
            show_gutter_dot,
            theme,
            label_style,
            markdown_width,
            crate::markdown_render::MarkdownRenderOptions::default(),
        )
    }

    /// Build from raw assistant text with explicit Markdown extension options.
    #[allow(clippy::too_many_arguments)]
    pub fn from_raw_with_options(
        text: &str,
        add_margin: bool,
        verbose: bool,
        show_gutter_dot: bool,
        theme: MessagesRenderTheme,
        label_style: Style,
        markdown_width: u16,
        render_options: crate::markdown_render::MarkdownRenderOptions,
    ) -> Self {
        let projection = project_assistant_text_message(&AssistantTextMessageInput {
            text: text.to_string(),
            add_margin,
            should_show_dot: show_gutter_dot,
            verbose,
            is_selected: false,
            dot_glyph: "\u{25CF}".into(),
            rate_limit_message: None,
            upgrade_hint: None,
            default_sonnet_model_name: "Sonnet".into(),
            is_keychain_locked: false,
            api_timeout_ms: None,
        });
        let rendered = render_assistant_projection_with_markdown(
            &projection,
            &theme,
            markdown_width,
            render_options,
        );
        Self {
            rendered,
            theme,
            // A stream-continuation row keeps the gutter column (so its
            // body aligns with the dotted half above) but paints no dot.
            prefix: if show_gutter_dot { "●" } else { " " },
            label_style,
            add_margin,
        }
    }

    /// Build from a streaming assistant-text payload. The caller owns a
    /// [`crate::StreamingMarkdownRenderer`] that persists across deltas;
    /// pass it here with the latest cumulative text. The stable prefix
    /// is memoized inside the renderer and only the in-flight block is
    /// re-parsed per call, which is the same split
    /// `StreamingMarkdownRenderer` uses.
    ///
    /// Non-markdown projection branches (`Hidden`, `RateLimit`,
    /// `Response`) fall back to the static renderer since streaming
    /// only applies to the free-form assistant body.
    pub fn from_streaming(
        text: &str,
        add_margin: bool,
        verbose: bool,
        theme: MessagesRenderTheme,
        label_style: Style,
        markdown_width: u16,
        renderer: &mut crate::streaming_markdown::StreamingMarkdownRenderer,
    ) -> Self {
        let projection = project_assistant_text_message(&AssistantTextMessageInput {
            text: text.to_string(),
            add_margin,
            should_show_dot: true,
            verbose,
            is_selected: false,
            dot_glyph: "\u{25CF}".into(),
            rate_limit_message: None,
            upgrade_hint: None,
            default_sonnet_model_name: "Sonnet".into(),
            is_keychain_locked: false,
            api_timeout_ms: None,
        });
        let rendered = match &projection {
            AssistantTextMessageProjection::Markdown(display) => {
                render_assistant_markdown_streaming(display, &theme, markdown_width, renderer)
            }
            _ => {
                // Keep the renderer's cached prefix coherent with the
                // actual visible markdown: a non-markdown branch means
                // no free-form body was emitted this delta.
                renderer.reset();
                crate::markdown_render::RenderedMarkdown {
                    text: render_assistant_text_projection(&projection, &theme),
                    hyperlinks: Vec::new(),
                    formulas: Vec::new(),
                }
            }
        };
        Self {
            rendered,
            theme,
            prefix: "●",
            label_style,
            add_margin,
        }
    }

    /// Height in rows at `width`.
    pub fn height(&self, width: u16) -> u16 {
        let content_width = width.saturating_sub(GUTTER_WIDTH).max(1);
        measure_text_height(&self.rendered.text, content_width)
            .max(1)
            .saturating_add(u16::from(self.add_margin))
    }

    /// Paint the widget into `area`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let mut discarded_layers = Vec::new();
        self.render_to_buffer_with_hyperlinks(area, buf, &mut discarded_layers);
    }

    pub(crate) fn render_to_buffer_with_hyperlinks(
        self,
        area: Rect,
        buf: &mut Buffer,
        layers: &mut Vec<crate::widget::HyperlinkPaintLayer>,
    ) {
        let Self {
            rendered,
            theme,
            prefix,
            label_style,
            add_margin,
        } = self;
        let _ = theme;
        let (gutter_area, content_area) = split_gutter(area);
        if content_area.width == 0 || content_area.height == 0 {
            return;
        }
        // When add_margin is true, push both gutter label and content
        // down by 1 row so they stay aligned on the same line.
        let y = if add_margin {
            content_area.y.saturating_add(1)
        } else {
            content_area.y
        };
        if y >= content_area.y.saturating_add(content_area.height) {
            return;
        }
        if gutter_area.width > 0 && y < gutter_area.y.saturating_add(gutter_area.height) {
            paint_line_into(
                Rect {
                    x: gutter_area.x,
                    y,
                    width: gutter_area.width,
                    height: 1,
                },
                buf,
                gutter_label(prefix, label_style),
            );
        }
        let available_height = content_area
            .y
            .saturating_add(content_area.height)
            .saturating_sub(y);
        let paint_area = Rect {
            x: content_area.x,
            y,
            width: content_area.width,
            height: measure_text_height(&rendered.text, content_area.width)
                .max(1)
                .min(available_height),
        };
        if !rendered.hyperlinks.is_empty() || !rendered.formulas.is_empty() {
            layers.push(crate::widget::HyperlinkPaintLayer {
                area: paint_area,
                text: rendered.text.clone(),
                hyperlinks: rendered.hyperlinks.clone(),
                formulas: rendered.formulas.clone(),
            });
        }
        paint_text_into(paint_area, buf, rendered.text);
    }
}

/// Typed widget for a single user-text row.
///
/// Matches [`AssistantTextBodyWidget`] but uses the user gutter symbol `❯`
/// and routes through the user-text projection pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTextBodyWidget {
    projection: UserTextProjection,
    theme: MessagesRenderTheme,
    prefix: &'static str,
    label_style: Style,
    add_margin: bool,
}

impl UserTextBodyWidget {
    /// Build from a raw user-text payload using the child projection.
    pub fn from_raw(
        user_text: &str,
        add_margin: bool,
        verbose: bool,
        plan_content: Option<String>,
        timestamp: Option<String>,
        is_transcript_mode: bool,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        let projection = project_user_text(&UserTextInput {
            add_margin,
            text: user_text.to_string(),
            verbose,
            plan_content,
            is_transcript_mode,
            timestamp,
            github_webhooks_enabled: false,
            fork_subagent_enabled: false,
            uds_inbox_enabled: false,
            channels_enabled: false,
            agent_swarms_enabled: true,
        });
        let prefix = if projection.is_teammate_task_completion() {
            "●"
        } else if projection.is_plain_teammate_message() {
            "›"
        } else {
            "❯"
        };
        Self {
            projection,
            theme,
            prefix,
            label_style,
            add_margin,
        }
    }

    /// Height in rows at `width`.
    pub fn height(&self, width: u16) -> u16 {
        let content_width = width.saturating_sub(GUTTER_WIDTH).max(1);
        let text =
            render_user_text_projection_for_width(&self.projection, &self.theme, content_width);
        measure_text_height(&text, content_width)
            .max(1)
            .saturating_add(u16::from(self.add_margin))
    }

    /// Paint the widget into `area`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let Self {
            projection,
            theme,
            prefix,
            label_style,
            add_margin,
        } = self;
        let paint_background = matches!(&projection, UserTextProjection::Prompt { .. });
        let (gutter_area, content_area) = split_gutter(area);
        if content_area.width == 0 || content_area.height == 0 {
            return;
        }
        let y = if add_margin {
            content_area.y.saturating_add(1)
        } else {
            content_area.y
        };
        if y >= content_area.y.saturating_add(content_area.height) {
            return;
        }
        let text = render_user_text_projection_for_width(&projection, &theme, content_area.width);
        let text_height = measure_text_height(&text, content_area.width).max(1);
        let render_area = Rect {
            x: content_area.x,
            y,
            width: content_area.width,
            height: content_area
                .y
                .saturating_add(content_area.height)
                .saturating_sub(y),
        };
        if paint_background {
            paint_user_text_background(
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: render_area.height,
                },
                text_height,
                buf,
            );
        }
        if gutter_area.width > 0 && y < gutter_area.y.saturating_add(gutter_area.height) {
            paint_line_into(
                Rect {
                    x: gutter_area.x,
                    y,
                    width: gutter_area.width,
                    height: 1,
                },
                buf,
                gutter_label(prefix, label_style),
            );
        }
        paint_text_into(render_area, buf, text);
    }
}

fn paint_user_text_background(area: Rect, text_height: u16, buf: &mut Buffer) {
    let ds = theme::get_active_theme();
    let bg = parse_theme_color(ds.userMessageBackground);
    let rows = text_height.min(area.height);
    for row in area.y..area.y.saturating_add(rows) {
        for col in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.set_bg(bg);
            }
        }
    }
}

fn render_user_text_projection_for_width(
    projection: &UserTextProjection,
    theme: &MessagesRenderTheme,
    width: u16,
) -> Text<'static> {
    match projection {
        UserTextProjection::Prompt {
            text,
            verbose,
            is_transcript_mode,
            ..
        } => Text::from(
            project_user_prompt_display_lines(text, *verbose, *is_transcript_mode)
                .into_iter()
                .map(|line| match line {
                    UserPromptDisplayLine::Text(line) => Line::from(Span::styled(line, theme.text)),
                    UserPromptDisplayLine::HiddenSeparator { hidden_line_count } => {
                        Line::from(Span::styled(
                            format_user_prompt_hidden_separator(hidden_line_count, width as usize),
                            fold_separator_style(),
                        ))
                    }
                })
                .collect::<Vec<_>>(),
        ),
        _ => render_user_text_projection(projection, theme),
    }
}

/// Typed widget for a single assistant-thinking row.
///
/// Caches the rendered markdown `Text` so that `height()` and
/// `render_to_buffer()` share a single pulldown-cmark parse instead
/// of running the parser independently.
#[derive(Debug, Clone)]
pub struct AssistantThinkingBodyWidget {
    projection: AssistantThinkingMessageProjection,
    theme: MessagesRenderTheme,
    prefix: &'static str,
    label_style: Style,
    redacted_placeholder: bool,
    add_margin: bool,
    cached_text: std::cell::RefCell<Option<(u16, Text<'static>)>>,
}

impl PartialEq for AssistantThinkingBodyWidget {
    fn eq(&self, other: &Self) -> bool {
        self.projection == other.projection
            && self.theme == other.theme
            && self.prefix == other.prefix
            && self.label_style == other.label_style
            && self.redacted_placeholder == other.redacted_placeholder
            && self.add_margin == other.add_margin
    }
}

impl Eq for AssistantThinkingBodyWidget {}

impl AssistantThinkingBodyWidget {
    /// Build from a raw thinking payload using the child projection.
    pub fn from_raw(
        thinking: &str,
        add_margin: bool,
        is_transcript_mode: bool,
        verbose: bool,
        hide_in_transcript: bool,
        compact_preview: bool,
        show_expand_hint: bool,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        Self::from_projection(
            project_assistant_thinking_message(&AssistantThinkingMessageInput {
                thinking: thinking.to_string(),
                add_margin,
                is_transcript_mode,
                verbose,
                hide_in_transcript,
                compact_preview,
                show_expand_hint,
            }),
            theme,
            label_style,
        )
    }

    /// Build a visible redacted-thinking placeholder branch.
    pub fn redacted_placeholder(
        add_margin: bool,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        Self {
            projection: AssistantThinkingMessageProjection::Hidden,
            theme,
            prefix: "·",
            label_style,
            redacted_placeholder: true,
            add_margin,
            cached_text: std::cell::RefCell::new(None),
        }
    }

    /// Build from an already projected thinking display.
    pub fn from_projection(
        projection: AssistantThinkingMessageProjection,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        let add_margin = match &projection {
            AssistantThinkingMessageProjection::Hidden => false,
            AssistantThinkingMessageProjection::Collapsed(display) => display.margin_top > 0,
            AssistantThinkingMessageProjection::Expanded(display) => display.margin_top > 0,
        };
        Self {
            projection,
            theme,
            prefix: "·",
            label_style,
            redacted_placeholder: false,
            add_margin,
            cached_text: std::cell::RefCell::new(None),
        }
    }

    fn rendered_text(&self, content_width: u16) -> Text<'static> {
        {
            let cached = self.cached_text.borrow();
            if let Some((w, text)) = cached.as_ref() {
                if *w == content_width {
                    return text.clone();
                }
            }
        }
        let text =
            render_thinking_projection_with_markdown(&self.projection, &self.theme, content_width);
        *self.cached_text.borrow_mut() = Some((content_width, text.clone()));
        text
    }

    /// Height in rows at `width`.
    pub fn height(&self, width: u16) -> u16 {
        if matches!(self.projection, AssistantThinkingMessageProjection::Hidden)
            && !self.redacted_placeholder
        {
            return 0;
        }
        if self.redacted_placeholder {
            let rows = assistant_thinking_rows(
                &self.projection,
                self.redacted_placeholder,
                self.add_margin,
                &self.theme,
            );
            if rows.is_empty() {
                return 0;
            }
            let content_width = width.saturating_sub(GUTTER_WIDTH).max(1);
            return rows.iter().map(|row| row.height(content_width)).sum();
        }

        let content_width = width.saturating_sub(GUTTER_WIDTH).max(1);
        let text = self.rendered_text(content_width);
        measure_text_height(&text, content_width)
            .max(1)
            .saturating_add(u16::from(self.add_margin))
    }

    /// Paint the widget into `area`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let Self {
            projection,
            theme,
            prefix,
            label_style,
            redacted_placeholder,
            add_margin,
            cached_text,
        } = self;
        if matches!(projection, AssistantThinkingMessageProjection::Hidden) && !redacted_placeholder
        {
            return;
        }
        if redacted_placeholder {
            let rows = assistant_thinking_rows(&projection, true, add_margin, &theme);
            if rows.is_empty() {
                return;
            }
            let (gutter_area, content_area) = split_gutter(area);
            if content_area.width == 0 || content_area.height == 0 {
                return;
            }
            let label_offset = rows
                .iter()
                .take_while(|r| matches!(r, BodyRow::Blank))
                .count() as u16;
            let label_y = content_area.y.saturating_add(label_offset);
            if gutter_area.width > 0 && label_y < gutter_area.y.saturating_add(gutter_area.height) {
                paint_line_into(
                    Rect {
                        x: gutter_area.x,
                        y: label_y,
                        width: gutter_area.width,
                        height: 1,
                    },
                    buf,
                    gutter_label(prefix, label_style),
                );
            }
            let mut y = content_area.y;
            let bottom = content_area.y.saturating_add(content_area.height);
            for row in rows {
                if y >= bottom {
                    break;
                }
                let row_h = row.height(content_area.width);
                let slot = Rect {
                    x: content_area.x,
                    y,
                    width: content_area.width,
                    height: row_h.min(bottom.saturating_sub(y)),
                };
                row.paint(slot, buf);
                y = y.saturating_add(row_h);
            }
            return;
        }

        let (gutter_area, content_area) = split_gutter(area);
        if content_area.width == 0 || content_area.height == 0 {
            return;
        }
        let y = if add_margin {
            content_area.y.saturating_add(1)
        } else {
            content_area.y
        };
        if y >= content_area.y.saturating_add(content_area.height) {
            return;
        }
        if gutter_area.width > 0 && y < gutter_area.y.saturating_add(gutter_area.height) {
            paint_line_into(
                Rect {
                    x: gutter_area.x,
                    y,
                    width: gutter_area.width,
                    height: 1,
                },
                buf,
                gutter_label(prefix, label_style),
            );
        }
        let text = cached_text
            .into_inner()
            .filter(|(w, _)| *w == content_area.width)
            .map(|(_, t)| t)
            .unwrap_or_else(|| {
                render_thinking_projection_with_markdown(&projection, &theme, content_area.width)
            });
        paint_text_into(
            Rect {
                x: content_area.x,
                y,
                width: content_area.width,
                height: content_area
                    .y
                    .saturating_add(content_area.height)
                    .saturating_sub(y),
            },
            buf,
            text,
        );
    }
}

/// Typed widget for a single user-tool-result row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserToolResultBodyWidget {
    projection: UserToolResultProjection,
    result_body: Option<String>,
    theme: MessagesRenderTheme,
    prefix: &'static str,
    label_style: Style,
}

impl UserToolResultBodyWidget {
    /// Build from a raw tool-result payload using the child projection.
    pub fn from_raw(
        input: UserToolResultInput,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        let projection = project_user_tool_result(&input);
        Self::from_projection(
            projection,
            Some(input.content),
            input.is_error,
            theme,
            label_style,
        )
    }

    /// Build from an already projected tool-result branch.
    pub fn from_projection(
        projection: UserToolResultProjection,
        result_body: Option<String>,
        _is_error: bool,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        Self {
            projection,
            result_body,
            theme,
            prefix: "↳",
            label_style,
        }
    }

    /// Height in rows at `width`.
    pub fn height(&self, width: u16) -> u16 {
        let content_width = width.saturating_sub(GUTTER_WIDTH).max(1);
        let (rows, block) =
            user_tool_result_layout(&self.projection, self.result_body.as_deref(), &self.theme);
        let rows_h: u16 = rows.iter().map(|row| row.height(content_width)).sum();
        let block_h = block.map(|block| block.height(content_width)).unwrap_or(0);
        rows_h.saturating_add(block_h).max(1)
    }

    /// Paint the widget into `area`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let Self {
            projection,
            result_body,
            theme,
            prefix,
            label_style,
        } = self;
        let (rows, block) = user_tool_result_layout(&projection, result_body.as_deref(), &theme);
        let (gutter_area, content_area) = split_gutter(area);
        if gutter_area.width > 0 && gutter_area.height > 0 {
            paint_line_into(
                Rect {
                    x: gutter_area.x,
                    y: gutter_area.y,
                    width: gutter_area.width,
                    height: 1,
                },
                buf,
                gutter_label(prefix, label_style),
            );
        }
        if content_area.width == 0 || content_area.height == 0 {
            return;
        }
        let mut y = content_area.y;
        let bottom = content_area.y.saturating_add(content_area.height);
        for row in rows {
            if y >= bottom {
                return;
            }
            let row_h = row.height(content_area.width);
            let slot = Rect {
                x: content_area.x,
                y,
                width: content_area.width,
                height: row_h.min(bottom.saturating_sub(y)),
            };
            row.paint(slot, buf);
            y = y.saturating_add(row_h);
        }
        if let Some(block) = block {
            if y >= bottom {
                return;
            }
            let block_h = block.height(content_area.width);
            block.paint(
                Rect {
                    x: content_area.x,
                    y,
                    width: content_area.width,
                    height: block_h.min(bottom.saturating_sub(y)),
                },
                buf,
            );
        }
    }
}

/// Connector glyph for the first child in a subtree (`⎿`).
const SUBTREE_CONNECTOR: &str = "\u{23BF}";

/// File-edit projection kinds accepted by [`FileEditBodyWidget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEditBodyKind {
    /// A file edit that went through, described by
    /// [`FileEditUpdatedProjection`].
    Updated(FileEditUpdatedProjection),
    /// A file edit the user turned down, described by
    /// [`FileEditRejectedProjection`].
    Rejected(FileEditRejectedProjection),
    /// A rejected notebook cell edit, described by
    /// [`NotebookEditRejectedProjection`].
    NotebookRejected(NotebookEditRejectedProjection),
}

/// Typed file-edit body widget with a bordered diff/preview block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEditBodyWidget {
    /// The projection being rendered.
    pub projection: FileEditBodyKind,
    /// Owned theme.
    pub theme: MessagesRenderTheme,
    /// Gutter prefix label (`"EDT"` / `"REJ"` / `"NB"`).
    pub prefix: &'static str,
    /// Gutter label style.
    pub label_style: Style,
}

impl FileEditBodyWidget {
    /// Construct from an [`FileEditUpdatedProjection`].
    pub fn from_updated(
        projection: FileEditUpdatedProjection,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        Self {
            projection: FileEditBodyKind::Updated(projection),
            theme,
            prefix: "●",
            label_style,
        }
    }

    /// Construct from an [`FileEditRejectedProjection`].
    pub fn from_rejected(
        projection: FileEditRejectedProjection,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        Self {
            projection: FileEditBodyKind::Rejected(projection),
            theme,
            prefix: "●",
            label_style,
        }
    }

    /// Construct from a [`NotebookEditRejectedProjection`].
    pub fn from_notebook_rejected(
        projection: NotebookEditRejectedProjection,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        Self {
            projection: FileEditBodyKind::NotebookRejected(projection),
            theme,
            prefix: "●",
            label_style,
        }
    }

    /// Height in rows at `width`.
    pub fn height(&self, width: u16) -> u16 {
        let layout = file_edit_layout(&self.projection, &self.theme);
        let content_width = width.saturating_sub(GUTTER_WIDTH).max(1);
        let mut total = layout.summary_height(content_width);
        if let Some(block) = &layout.block {
            total = total.saturating_add(block.height(content_width));
        }
        if layout.footer.is_some() {
            total = total.saturating_add(1);
        }
        total.max(1)
    }

    /// Paint the widget into `area`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let layout = file_edit_layout(&self.projection, &self.theme);
        let (gutter_area, content_area) = split_gutter(area);
        if gutter_area.width > 0 && gutter_area.height > 0 {
            paint_line_into(
                Rect {
                    x: gutter_area.x,
                    y: gutter_area.y,
                    width: gutter_area.width,
                    height: 1,
                },
                buf,
                gutter_label(self.prefix, self.label_style),
            );
        }
        if content_area.width == 0 || content_area.height == 0 {
            return;
        }

        let mut y = content_area.y;
        let bottom = content_area.y.saturating_add(content_area.height);

        // Summary header line.
        let summary_h = layout.summary_height(content_area.width);
        if summary_h > 0 {
            let slot = Rect {
                x: content_area.x,
                y,
                width: content_area.width,
                height: summary_h.min(bottom.saturating_sub(y)),
            };
            paint_line_into(slot, buf, layout.summary);
            y = y.saturating_add(summary_h);
        }

        // Bordered diff / preview block.
        if let Some(block_data) = layout.block {
            if y >= bottom {
                return;
            }
            let block_h = block_data.height(content_area.width);
            let block_area = Rect {
                x: content_area.x,
                y,
                width: content_area.width,
                height: block_h.min(bottom.saturating_sub(y)),
            };
            block_data.paint(block_area, buf);
            y = y.saturating_add(block_h);
        }

        // Optional footer (hidden-line hint).
        if let Some(footer) = layout.footer {
            if y >= bottom {
                return;
            }
            let footer_slot = Rect {
                x: content_area.x,
                y,
                width: content_area.width,
                height: 1,
            };
            paint_line_into(footer_slot, buf, footer);
        }
    }
}

/// Internal file-edit layout result: a summary line, an optional
/// diff/preview block, and an optional footer hint.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileEditLayout {
    summary: Line<'static>,
    block: Option<FileEditBlock>,
    footer: Option<Line<'static>>,
}

/// Either a bordered block (write previews, notebook) or an inline diff body.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileEditBlock {
    Bordered(BorderedBlock),
    Inline(InlineDiffBlock),
}

impl FileEditBlock {
    fn height(&self, width: u16) -> u16 {
        match self {
            Self::Bordered(b) => b.height(width),
            Self::Inline(i) => i.height(width),
        }
    }

    fn paint(self, area: Rect, buf: &mut Buffer) {
        match self {
            Self::Bordered(b) => b.paint(area, buf),
            Self::Inline(i) => i.paint(area, buf),
        }
    }
}

impl FileEditLayout {
    fn summary_height(&self, width: u16) -> u16 {
        line_row_height(&self.summary, width).max(1)
    }
}

fn file_edit_layout(projection: &FileEditBodyKind, theme: &MessagesRenderTheme) -> FileEditLayout {
    match projection {
        FileEditBodyKind::Updated(updated) => match updated {
            FileEditUpdatedProjection::PreviewHint { hint } => FileEditLayout {
                summary: Line::from(Span::styled(hint.clone(), theme.warning)),
                block: None,
                footer: None,
            },
            FileEditUpdatedProjection::SummaryOnly { summary } => FileEditLayout {
                summary: Line::from(Span::styled(summary.clone(), theme.text)),
                block: None,
                footer: None,
            },
            FileEditUpdatedProjection::Detailed {
                summary,
                structured_patch,
                diff_width,
                fold_long_runs,
                ..
            } => {
                let diff_lines =
                    render_inline_diff_for_display(structured_patch, *diff_width, *fold_long_runs);
                let body = diff_lines_to_text(&diff_lines, theme);
                FileEditLayout {
                    summary: Line::from(Span::styled(summary.clone(), theme.text)),
                    block: if body.lines.is_empty() {
                        None
                    } else {
                        Some(FileEditBlock::Inline(InlineDiffBlock { body }))
                    },
                    footer: None,
                }
            }
        },
        FileEditBodyKind::Rejected(rejected) => match rejected {
            FileEditRejectedProjection::SummaryOnly { summary } => FileEditLayout {
                summary: Line::from(Span::styled(summary.clone(), theme.error)),
                block: None,
                footer: None,
            },
            FileEditRejectedProjection::WritePreview {
                summary,
                preview,
                hidden_line_count,
                file_path,
                ..
            } => {
                let total_lines = preview.lines().count();
                let body_text = Text::from(
                    preview
                        .lines()
                        .map(|line| Line::from(Span::styled(line.to_owned(), theme.text)))
                        .collect::<Vec<_>>(),
                );
                let footer = (*hidden_line_count > 0).then(|| {
                    Line::from(Span::styled(
                        format!(
                            "... +{hidden_line_count} {}",
                            if *hidden_line_count == 1 {
                                "line"
                            } else {
                                "lines"
                            }
                        ),
                        theme.dim,
                    ))
                });
                FileEditLayout {
                    summary: Line::from(Span::styled(summary.clone(), theme.error)),
                    block: Some(FileEditBlock::Bordered(BorderedBlock {
                        title: format!(" {file_path} (write preview) "),
                        title_style: theme.error,
                        title_bottom: Some(format!(" {total_lines} lines ")),
                        title_bottom_style: theme.dim,
                        body: BorderedBlockBody::SingleColumn(body_text),
                        border_style: theme.error,
                    })),
                    footer,
                }
            }
            FileEditRejectedProjection::DiffPreview {
                summary,
                patch,
                diff_width,
                fold_long_runs,
                ..
            } => {
                let diff_lines =
                    render_inline_diff_for_display(patch, *diff_width, *fold_long_runs);
                let body = diff_lines_to_text(&diff_lines, theme);
                FileEditLayout {
                    summary: Line::from(Span::styled(summary.clone(), theme.error)),
                    block: if body.lines.is_empty() {
                        None
                    } else {
                        Some(FileEditBlock::Inline(InlineDiffBlock { body }))
                    },
                    footer: None,
                }
            }
        },
        FileEditBodyKind::NotebookRejected(notebook) => {
            let block = notebook.preview.as_ref().map(|preview| {
                let line_count = preview.lines().count();
                FileEditBlock::Bordered(BorderedBlock {
                    title: format!(
                        " {} ",
                        notebook
                            .preview_file_path
                            .clone()
                            .unwrap_or_else(|| "cell".to_string())
                    ),
                    title_style: theme.error,
                    title_bottom: Some(format!(" {line_count} lines ")),
                    title_bottom_style: theme.dim,
                    body: BorderedBlockBody::SingleColumn(Text::from(
                        preview
                            .lines()
                            .map(|line| Line::from(Span::styled(line.to_owned(), theme.text)))
                            .collect::<Vec<_>>(),
                    )),
                    border_style: theme.error,
                })
            });
            FileEditLayout {
                summary: Line::from(Span::styled(notebook.summary.clone(), theme.error)),
                block,
                footer: None,
            }
        }
    }
}

// ── Inline diff helpers ──────────────────────────────────────────────

/// Render all hunks in a structured patch into `RenderedLine`s using the
/// `rebon-render` fallback pipeline. Multiple hunks are separated
/// by a `...` ellipsis line.
pub fn render_inline_diff(hunks: &[StructuredPatchHunk], width: usize) -> Vec<RenderedLine> {
    render_inline_diff_for_display(hunks, width, false)
}

fn render_inline_diff_for_display(
    hunks: &[StructuredPatchHunk],
    width: usize,
    fold_long_runs: bool,
) -> Vec<RenderedLine> {
    let wrap = |s: &str, _w: usize| -> Vec<String> { vec![s.to_string()] };
    let options = FormatOptions {
        width,
        dim: false,
        wrap: &wrap,
        word_diff: &calculate_word_diff,
    };

    let mut all: Vec<RenderedLine> = Vec::new();
    for (idx, hunk) in hunks.iter().enumerate() {
        if idx > 0 {
            // Ellipsis separator between hunks.
            all.push(RenderedLine {
                gutter: String::new(),
                content: vec![DiffSegment {
                    text: "...".to_string(),
                    word_color: WordColor::None,
                }],
                padding: String::new(),
                line_color: LineColor::None,
                dim: true,
            });
        }
        let rendered = format_diff_lines(&hunk.lines, 1, &options);
        if fold_long_runs {
            all.extend(fold_long_diff_runs(rendered, width));
        } else {
            all.extend(rendered);
        }
    }
    all
}

/// Convert `RenderedLine`s into a ratatui `Text` block, resolving
/// `LineColor` / `WordColor` intents to theme-derived styles.
///
/// `diffAdded` / `diffRemoved` palette tokens are subtle tints meant
/// to be painted as **line backgrounds**, not foreground colours:
/// `diffAddedWord` / `diffRemovedWord` are a stronger background
/// accent on the changed token. Painting the line tokens as a
/// foreground colour only produced unreadable dark-green-on-dark
/// terminal text.
pub fn diff_lines_to_text(lines: &[RenderedLine], theme: &MessagesRenderTheme) -> Text<'static> {
    let ds = theme::get_active_theme();
    let added_bg = parse_theme_color(ds.diffAdded);
    let removed_bg = parse_theme_color(ds.diffRemoved);
    let added_word_bg = parse_theme_color(ds.diffAddedWord);
    let removed_word_bg = parse_theme_color(ds.diffRemovedWord);
    let added_dim_bg = parse_theme_color(ds.diffAddedDimmed);
    let removed_dim_bg = parse_theme_color(ds.diffRemovedDimmed);

    let ratatui_lines: Vec<Line<'static>> = lines
        .iter()
        .map(|rl| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let is_hunk_separator = rl.gutter.is_empty()
                && rl.padding.is_empty()
                && rl.dim
                && matches!(rl.line_color, LineColor::None)
                && rl.content.len() == 1
                && matches!(rl.content[0].word_color, WordColor::None)
                && rl.content[0].text == "...";
            let is_hidden_separator = !rl.gutter.is_empty()
                && rl.gutter.chars().all(char::is_whitespace)
                && rl.padding.is_empty()
                && rl.dim
                && rl.content.len() == 1
                && matches!(rl.content[0].word_color, WordColor::None)
                && rl.content[0].text.starts_with("──── (")
                && rl.content[0].text.contains(" lines hidden)");
            let dim_style = if is_hunk_separator || is_hidden_separator {
                fold_separator_style()
            } else {
                theme.dim
            };
            let (line_bg, word_bg) = match rl.line_color {
                LineColor::Added => (Some(added_bg), Some(added_word_bg)),
                LineColor::AddedDimmed => (Some(added_dim_bg), Some(added_word_bg)),
                LineColor::Removed => (Some(removed_bg), Some(removed_word_bg)),
                LineColor::RemovedDimmed => (Some(removed_dim_bg), Some(removed_word_bg)),
                LineColor::None => (None, None),
            };

            if !rl.gutter.is_empty() {
                let gutter_style = line_bg.map_or(dim_style, |bg| dim_style.bg(bg));
                spans.push(Span::styled(rl.gutter.clone(), gutter_style));
            }

            for seg in &rl.content {
                let mut style = Style::new();
                if let Some(bg) = line_bg {
                    style = style.bg(bg);
                }
                match seg.word_color {
                    WordColor::AddedWord | WordColor::RemovedWord => {
                        if let Some(bg) = word_bg {
                            style = style.bg(bg);
                        }
                    }
                    WordColor::None => {
                        if rl.dim {
                            // Preserve dim fg from theme but keep
                            // any line bg above so the dim row still
                            // looks like part of the diff column.
                            style = style.patch(dim_style);
                        }
                    }
                }
                spans.push(Span::styled(seg.text.clone(), style));
            }

            if let Some(bg) = line_bg {
                spans.push(Span::styled(rl.padding.clone(), Style::new().bg(bg)));
            }

            Line::from(spans)
        })
        .collect();

    Text::from(ratatui_lines)
}

/// An inline diff body — no border, just indented diff lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InlineDiffBlock {
    body: Text<'static>,
}

impl InlineDiffBlock {
    fn height(&self, width: u16) -> u16 {
        measure_text_height(&self.body, width).max(1)
    }

    fn paint(self, area: Rect, buf: &mut Buffer) {
        paint_text_into(area, buf, self.body);
    }
}

/// Typed fallback tool-use error body widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackBodyWidget {
    /// Owned projection.
    pub projection: FallbackToolUseErrorProjection,
    /// Owned theme.
    pub theme: MessagesRenderTheme,
    /// Gutter prefix label (`"ERR"`).
    pub prefix: &'static str,
    /// Gutter label style.
    pub label_style: Style,
}

impl FallbackBodyWidget {
    /// Construct from a projection.
    pub fn new(
        projection: FallbackToolUseErrorProjection,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        Self {
            projection,
            theme,
            prefix: "●",
            label_style,
        }
    }

    /// Height in rows at `width`.
    pub fn height(&self, width: u16) -> u16 {
        let content_width = width.saturating_sub(GUTTER_WIDTH).max(1);
        let body = Text::from(
            self.projection
                .error_text
                .lines()
                .map(|line| Line::from(Span::raw(line.to_owned())))
                .collect::<Vec<_>>(),
        );
        let block_h = if content_width > 2 {
            let inner = content_width.saturating_sub(2).max(1);
            measure_text_height(&body, inner).max(1) + 2
        } else {
            2
        };
        let footer_h = u16::from(self.projection.footer.is_some());
        block_h + footer_h
    }

    /// Paint the widget into `area`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let (gutter_area, content_area) = split_gutter(area);
        if gutter_area.width > 0 && gutter_area.height > 0 {
            paint_line_into(
                Rect {
                    x: gutter_area.x,
                    y: gutter_area.y,
                    width: gutter_area.width,
                    height: 1,
                },
                buf,
                gutter_label(self.prefix, self.label_style),
            );
        }
        if content_area.width == 0 || content_area.height == 0 {
            return;
        }

        let mut y = content_area.y;
        let bottom = content_area.y.saturating_add(content_area.height);

        // Bordered error block.
        let body_lines: Vec<Line<'static>> = self
            .projection
            .error_text
            .lines()
            .map(|line| Line::from(Span::styled(line.to_owned(), self.theme.error)))
            .collect();
        let body = Text::from(if body_lines.is_empty() {
            vec![Line::from(Span::styled("(empty error)", self.theme.dim))]
        } else {
            body_lines
        });
        let hidden = self.projection.hidden_line_count;
        let bordered = BorderedBlock {
            title: " Tool Use Error ".to_string(),
            title_style: self.theme.error,
            title_bottom: (hidden > 0).then(|| format!(" +{hidden} hidden ")),
            title_bottom_style: self.theme.dim,
            body: BorderedBlockBody::SingleColumn(body),
            border_style: self.theme.error,
        };
        let block_h = bordered.height(content_area.width);
        let block_slot = Rect {
            x: content_area.x,
            y,
            width: content_area.width,
            height: block_h.min(bottom.saturating_sub(y)),
        };
        bordered.paint(block_slot, buf);
        y = y.saturating_add(block_h);

        if let Some(footer) = self.projection.footer {
            if y >= bottom {
                return;
            }
            let footer_slot = Rect {
                x: content_area.x,
                y,
                width: content_area.width,
                height: 1,
            };
            paint_line_into(
                footer_slot,
                buf,
                Line::from(Span::styled(footer, self.theme.dim)),
            );
        }
    }
}

/// Typed widget for the `StopHookSummary` branch of a system text row.
///
/// Renders a bordered block titled " Stop Hooks " with the hook count /
/// total duration in the bottom-right, the `summary` text as the header row
/// inside the block, and the prevented-continuation / error lines stacked
/// below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemTextBodyWidget {
    /// Owned system projection that will drive the body.
    pub projection: SystemTextProjection,
    /// Owned theme.
    pub theme: MessagesRenderTheme,
}

impl SystemTextBodyWidget {
    /// Construct from an owned `SystemTextProjection`.
    pub fn from_projection(
        projection: SystemTextProjection,
        theme: MessagesRenderTheme,
        _label_style: Style,
    ) -> Self {
        Self { projection, theme }
    }

    /// Construct from an owned `StopHookSummaryDisplay`.
    pub fn from_stop_hook_summary(
        display: StopHookSummaryDisplay,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        Self::from_projection(
            SystemTextProjection::StopHookSummary(display),
            theme,
            label_style,
        )
    }

    /// Height in rows at `width`.
    pub fn height(&self, width: u16) -> u16 {
        let content_width = width.max(1);
        let (rows, block) = system_text_layout(&self.projection, &self.theme);
        let rows_h: u16 = rows.iter().map(|row| row.height(content_width)).sum();
        let block_h = block.map(|block| block.height(content_width)).unwrap_or(0);
        rows_h.saturating_add(block_h).max(1)
    }

    /// Paint the widget into `area`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let (rows, block) = system_text_layout(&self.projection, &self.theme);
        let mut y = area.y;
        let bottom = area.y.saturating_add(area.height);
        for row in rows {
            if y >= bottom {
                return;
            }
            let row_h = row.height(area.width);
            let slot = Rect {
                x: area.x,
                y,
                width: area.width,
                height: row_h.min(bottom.saturating_sub(y)),
            };
            row.paint(slot, buf);
            y = y.saturating_add(row_h);
        }
        if let Some(block) = block {
            if y >= bottom {
                return;
            }
            let block_h = block.height(area.width);
            let slot = Rect {
                x: area.x,
                y,
                width: area.width,
                height: block_h.min(bottom.saturating_sub(y)),
            };
            block.paint(slot, buf);
        }
    }
}

fn system_text_layout(
    projection: &SystemTextProjection,
    theme: &MessagesRenderTheme,
) -> (Vec<BodyRow>, Option<BorderedBlock>) {
    let margin_top = match projection {
        SystemTextProjection::TurnDuration(display) => display.margin_top,
        SystemTextProjection::MemorySaved(display) => display.margin_top,
        SystemTextProjection::BridgeStatus(display) => display.margin_top,
        SystemTextProjection::PermissionRetry { margin_top, .. }
        | SystemTextProjection::ProviderSwitch { margin_top, .. } => *margin_top,
        SystemTextProjection::AwaySummary(display)
        | SystemTextProjection::AgentsKilled(display)
        | SystemTextProjection::ScheduledTaskFire(display)
        | SystemTextProjection::Generic(display)
        | SystemTextProjection::Thinking(display) => display.margin_top,
        SystemTextProjection::StopHookSummary(_)
        | SystemTextProjection::ApiError(_)
        | SystemTextProjection::Hidden => 0,
    };
    let (mut rows, block) = match projection {
        SystemTextProjection::ProviderSwitch {
            provider, model, ..
        } => (
            crate::projection_render::provider_switch_lines(provider, model, theme)
                .into_iter()
                .map(BodyRow::Line)
                .collect(),
            None,
        ),
        SystemTextProjection::StopHookSummary(display) => stop_hook_layout(display, theme),
        SystemTextProjection::TurnDuration(display) => {
            let mut text = display.duration_text.clone().unwrap_or_default();
            if let Some(budget) = &display.budget {
                if budget.prefixed_with_separator && !text.is_empty() {
                    text.push_str(" · ");
                }
                text.push_str(&budget.usage_text);
                if let Some(nudges) = &budget.nudges_text {
                    text.push_str(" · ");
                    text.push_str(nudges);
                }
            }
            let mut rows = vec![BodyRow::Line(render_system_line(
                Some(display.marker),
                text,
                theme.dim,
            ))];
            if let Some(summary) = &display.background_task_summary {
                rows.push(BodyRow::Indented {
                    indent: 2,
                    line: Line::from(Span::styled(summary.clone(), theme.dim)),
                });
            }
            (rows, None)
        }
        SystemTextProjection::MemorySaved(display) => {
            let mut rows = vec![BodyRow::Line(render_system_line(
                Some(display.marker),
                format!("{} {}", display.verb, display.parts.join(" · ")),
                theme.text,
            ))];
            rows.extend(display.entries.iter().map(|entry| BodyRow::Indented {
                indent: 2,
                line: Line::from(Span::styled(entry.basename.clone(), theme.dim)),
            }));
            (rows, None)
        }
        SystemTextProjection::BridgeStatus(display) => (
            vec![
                BodyRow::Line(Line::from(Span::styled(display.intro, theme.text))),
                BodyRow::Line(Line::from(Span::styled(display.url.clone(), theme.accent))),
            ]
            .into_iter()
            .chain(
                display
                    .upgrade_nudge
                    .as_ref()
                    .map(|line| BodyRow::Line(Line::from(Span::styled(line.clone(), theme.dim)))),
            )
            .collect(),
            None,
        ),
        SystemTextProjection::PermissionRetry {
            commands, marker, ..
        } => (
            vec![BodyRow::Line(render_system_line(
                Some(*marker),
                format!("Allowed {commands}"),
                theme.text,
            ))],
            None,
        ),
        SystemTextProjection::ApiError(display) => {
            let mut rows = Vec::new();
            if let Some(error) = display.displayed_error.as_ref() {
                rows.push(BodyRow::Indented {
                    indent: 2,
                    line: Line::from(vec![
                        Span::styled(format!("{SUBTREE_CONNECTOR}  "), theme.dim),
                        Span::styled(error.clone(), theme.error),
                    ]),
                });
            }
            if display.show_expand_hint {
                rows.push(BodyRow::Indented {
                    indent: 5,
                    line: Line::from(Span::styled(
                        format_shortcut_hint("ctrl+o", "expand", false, false).plain_text,
                        theme.dim,
                    )),
                });
            }
            if let Some(line) = display.retry_text.as_ref() {
                rows.push(BodyRow::Indented {
                    indent: 5,
                    line: Line::from(Span::styled(line.clone(), theme.dim)),
                });
            }
            (rows, None)
        }
        SystemTextProjection::AwaySummary(display)
        | SystemTextProjection::AgentsKilled(display)
        | SystemTextProjection::ScheduledTaskFire(display)
        | SystemTextProjection::Generic(display)
        | SystemTextProjection::Thinking(display) => (
            vec![BodyRow::Line(render_system_line(
                display.marker,
                display.content.clone(),
                match display.color.as_deref() {
                    Some("warning") => theme.warning,
                    Some("error") => theme.error,
                    _ if display.dim_color => theme.dim,
                    _ => theme.text,
                },
            ))],
            None,
        ),
        SystemTextProjection::Hidden => (Vec::new(), None),
    };
    if margin_top > 0 && !rows.is_empty() {
        rows.insert(0, BodyRow::Blank);
    }
    (rows, block)
}

fn stop_hook_layout(
    display: &StopHookSummaryDisplay,
    theme: &MessagesRenderTheme,
) -> (Vec<BodyRow>, Option<BorderedBlock>) {
    match display {
        StopHookSummaryDisplay::Labeled {
            summary,
            transcript_lines,
        } => {
            if transcript_lines.is_empty() {
                (
                    vec![BodyRow::Line(Line::from(Span::styled(
                        summary.clone(),
                        theme.text,
                    )))],
                    None,
                )
            } else {
                let body = Text::from(
                    transcript_lines
                        .iter()
                        .map(|line| Line::from(Span::styled(line.clone(), theme.dim)))
                        .collect::<Vec<_>>(),
                );
                (
                    vec![BodyRow::Line(Line::from(Span::styled(
                        summary.clone(),
                        theme.text,
                    )))],
                    Some(BorderedBlock {
                        title: " Stop Hooks ".to_string(),
                        title_style: theme.accent,
                        title_bottom: Some(format!(" {summary} ")),
                        title_bottom_style: theme.dim,
                        body: BorderedBlockBody::SingleColumn(body),
                        border_style: theme.dim,
                    }),
                )
            }
        }
        StopHookSummaryDisplay::Default {
            summary,
            detail_lines,
            prevented_line,
            error_lines,
            ..
        } => {
            // Build the body by stacking prevented + error + detail lines.
            let mut body_lines: Vec<Line<'static>> = Vec::new();
            if let Some(line) = prevented_line.as_ref() {
                body_lines.push(Line::from(Span::styled(line.clone(), theme.warning)));
            }
            for err in error_lines {
                body_lines.push(Line::from(Span::styled(err.clone(), theme.error)));
            }
            for detail in detail_lines {
                body_lines.push(Line::from(Span::styled(detail.clone(), theme.dim)));
            }
            if body_lines.is_empty() {
                (
                    vec![BodyRow::Line(Line::from(Span::styled(
                        summary.clone(),
                        theme.text,
                    )))],
                    None,
                )
            } else {
                let border_style = if !error_lines.is_empty() {
                    theme.error
                } else if prevented_line.is_some() {
                    theme.warning
                } else {
                    theme.dim
                };
                (
                    vec![BodyRow::Line(Line::from(Span::styled(
                        summary.clone(),
                        theme.text,
                    )))],
                    Some(BorderedBlock {
                        title: " Stop Hooks ".to_string(),
                        title_style: theme.accent,
                        title_bottom: Some(format!(" {summary} ")),
                        title_bottom_style: theme.dim,
                        body: BorderedBlockBody::SingleColumn(Text::from(body_lines)),
                        border_style,
                    }),
                )
            }
        }
    }
}

fn render_system_line(
    _marker: Option<SystemVisualMarker>,
    text: String,
    style: Style,
) -> Line<'static> {
    Line::from(Span::styled(text, style))
}

/// Typed widget for the plan-content branch of a user text row.
///
/// Fences the (markdown) plan body between a titled rule and a closing rule.
/// The rules span the gutter column too, so the `●` sits under the opening
/// rule rather than beside a second vertical border.
///
/// The glyph is the event `●`, not the prompt `❯`: the plan reaches the
/// transcript as a user message, but nobody typed it — it is what the model
/// proposed, echoed back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTextPlanBodyWidget {
    /// Plan body text (may span multiple lines; kept as-is and painted unchanged).
    pub plan_content: String,
    /// Owned theme.
    pub theme: MessagesRenderTheme,
    /// Gutter prefix label (`"●"`).
    pub prefix: &'static str,
    /// Gutter label style.
    pub label_style: Style,
}

impl UserTextPlanBodyWidget {
    /// Construct from an owned plan content string.
    pub fn new(plan_content: String, theme: MessagesRenderTheme, label_style: Style) -> Self {
        Self {
            plan_content,
            theme,
            prefix: "●",
            label_style,
        }
    }

    fn block(&self) -> RuledBlock {
        let line_count = self.plan_content.lines().count();
        RuledBlock {
            title: "Plan to implement".to_string(),
            title_style: self.theme.accent,
            footer: Some(format!("{line_count} lines")),
            footer_style: self.theme.dim,
            rule_style: self.theme.accent,
        }
    }

    /// Height in rows at `width`.
    pub fn height(&self, width: u16) -> u16 {
        let body = plan_body_text(&self.plan_content, &self.theme);
        ruled_body_height(&body, width)
    }

    /// Paint the widget into `area`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let body = plan_body_text(&self.plan_content, &self.theme);
        paint_ruled_body(
            area,
            buf,
            &self.block(),
            body,
            self.prefix,
            self.label_style,
        );
    }
}

/// Rows a ruled body occupies at `width`: the two rules plus the wrapped body.
fn ruled_body_height(body: &Text<'static>, width: u16) -> u16 {
    let content_width = width.saturating_sub(GUTTER_WIDTH).max(1);
    measure_text_height(body, content_width)
        .max(1)
        .saturating_add(2)
}

/// Paint a ruled block: opening rule across the full row, then the gutter
/// glyph beside the body, then the closing rule.
fn paint_ruled_body(
    area: Rect,
    buf: &mut Buffer,
    block: &RuledBlock,
    body: Text<'static>,
    prefix: &'static str,
    label_style: Style,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Take only the rows this block needs. A caller that hands over a taller
    // area would otherwise leave the closing rule stranded at the bottom of it
    // with blank rows between the body and the rule that is meant to close it.
    let area = Rect {
        height: ruled_body_height(&body, area.width).min(area.height),
        ..area
    };
    let row = |y: u16| Rect {
        x: area.x,
        y,
        width: area.width,
        height: 1,
    };
    paint_line_into(row(area.y), buf, block.top_line(area.width));
    if area.height == 1 {
        return;
    }
    let body_top = area.y.saturating_add(1);
    // The closing rule owns the last row, so the body gets everything between
    // the rules and never paints over either of them.
    let body_height = area.height.saturating_sub(2);
    if body_height > 0 {
        let (gutter_area, content_area) = split_gutter(Rect {
            x: area.x,
            y: body_top,
            width: area.width,
            height: body_height,
        });
        if gutter_area.width > 0 {
            paint_line_into(
                Rect {
                    x: gutter_area.x,
                    y: gutter_area.y,
                    width: gutter_area.width,
                    height: 1,
                },
                buf,
                gutter_label(prefix, label_style),
            );
        }
        if content_area.width > 0 {
            paint_text_into(content_area, buf, body);
        }
    }
    let bottom = area.y.saturating_add(area.height).saturating_sub(1);
    paint_line_into(row(bottom), buf, block.bottom_line(area.width));
}

/// Typed widget for a user row that is an answered `AskUserQuestion`.
///
/// The answer travels as a user message because that is what it is, but it is
/// not something anyone typed — the turn paused, a dialog collected picks, and
/// the turn resumed. So it gets the ruled card and the `●` event glyph
/// rather than the `❯` prompt gutter, and none of the highlight a typed
/// prompt is painted with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAskAnswersBodyWidget {
    /// The parsed question/answer pairs, in the order they were asked.
    pub answered: Vec<AnsweredQuestion>,
    /// Owned theme.
    pub theme: MessagesRenderTheme,
    /// Gutter prefix label (`"●"`).
    pub prefix: &'static str,
    /// Gutter label style.
    pub label_style: Style,
}

impl UserAskAnswersBodyWidget {
    /// Construct from parsed answers.
    pub fn new(
        answered: Vec<AnsweredQuestion>,
        theme: MessagesRenderTheme,
        label_style: Style,
    ) -> Self {
        Self {
            answered,
            theme,
            prefix: "●",
            label_style,
        }
    }

    fn block(&self) -> RuledBlock {
        let count = self.answered.len();
        let noun = if count == 1 { "answer" } else { "answers" };
        RuledBlock {
            title: "Answered questions".to_string(),
            title_style: self.theme.accent,
            footer: Some(format!("{count} {noun}")),
            footer_style: self.theme.dim,
            rule_style: self.theme.dim,
        }
    }

    /// Height in rows at `width`.
    pub fn height(&self, width: u16) -> u16 {
        ruled_body_height(&ask_answers_body_text(&self.answered, &self.theme), width)
    }

    /// Paint the widget into `area`.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let body = ask_answers_body_text(&self.answered, &self.theme);
        paint_ruled_body(
            area,
            buf,
            &self.block(),
            body,
            self.prefix,
            self.label_style,
        );
    }
}

/// Question, then the pick under it, then whatever the pick carried.
fn ask_answers_body_text(
    answered: &[AnsweredQuestion],
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, item) in answered.iter().enumerate() {
        // One blank row between pairs, and none before the first or after the
        // last: the rules already hold the card apart from its neighbours.
        if index > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(item.question.clone(), theme.text)));
        lines.push(Line::from(vec![
            Span::styled("  ❯ ", theme.accent),
            Span::styled(item.answer.clone(), theme.accent),
        ]));
        if let Some(notes) = &item.notes {
            lines.push(Line::from(Span::styled(format!("    {notes}"), theme.dim)));
        }
        if let Some(preview) = &item.preview {
            for raw in preview.lines() {
                lines.push(Line::from(Span::styled(format!("    {raw}"), theme.dim)));
            }
        }
    }
    Text::from(lines)
}

fn plan_body_text(plan_content: &str, theme: &MessagesRenderTheme) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for raw in plan_content.lines() {
        // Crude markdown hint: leading `#` → accent heading, everything else
        // (bullets, paragraphs) keeps the default text style.
        let trimmed = raw.trim_start();
        let style = if trimmed.starts_with('#') {
            theme.accent
        } else {
            theme.text
        };
        lines.push(Line::from(Span::styled(raw.to_owned(), style)));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("(empty plan)", theme.dim)));
    }
    Text::from(lines)
}

fn attachment_rows(projection: &AttachmentProjection, theme: &MessagesRenderTheme) -> Vec<BodyRow> {
    match projection {
        AttachmentProjection::Hidden => Vec::new(),
        AttachmentProjection::Lines(lines) => lines
            .iter()
            .map(|line| {
                BodyRow::Line(Line::from(Span::styled(
                    line.text.clone(),
                    line_style(line, theme),
                )))
            })
            .collect(),
        AttachmentProjection::RelevantMemories(display) => relevant_memories_rows(display, theme),
        AttachmentProjection::QueuedCommand(display) => {
            let mut rows: Vec<BodyRow> = Vec::new();
            if display.margin_top > 0 {
                rows.push(BodyRow::Blank);
            }
            for line in display.prompt_text.lines() {
                rows.push(BodyRow::Line(Line::from(Span::styled(
                    line.to_owned(),
                    theme.text,
                ))));
            }
            for id in &display.image_paste_ids {
                rows.push(BodyRow::Indented {
                    indent: 2,
                    line: Line::from(Span::styled(format!("[image #{id}]"), theme.dim)),
                });
            }
            rows
        }
        AttachmentProjection::Diagnostics(diagnostics) => diagnostics_rows(diagnostics, theme),
        AttachmentProjection::TaskStatus(status) => task_status_rows(status, theme),
        AttachmentProjection::TeammateMailbox(mailbox) => mailbox_rows(mailbox, theme),
    }
}

fn line_style(line: &AttachmentLineDisplay, theme: &MessagesRenderTheme) -> Style {
    match line.tone {
        Some(AttachmentTone::Error) => theme.error,
        Some(AttachmentTone::Warning) => theme.warning,
        None if line.dim => theme.dim,
        None => theme.text,
    }
}

fn relevant_memories_rows(
    display: &AttachmentRelevantMemoriesDisplay,
    theme: &MessagesRenderTheme,
) -> Vec<BodyRow> {
    let mut rows: Vec<BodyRow> = Vec::new();
    if display.margin_top > 0 {
        rows.push(BodyRow::Blank);
    }
    let expand_hint = display
        .show_expand_hint
        .then(|| {
            format!(
                " ({})",
                format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
            )
        })
        .unwrap_or_default();
    let summary = format!(
        "Recalled {} {}{}",
        display.count, display.count_word, expand_hint
    );
    rows.push(BodyRow::Indented {
        indent: u16::from(display.gutter_width),
        line: Line::from(Span::styled(summary, theme.dim)),
    });
    if !display.show_entries {
        return rows;
    }
    for entry in &display.entries {
        match &entry.transcript_body {
            Some(body) => {
                let body_text = Text::from(
                    body.lines()
                        .map(|line| Line::from(Span::styled(line.to_owned(), theme.text)))
                        .collect::<Vec<_>>(),
                );
                rows.push(BodyRow::TranscriptBlock {
                    header: Line::from(Span::styled(entry.basename.clone(), theme.dim)),
                    body: body_text,
                    body_padding: u16::from(display.transcript_padding_left),
                });
            }
            None => {
                rows.push(BodyRow::Indented {
                    indent: u16::from(display.gutter_width),
                    line: Line::from(Span::styled(entry.basename.clone(), theme.dim)),
                });
            }
        }
    }
    rows
}

fn diagnostics_rows(
    projection: &DiagnosticsProjection,
    theme: &MessagesRenderTheme,
) -> Vec<BodyRow> {
    match projection {
        DiagnosticsProjection::Summary {
            total_issues,
            file_count,
            issue_word,
            file_word,
        } => vec![BodyRow::Line(Line::from(Span::styled(
            format!("{total_issues} {issue_word} across {file_count} {file_word}"),
            theme.warning,
        )))],
        DiagnosticsProjection::Verbose { files } => {
            let mut rows = Vec::new();
            for file in files {
                rows.push(BodyRow::Line(Line::from(Span::styled(
                    format!("{} {}:", file.display_path, file.uri_suffix),
                    theme.dim,
                ))));
                for diagnostic in &file.diagnostics {
                    rows.push(BodyRow::Indented {
                        indent: 2,
                        line: Line::from(Span::styled(diagnostic.clone(), theme.error)),
                    });
                }
            }
            rows
        }
    }
}

fn task_status_rows(
    status: &AttachmentTaskStatusDisplay,
    theme: &MessagesRenderTheme,
) -> Vec<BodyRow> {
    match status {
        AttachmentTaskStatusDisplay::Generic {
            dot_glyph,
            description,
            status_text,
            ..
        } => vec![BodyRow::Line(Line::from(vec![
            Span::styled(format!("{dot_glyph} "), theme.accent),
            Span::styled("Task \"", theme.dim),
            Span::styled(description.clone(), theme.text),
            Span::styled(format!("\" {status_text}"), theme.dim),
        ]))],
        AttachmentTaskStatusDisplay::Teammate {
            dot_glyph,
            agent_name,
            status_text,
            ..
        } => vec![BodyRow::Line(Line::from(vec![
            Span::styled(format!("{dot_glyph} "), theme.accent),
            Span::styled("Teammate ", theme.dim),
            Span::styled(format!("@{agent_name}"), theme.accent),
            Span::styled(format!(" {status_text}"), theme.dim),
        ]))],
    }
}

fn mailbox_rows(
    mailbox: &AttachmentTeammateMailboxDisplay,
    theme: &MessagesRenderTheme,
) -> Vec<BodyRow> {
    let mut rows = Vec::new();
    for item in &mailbox.items {
        match item {
            AttachmentTeammateMailboxItemDisplay::TaskAssignment {
                dot_glyph,
                task_id,
                subject,
                from,
            } => {
                let mut spans = vec![
                    Span::styled(format!("{dot_glyph} "), theme.accent),
                    Span::styled("Task assigned: ", theme.text),
                    Span::styled(format!("#{task_id}"), theme.accent),
                ];
                if let Some(subject) = subject.as_ref().filter(|subject| !subject.is_empty()) {
                    spans.push(Span::raw(" - "));
                    spans.push(Span::styled(subject.clone(), theme.text));
                }
                spans.push(Span::styled(format!(" (from {from})"), theme.dim));
                rows.push(BodyRow::Line(Line::from(spans)));
            }
            AttachmentTeammateMailboxItemDisplay::PlanApproval(renderable) => {
                rows.extend(plan_approval_rows(renderable, theme));
            }
            AttachmentTeammateMailboxItemDisplay::Plain(plain) => {
                rows.extend(teammate_plain_rows(plain, theme));
            }
        }
    }
    rows
}

fn plan_approval_rows(
    renderable: &PlanApprovalRenderable,
    theme: &MessagesRenderTheme,
) -> Vec<BodyRow> {
    match renderable {
        PlanApprovalRenderable::Request(request) => {
            let mut rows = vec![BodyRow::Line(Line::from(Span::styled(
                request.title.clone(),
                theme.accent,
            )))];
            for line in request.plan_content.lines() {
                rows.push(BodyRow::Indented {
                    indent: 2,
                    line: Line::from(Span::styled(line.to_owned(), theme.text)),
                });
            }
            rows.push(BodyRow::Line(Line::from(Span::styled(
                format!("Plan file: {}", request.plan_file_path),
                theme.dim,
            ))));
            rows
        }
        PlanApprovalRenderable::Response(response) => match response {
            PlanApprovalResponseDisplay::Approved { title, body } => vec![
                BodyRow::Line(Line::from(Span::styled(title.clone(), theme.accent))),
                BodyRow::Indented {
                    indent: 2,
                    line: Line::from(Span::styled(*body, theme.text)),
                },
            ],
            PlanApprovalResponseDisplay::Rejected {
                title,
                feedback,
                footer,
            } => {
                let mut rows = vec![BodyRow::Line(Line::from(Span::styled(
                    title.clone(),
                    theme.error,
                )))];
                if let Some(feedback) = feedback {
                    rows.push(BodyRow::Indented {
                        indent: 2,
                        line: Line::from(Span::styled(format!("Feedback: {feedback}"), theme.text)),
                    });
                }
                rows.push(BodyRow::Line(Line::from(Span::styled(*footer, theme.dim))));
                rows
            }
        },
    }
}

fn teammate_plain_rows(
    plain: &TeammateMessageContentDisplay,
    theme: &MessagesRenderTheme,
) -> Vec<BodyRow> {
    let name_style = if plain.color.is_some() {
        theme.accent
    } else {
        theme.text
    };
    let mut header = vec![Span::styled(
        format!("@{}>", plain.display_name),
        name_style,
    )];
    if let Some(summary) = plain.summary.as_ref() {
        header.push(Span::raw(format!(" {summary}")));
    }
    let mut rows = vec![BodyRow::Line(Line::from(header))];
    if plain.is_transcript_mode {
        for line in plain.content.lines() {
            rows.push(BodyRow::Indented {
                indent: 2,
                line: Line::from(Span::styled(line.to_owned(), theme.text)),
            });
        }
    }
    rows
}

fn progress_rows(
    progress: &rebon_render::AssistantToolProgressDisplay,
    theme: &MessagesRenderTheme,
) -> Vec<BodyRow> {
    let mut rows = Vec::new();
    if let Some(line) = progress.hook_progress_message.as_ref() {
        for part in line.lines() {
            rows.push(BodyRow::Indented {
                indent: 2,
                line: Line::from(Span::styled(part.to_string(), theme.dim)),
            });
        }
    }
    if let Some(line) = progress.progress_message.as_ref() {
        for part in line.lines() {
            rows.push(BodyRow::Indented {
                indent: 2,
                line: Line::from(Span::styled(part.to_string(), theme.dim)),
            });
        }
    }
    rows
}

fn assistant_tool_use_rows(
    projection: &AssistantToolUseProjection,
    theme: &MessagesRenderTheme,
) -> Vec<BodyRow> {
    match projection {
        AssistantToolUseProjection::Hidden(_reason) => {
            // Hidden tools (EnterPlanMode, AskUserQuestion, TodoWrite, etc.)
            // should produce no visible rows in the transcript.
            vec![]
        }
        AssistantToolUseProjection::Transparent(progress) => progress_rows(progress, theme),
        AssistantToolUseProjection::Row(display) => {
            let mut spans = Vec::new();
            if let Some(leading) = display.header.leading.as_ref() {
                spans.push(Span::styled(
                    assistant_tool_use_leading_label(leading),
                    match leading {
                        rebon_render::AssistantToolLeadingDisplay::QueuedDot { .. } => theme.dim,
                        rebon_render::AssistantToolLeadingDisplay::Loader {
                            is_error: true,
                            ..
                        } => theme.error,
                        rebon_render::AssistantToolLeadingDisplay::Loader {
                            is_unresolved: true,
                            ..
                        } => theme.warning,
                        rebon_render::AssistantToolLeadingDisplay::Loader { .. } => theme.text,
                    },
                ));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                display.header.user_facing_name.clone(),
                tool_name_style(theme),
            ));
            if let Some(message) = display.header.rendered_message.as_ref() {
                spans.push(Span::styled(format!(" ({message})"), theme.dim));
            }
            if let Some(tag) = display.header.tag.as_ref() {
                spans.push(Span::styled(format!(" [{tag}]"), theme.warning));
            }

            let mut rows = vec![BodyRow::Line(Line::from(spans))];
            if let Some(progress) = display.progress.as_ref() {
                match progress {
                    AssistantToolSecondaryDisplay::WaitingForPermission { message, .. } => rows
                        .push(BodyRow::Indented {
                            indent: 2,
                            line: Line::from(Span::styled(*message, theme.warning)),
                        }),
                    AssistantToolSecondaryDisplay::Progress(progress) => {
                        rows.extend(progress_rows(progress, theme));
                    }
                }
            }
            if let Some(message) = display.queued_message.as_ref() {
                rows.push(BodyRow::Indented {
                    indent: 2,
                    line: Line::from(Span::styled(message.clone(), theme.dim)),
                });
            }
            rows
        }
    }
}

fn thinking_expand_hint_text() -> String {
    format!(
        "({})",
        format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
    )
}

fn render_thinking_projection_with_markdown(
    projection: &AssistantThinkingMessageProjection,
    theme: &MessagesRenderTheme,
    markdown_width: u16,
) -> Text<'static> {
    match projection {
        AssistantThinkingMessageProjection::Collapsed(display) => {
            let md_theme = crate::markdown_render::MarkdownTheme::from_messages(theme);
            let mut text = crate::markdown_render::render_markdown_blocks_with_width(
                &display.markdown,
                &md_theme,
                markdown_width as usize,
            );
            if display.show_expand_hint {
                let hint = thinking_expand_hint_text();
                if let Some(first) = text.lines.first_mut() {
                    first
                        .spans
                        .extend([Span::styled(" ", theme.dim), Span::styled(hint, theme.dim)]);
                } else {
                    text.lines.push(Line::from(Span::styled(hint, theme.dim)));
                }
            }
            text
        }
        AssistantThinkingMessageProjection::Expanded(display) => {
            let mut lines = Vec::new();
            if display.show_label {
                lines.push(Line::from(Span::styled(display.label, theme.dim)));
            }
            let md_theme = crate::markdown_render::MarkdownTheme::from_messages(theme);
            let markdown = crate::markdown_render::render_markdown_blocks_with_width(
                &display.markdown,
                &md_theme,
                markdown_width.saturating_sub(display.padding_left as u16) as usize,
            );
            if display.show_label && display.gap > 0 && !markdown.lines.is_empty() {
                lines.push(Line::default());
            }
            if display.padding_left > 0 {
                let padding = " ".repeat(display.padding_left as usize);
                lines.extend(markdown.lines.into_iter().map(|line| {
                    let mut spans = vec![Span::raw(padding.clone())];
                    spans.extend(line.spans);
                    Line::from(spans)
                }));
            } else {
                lines.extend(markdown.lines);
            }
            if display.show_expand_hint {
                lines.push(Line::from(Span::styled(
                    thinking_expand_hint_text(),
                    theme.dim,
                )));
            }
            Text::from(lines)
        }
        _ => render_assistant_thinking_projection(projection, theme),
    }
}

fn assistant_thinking_rows(
    projection: &AssistantThinkingMessageProjection,
    redacted_placeholder: bool,
    add_margin: bool,
    theme: &MessagesRenderTheme,
) -> Vec<BodyRow> {
    let mut rows = Vec::new();
    if add_margin {
        rows.push(BodyRow::Blank);
    }
    if redacted_placeholder {
        rows.push(BodyRow::Line(Line::from(Span::styled(
            "[redacted thinking]",
            theme.dim,
        ))));
        return rows;
    }
    match projection {
        AssistantThinkingMessageProjection::Hidden => rows,
        AssistantThinkingMessageProjection::Collapsed(display) => {
            rows.push(BodyRow::Line(Line::from(Span::styled(
                display.markdown.clone(),
                theme.dim,
            ))));
            rows
        }
        AssistantThinkingMessageProjection::Expanded(display) => {
            if display.show_label {
                rows.push(BodyRow::Line(Line::from(Span::styled(
                    display.label,
                    theme.dim,
                ))));
            }
            for line in display.markdown.lines() {
                rows.push(BodyRow::Indented {
                    indent: display.padding_left as u16,
                    line: Line::from(Span::styled(line.to_owned(), theme.dim)),
                });
            }
            rows
        }
    }
}

fn user_tool_result_layout(
    projection: &UserToolResultProjection,
    result_body: Option<&str>,
    theme: &MessagesRenderTheme,
) -> (Vec<BodyRow>, Option<BorderedBlock>) {
    match projection {
        UserToolResultProjection::MissingToolUse => (
            vec![BodyRow::Line(Line::from(Span::styled(
                "[missing tool use]",
                theme.warning,
            )))],
            None,
        ),
        UserToolResultProjection::Canceled => (
            vec![BodyRow::Line(Line::from(Span::styled(
                "[tool canceled]",
                theme.warning,
            )))],
            None,
        ),
        UserToolResultProjection::RejectedPlan { plan } => rejected_plan_layout(plan, theme),
        UserToolResultProjection::RejectedToolUse => (
            vec![BodyRow::Line(Line::from(Span::styled(
                "[rejected tool use]",
                theme.error,
            )))],
            None,
        ),
        UserToolResultProjection::Error(error) => user_tool_error_layout(error, theme),
        UserToolResultProjection::Success(success) => {
            let mut rows = vec![BodyRow::Line(Line::from(Span::styled(
                format!("[tool result {}]", success.tool_use_id),
                theme.accent,
            )))];
            if let Some(rule) = success.classifier_rule.as_ref() {
                rows.push(BodyRow::Line(Line::from(Span::styled(
                    rule.clone(),
                    theme.dim,
                ))));
            }
            if let Some(reason) = success.yolo_reason.as_ref() {
                rows.push(BodyRow::Line(Line::from(Span::styled(
                    reason.clone(),
                    theme.warning,
                ))));
            }
            let block = result_body.filter(|body| !body.is_empty()).map(|body| {
                match parse_bash_tool_result_json(body) {
                    Some(parsed) => bash_tool_result_block(&parsed, success, theme),
                    None => bordered_text_block(" Result ".to_string(), body, theme.text, theme),
                }
            });
            (rows, block)
        }
    }
}

fn bash_tool_result_block(
    parsed: &ParsedBashToolResult,
    success: &UserToolSuccessProjection,
    theme: &MessagesRenderTheme,
) -> BorderedBlock {
    let terminal_columns = success
        .width
        .parse::<i32>()
        .ok()
        .and_then(|value| usize::try_from(value.max(0)).ok())
        .unwrap_or(80);
    let input = BashToolResultInput {
        stdout: parsed.stdout.clone(),
        stderr: parsed.stderr.clone(),
        is_image: parsed.is_image,
        return_code_interpretation: parsed.return_code_interpretation.clone(),
        no_output_expected: parsed.no_output_expected,
        background_task_id: parsed.background_task_id.clone(),
        timeout_ms: None,
        verbose: success.verbose,
        supports_hyperlinks: false,
        terminal_columns,
        in_virtual_list: false,
        expand_shell_output: ExpandShellOutputContextValue::default(),
    };
    let display = project_bash_tool_result_message(&input);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut stdout_line_count = 0usize;
    let mut stderr_line_count = 0usize;
    let mut has_image = false;

    for block in display.blocks {
        match block {
            BashToolResultBlock::ImagePlaceholder => {
                has_image = true;
                lines.push(Line::from(Span::styled(
                    IMAGE_PLACEHOLDER.to_string(),
                    theme.dim,
                )));
            }
            BashToolResultBlock::Stdout(output) => {
                for line in output.formatted.lines() {
                    lines.push(Line::from(Span::styled(line.to_owned(), theme.text)));
                    stdout_line_count = stdout_line_count.saturating_add(1);
                }
            }
            BashToolResultBlock::Stderr(output) => {
                for line in output.formatted.lines() {
                    lines.push(Line::from(Span::styled(line.to_owned(), theme.error)));
                    stderr_line_count = stderr_line_count.saturating_add(1);
                }
            }
            BashToolResultBlock::CwdResetWarning(warning) => {
                lines.push(Line::from(Span::styled(warning, theme.dim)));
            }
            BashToolResultBlock::EmptyOutputFallback(message) => {
                lines.push(Line::from(Span::styled(message, theme.dim)));
            }
            BashToolResultBlock::TimeoutDisplay(time) => {
                lines.push(Line::from(Span::styled(time.text, theme.dim)));
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            EMPTY_OUTPUT_PLACEHOLDER.to_string(),
            theme.dim,
        )));
    }

    let mut subtitle_parts: Vec<String> = Vec::new();
    if stdout_line_count > 0 {
        subtitle_parts.push(format!(
            "{stdout_line_count} stdout {}",
            if stdout_line_count == 1 {
                "line"
            } else {
                "lines"
            }
        ));
    }
    if stderr_line_count > 0 {
        subtitle_parts.push(format!(
            "{stderr_line_count} stderr {}",
            if stderr_line_count == 1 {
                "line"
            } else {
                "lines"
            }
        ));
    }
    if subtitle_parts.is_empty() {
        let total = lines.len();
        subtitle_parts.push(format!(
            "{total} {}",
            if total == 1 { "line" } else { "lines" }
        ));
    }

    let has_stderr = stderr_line_count > 0;
    let title_style = if has_stderr {
        theme.error
    } else {
        theme.accent
    };
    let title = if has_image {
        " Bash (image) "
    } else {
        " Bash Output "
    };

    BorderedBlock {
        title: title.to_string(),
        title_style,
        title_bottom: Some(format!(" {} ", subtitle_parts.join(" · "))),
        title_bottom_style: theme.dim,
        body: BorderedBlockBody::SingleColumn(Text::from(lines)),
        border_style: title_style,
    }
}

fn user_tool_error_layout(
    error: &UserToolErrorProjection,
    theme: &MessagesRenderTheme,
) -> (Vec<BodyRow>, Option<BorderedBlock>) {
    match error {
        UserToolErrorProjection::Interrupted => (
            vec![BodyRow::Line(Line::from(Span::styled(
                "[interrupted by user]",
                theme.warning,
            )))],
            None,
        ),
        UserToolErrorProjection::RejectedPlan { plan } => rejected_plan_layout(plan, theme),
        UserToolErrorProjection::RejectedToolUse => (
            vec![BodyRow::Line(Line::from(Span::styled(
                "[rejected tool use]",
                theme.error,
            )))],
            None,
        ),
        UserToolErrorProjection::ClassifierDenied => (
            vec![BodyRow::Line(Line::from(Span::styled(
                "[classifier denied]",
                theme.warning,
            )))],
            None,
        ),
        UserToolErrorProjection::Fallback { result, .. }
        | UserToolErrorProjection::Custom { result, .. } => (
            Vec::new(),
            Some(bordered_text_block(
                " Tool Error ".to_string(),
                result,
                theme.error,
                theme,
            )),
        ),
    }
}

fn rejected_plan_layout(
    plan: &str,
    theme: &MessagesRenderTheme,
) -> (Vec<BodyRow>, Option<BorderedBlock>) {
    (
        vec![BodyRow::Line(Line::from(Span::styled(
            "[rejected plan]",
            theme.error,
        )))],
        Some(bordered_text_block(
            " Rejected plan ".to_string(),
            plan,
            theme.text,
            theme,
        )),
    )
}

fn assistant_tool_use_leading_label(leading: &rebon_render::AssistantToolLeadingDisplay) -> String {
    match leading {
        rebon_render::AssistantToolLeadingDisplay::QueuedDot { glyph, .. } => glyph.clone(),
        rebon_render::AssistantToolLeadingDisplay::Loader {
            is_error,
            is_unresolved,
            ..
        } => {
            if *is_error {
                "ERR".into()
            } else if *is_unresolved {
                "..".into()
            } else {
                "OK".into()
            }
        }
    }
}

#[cfg(test)]
mod tests;
