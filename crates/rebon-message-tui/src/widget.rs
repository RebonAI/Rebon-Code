//! Widget-level composition for a message row.
//!
//! The body dispatches to typed sub-widgets defined in
//! [`crate::widget_subtree`] for richer visual layouts (attachment / grouped
//! tool use / collapsed read-search). Rows that have no typed widget
//! fall back to the plain `Paragraph<Text>` body painting path.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use rebon_render::{
    ask_user_answers::{parse_answered_questions, AnsweredQuestion},
    project_system_text_message, SystemTextMessageInput, SystemTextProjectionInput,
    UserToolResultInput,
};

/// The answers a user row carries, when it is an answered `AskUserQuestion`.
///
/// The answer has no marker of its own on the wire — it is a plain text user
/// message, so that the model reads it as one — which leaves its text as the
/// only thing to recognise it by. `parse_answered_questions` rejects anything
/// that is not exactly what the formatter emits, so a prompt that merely opens
/// with the same words keeps the ordinary rendering.
fn ask_user_answers_of(user: &rebon_render::UserMessage) -> Option<Vec<AnsweredQuestion>> {
    let [rebon_render::UserContentBlock::Text { text }] = user.content.as_slice() else {
        return None;
    };
    parse_answered_questions(text.as_str())
}

use crate::{
    render_message, render_metadata_line,
    widget_subtree::{
        assistant_text_child_theme_for, child_theme_for, AssistantTextBodyWidget,
        AssistantThinkingBodyWidget, AssistantToolUseBodyWidget, AttachmentBodyWidget,
        MessageBodyKind, SystemTextBodyWidget, UserAskAnswersBodyWidget, UserTextBodyWidget,
        UserTextPlanBodyWidget, UserToolResultBodyWidget,
    },
    MessageRenderTheme,
};
use rebon_render::{
    project::UserBlockProjectInput, project_assistant_block, project_user_block,
    AssistantBlockProjection, AssistantContentBlock, MessageRow, MetadataLabelProjection,
    RenderMessageInput, UserBlockProjection,
};

/// Optional border decoration wrapped around a `RenderedMessageWidget`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageBorderDecoration {
    /// Border title rendered in the top-left corner of the frame.
    pub title: String,
    /// Border / title style.
    pub style: Style,
}

/// One assistant-markdown text layer painted into a concrete content area.
///
/// Hyperlink and formula ranges address the logical lines and UTF-8 byte
/// offsets in [`Self::text`]. `area` is the actual post-border, post-metadata,
/// post-gutter rectangle used for painting, so interaction/graphics code can
/// translate those logical ranges into terminal regions with the same wrapping
/// width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyperlinkPaintLayer {
    /// Actual content rectangle used to paint `text`.
    pub area: Rect,
    /// Styled text painted into [`Self::area`].
    pub text: ratatui::text::Text<'static>,
    /// Hyperlink ranges addressing [`Self::text`].
    pub hyperlinks: Vec<crate::HyperlinkRange>,
    /// Formula metadata and assets addressing [`Self::text`].
    pub formulas: Vec<crate::RenderedFormula>,
}

/// Fully rendered message widget payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMessageWidget {
    /// Optional metadata row rendered above the body.
    pub metadata: Option<Line<'static>>,
    /// Main body — either a flat text body or a typed sub-widget.
    pub body: MessageBodyKind,
    /// Optional border frame drawn around the metadata + body.
    pub border: Option<MessageBorderDecoration>,
}

impl RenderedMessageWidget {
    /// Build a widget payload from the message renderer plus optional metadata.
    pub fn new(
        input: &RenderMessageInput,
        theme: &MessageRenderTheme,
        width: u16,
        timestamp: Option<&MetadataLabelProjection>,
        model: Option<&MetadataLabelProjection>,
    ) -> Self {
        Self::new_with_markdown_options(
            input,
            theme,
            width,
            timestamp,
            model,
            crate::MarkdownRenderOptions::default(),
        )
    }

    /// Build a widget payload with explicit Markdown extension options.
    pub fn new_with_markdown_options(
        input: &RenderMessageInput,
        theme: &MessageRenderTheme,
        width: u16,
        timestamp: Option<&MetadataLabelProjection>,
        model: Option<&MetadataLabelProjection>,
        markdown_options: crate::MarkdownRenderOptions,
    ) -> Self {
        Self {
            metadata: render_metadata_line(width, timestamp, model, theme),
            body: build_body(input, theme, width, markdown_options),
            border: None,
        }
    }

    /// Attach a border decoration to the widget (for selected rows etc.).
    pub fn with_border(mut self, border: MessageBorderDecoration) -> Self {
        self.border = Some(border);
        self
    }

    /// Height in rows when painted at `width`.
    pub fn height(&self, width: u16) -> u16 {
        let metadata = u16::from(self.metadata.is_some());
        let inner_width = if self.border.is_some() {
            width.saturating_sub(2).max(1)
        } else {
            width
        };
        let body = self.body.height(inner_width);
        let border_pad = if self.border.is_some() { 2 } else { 0 };
        metadata + body + border_pad
    }

    /// Paint into a ratatui buffer, discarding hyperlink paint metadata.
    pub fn render_to_buffer(self, area: Rect, buf: &mut Buffer) {
        let _ = self.render_to_buffer_with_hyperlinks(area, buf);
    }

    /// Paint into a ratatui buffer and return assistant-markdown hyperlink
    /// layers with their actual content rectangles.
    ///
    /// Each returned layer contains the same styled text that was painted and
    /// ranges relative to that text. Callers may therefore perform hyperlink
    /// hit-testing without changing the compatible [`Self::render_to_buffer`]
    /// path.
    pub fn render_to_buffer_with_hyperlinks(
        self,
        area: Rect,
        buf: &mut Buffer,
    ) -> Vec<HyperlinkPaintLayer> {
        let mut layers = Vec::new();
        if area.height == 0 || area.width == 0 {
            return layers;
        }

        // Reset every cell in the widget's owned rect before painting.
        // The body subtree is composed of typed sub-widgets (attachment,
        // grouped tool use, collapsed read/search, plain paragraph, …)
        // that each only write into the cells their current content
        // occupies. When a previous frame painted a taller / wider body
        // here (e.g. a verbose Read card collapsing to a shorter body, or a
        // long tool body trimmed between deltas), ratatui's diff back-end
        // would otherwise leave the stale glyphs untouched and they
        // bleed through as residue. Resetting cells (without forcing a
        // theme background) keeps the terminal's natural background.
        clear_widget_area(buf, area);

        let content_area = if let Some(border) = &self.border {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(border.style)
                .title(Span::styled(border.title.clone(), border.style));
            let inner = block.inner(area);
            block.render(area, buf);
            inner
        } else {
            area
        };

        if content_area.width == 0 || content_area.height == 0 {
            return layers;
        }

        let mut y = content_area.y;
        if let Some(metadata) = self.metadata {
            let metadata_area = Rect {
                x: content_area.x,
                y,
                width: content_area.width,
                height: 1,
            };
            Paragraph::new(metadata).render(metadata_area, buf);
            y = y.saturating_add(1);
        }

        if y >= content_area.y.saturating_add(content_area.height) {
            return layers;
        }

        let body_area = Rect {
            x: content_area.x,
            y,
            width: content_area.width,
            height: content_area
                .y
                .saturating_add(content_area.height)
                .saturating_sub(y),
        };
        self.body
            .render_to_buffer_with_hyperlinks(body_area, buf, &mut layers);
        layers
    }
}

impl Widget for RenderedMessageWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.render_to_buffer(area, buf);
    }
}

/// Reset every cell in `area` to ratatui's default state — empty
/// symbol, no fg/bg, no modifiers. Used to neutralize leftover
/// glyphs from prior frames before the message body repaints. We
/// deliberately do NOT apply the theme background here so unpainted
/// cells inherit the terminal's natural background; the terminal paint
/// loop's own buffer-clearing relies on the same invariant.
fn clear_widget_area(buf: &mut Buffer, area: Rect) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
}

fn tool_gutter_style(
    id: Option<&str>,
    input: &RenderMessageInput,
    theme: &MessageRenderTheme,
) -> Style {
    let Some(id) = id else {
        return theme.assistant;
    };
    if input
        .errored_tool_use_ids
        .iter()
        .any(|tool_id| tool_id == id)
    {
        theme.error
    } else if input
        .in_progress_tool_use_ids
        .iter()
        .any(|tool_id| tool_id == id)
        && input.frame_time_ms % 2000 >= 1000
    {
        theme.assistant.add_modifier(Modifier::DIM)
    } else {
        theme.assistant
    }
}

fn build_body(
    input: &RenderMessageInput,
    theme: &MessageRenderTheme,
    width: u16,
    markdown_options: crate::MarkdownRenderOptions,
) -> MessageBodyKind {
    let markdown_width = width
        .saturating_sub(crate::widget_subtree::GUTTER_WIDTH)
        .max(1);
    match &input.message {
        MessageRow::Attachment(attachment) => match attachment.attachment.as_ref() {
            Some(payload) => MessageBodyKind::Attachment(AttachmentBodyWidget::from_input(
                payload,
                input.verbose,
                input.is_transcript_mode,
                input.add_margin,
                child_theme_for(theme),
                theme.placeholder,
            )),
            None => MessageBodyKind::Text(render_message(input, theme)),
        },
        MessageRow::User(user) if !user.is_compact_summary && user.plan_content.is_some() => {
            // Any user text row that carries `plan_content` gets the typed
            // plan widget; everything else falls through to the text path.
            MessageBodyKind::UserTextPlan(UserTextPlanBodyWidget::new(
                user.plan_content.clone().unwrap_or_default(),
                child_theme_for(theme),
                theme.user,
            ))
        }
        MessageRow::User(user)
            if !user.is_compact_summary
                && user.plan_content.is_none()
                && ask_user_answers_of(user).is_some() =>
        {
            // An answered AskUserQuestion is a user message only because that
            // is how the answer reaches the model; nobody typed it, so it gets
            // its own card instead of the prompt gutter and highlight.
            MessageBodyKind::UserAskAnswers(UserAskAnswersBodyWidget::new(
                ask_user_answers_of(user).unwrap_or_default(),
                child_theme_for(theme),
                theme.metadata,
            ))
        }
        MessageRow::User(user) if user.content.len() == 1 => {
            let block = user.content[0].clone();
            let projection = project_user_block(&UserBlockProjectInput {
                message: user.clone(),
                block: block.clone(),
                image_id: None,
                add_margin: input.add_margin,
                verbose: input.verbose,
                style_condensed: input.style_condensed,
                is_user_continuation: input.is_user_continuation,
                is_transcript_mode: input.is_transcript_mode,
                terminal_columns: input.terminal_columns,
            });
            match (&block, projection) {
                (
                    rebon_render::UserContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    },
                    UserBlockProjection::ToolResult {
                        verbose,
                        width,
                        is_transcript_mode,
                        ..
                    },
                ) => MessageBodyKind::UserToolResult(UserToolResultBodyWidget::from_raw(
                    UserToolResultInput {
                        tool_use_id: tool_use_id.clone().unwrap_or_else(|| "tool".to_string()),
                        content: content.clone().unwrap_or_default(),
                        is_error: *is_error,
                        tool_exists: tool_use_id.is_some(),
                        tool_has_custom_reject_renderer: false,
                        tool_has_custom_error_renderer: false,
                        renders_as_assistant_text: false,
                        input_summary: None,
                        verbose,
                        is_transcript_mode,
                        width: width.to_string(),
                        classifier_rule: None,
                        yolo_reason: None,
                        classifier_denial: false,
                    },
                    child_theme_for(theme),
                    if *is_error { theme.error } else { theme.user },
                )),
                (rebon_render::UserContentBlock::Text { text }, _) => {
                    MessageBodyKind::UserText(UserTextBodyWidget::from_raw(
                        text,
                        input.add_margin,
                        input.verbose,
                        user.plan_content.clone(),
                        user.timestamp.clone(),
                        input.is_transcript_mode,
                        child_theme_for(theme),
                        theme.user,
                    ))
                }
                _ => MessageBodyKind::Text(render_message(input, theme)),
            }
        }
        MessageRow::Assistant(assistant) if assistant.content.len() == 1 => {
            build_single_content_assistant_body(
                input,
                theme,
                markdown_width,
                markdown_options,
                assistant,
            )
        }
        MessageRow::System(system) if system.stop_hook_summary.is_some() => {
            let display = *system.stop_hook_summary.clone().unwrap();
            MessageBodyKind::SystemText(SystemTextBodyWidget::from_stop_hook_summary(
                display,
                child_theme_for(theme),
                theme.system,
            ))
        }
        MessageRow::System(system)
            if matches!(system.subtype, rebon_render::SystemSubtype::Other) =>
        {
            let projection = project_system_text_message(&SystemTextMessageInput {
                add_margin: input.add_margin,
                verbose: input.verbose,
                is_transcript_mode: input.is_transcript_mode,
                background: None,
                terminal_columns: Some(input.terminal_columns),
                message: SystemTextProjectionInput::Generic {
                    subtype: system
                        .raw_subtype
                        .clone()
                        .unwrap_or_else(|| "informational".into()),
                    level: system.level.clone().unwrap_or_else(|| "info".into()),
                    content: Some(system.content.clone()),
                },
            });
            if matches!(projection, rebon_render::SystemTextProjection::Hidden) {
                MessageBodyKind::Hidden
            } else {
                MessageBodyKind::SystemText(SystemTextBodyWidget::from_projection(
                    projection,
                    child_theme_for(theme),
                    theme.system,
                ))
            }
        }
        MessageRow::Assistant(assistant) if assistant.content.len() > 1 => {
            let mut blocks: Vec<MessageBodyKind> = Vec::new();
            for (idx, block) in assistant.content.iter().enumerate() {
                let add_margin = input.add_margin || !blocks.is_empty();
                let projection = project_assistant_block(
                    block,
                    add_margin,
                    input.verbose,
                    input.is_transcript_mode,
                    input.last_thinking_block_id.as_deref(),
                    &format!("{}:{idx}", assistant.uuid),
                );
                if let Some(block) = build_single_assistant_block(
                    block,
                    projection,
                    input,
                    theme,
                    width,
                    markdown_options,
                ) {
                    if block.height(input.terminal_columns) > 0 {
                        blocks.push(block);
                    }
                }
            }
            if blocks.is_empty() {
                MessageBodyKind::Text(render_message(input, theme))
            } else {
                MessageBodyKind::CompositeBlocks(blocks)
            }
        }
        _ => MessageBodyKind::Text(render_message(input, theme)),
    }
}

/// Convert a single assistant block + projection into a typed widget.
fn build_single_assistant_block(
    block: &AssistantContentBlock,
    projection: AssistantBlockProjection,
    input: &RenderMessageInput,
    theme: &MessageRenderTheme,
    width: u16,
    markdown_options: crate::MarkdownRenderOptions,
) -> Option<MessageBodyKind> {
    let markdown_width = width
        .saturating_sub(crate::widget_subtree::GUTTER_WIDTH)
        .max(1);
    match (block, projection) {
        (
            AssistantContentBlock::ToolUse {
                id,
                name,
                input_summary,
                diff,
                body_lines,
            },
            AssistantBlockProjection::ToolUse { add_margin },
        ) => {
            if let Some((path, old_text, new_text)) = diff {
                use rebon_render::file_edit::{
                    project_file_edit_updated, FileEditUpdatedInput, StructuredPatchHunk,
                };
                let mut hunk_lines = Vec::new();
                if let Some(old) = old_text {
                    for line in old.lines() {
                        hunk_lines.push(format!("-{line}"));
                    }
                }
                for line in new_text.lines() {
                    hunk_lines.push(format!("+{line}"));
                }
                let hunks = vec![StructuredPatchHunk { lines: hunk_lines }];
                let projection = project_file_edit_updated(&FileEditUpdatedInput {
                    file_path: path.clone(),
                    structured_patch: hunks,
                    first_line: None,
                    file_content: None,
                    style_condensed: input.style_condensed,
                    verbose: input.verbose,
                    preview_hint: None,
                    columns: input.terminal_columns as usize,
                });
                Some(MessageBodyKind::FileEdit(
                    crate::widget_subtree::FileEditBodyWidget::from_updated(
                        projection,
                        child_theme_for(theme),
                        theme.assistant,
                    ),
                ))
            } else {
                Some(MessageBodyKind::AssistantToolUse(
                    AssistantToolUseBodyWidget::from_raw(
                        id.as_deref(),
                        name.as_deref(),
                        input_summary.as_deref(),
                        body_lines,
                        add_margin,
                        child_theme_for(theme),
                        tool_gutter_style(id.as_deref(), input, theme),
                    ),
                ))
            }
        }
        (
            AssistantContentBlock::Text { text },
            AssistantBlockProjection::Text {
                add_margin,
                verbose,
            },
        ) => Some(MessageBodyKind::AssistantText(
            AssistantTextBodyWidget::from_raw_with_options(
                &crate::render::normalize_assistant_display_text(text),
                add_margin,
                verbose,
                !matches!(&input.message, MessageRow::Assistant(a) if a.is_stream_continuation),
                assistant_text_child_theme_for(theme),
                theme.assistant,
                markdown_width,
                markdown_options,
            ),
        )),
        (
            AssistantContentBlock::ConnectorText { connector_text },
            AssistantBlockProjection::ConnectorText {
                add_margin,
                verbose,
            },
        ) => Some(MessageBodyKind::AssistantText(
            AssistantTextBodyWidget::from_raw_with_options(
                connector_text,
                add_margin,
                verbose,
                true,
                assistant_text_child_theme_for(theme),
                theme.assistant,
                markdown_width,
                markdown_options,
            ),
        )),
        (
            AssistantContentBlock::RedactedThinking { .. },
            AssistantBlockProjection::RedactedThinking {
                hidden: false,
                add_margin,
            },
        ) => Some(MessageBodyKind::AssistantThinking(
            AssistantThinkingBodyWidget::redacted_placeholder(
                add_margin,
                child_theme_for(theme),
                theme.hint,
            ),
        )),
        (
            AssistantContentBlock::Thinking { thinking },
            AssistantBlockProjection::Thinking {
                add_margin,
                is_transcript_mode,
                verbose,
                hide_in_transcript,
            },
        ) => Some(MessageBodyKind::AssistantThinking(
            AssistantThinkingBodyWidget::from_raw(
                thinking.as_deref().unwrap_or_default(),
                add_margin,
                is_transcript_mode,
                verbose,
                hide_in_transcript,
                input.compact_thinking_preview,
                input.show_thinking_expand_hint,
                child_theme_for(theme),
                theme.hint,
            ),
        )),
        (_, AssistantBlockProjection::Null) => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;

    use super::*;
    use rebon_render::{
        project_message_model, project_message_timestamp, AssistantContentBlock, AssistantMessage,
        MessageRow, TextBlock, TimelineContentBlock, TimelineMessage, UserContentBlock,
        UserMessage,
    };

    fn buffer(width: u16, height: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, width, height))
    }

    fn line(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn widget_renders_metadata_and_body_into_buffer() {
        let timeline = TimelineMessage::Assistant(rebon_render::AssistantTimelineMessage {
            uuid: "a-1".into(),
            is_api_error_message: false,
            timestamp: Some("2026-04-08T12:00:00Z".into()),
            model: Some("sonnet".into()),
            content: vec![TimelineContentBlock::Text(TextBlock {
                text: "hello".into(),
            })],
        });
        let timestamp = project_message_timestamp(&timeline, true, "01:23 PM");
        let model = project_message_model(&timeline, true);

        let input = RenderMessageInput {
            message: MessageRow::Assistant(AssistantMessage {
                uuid: "a-1".into(),
                content: vec![AssistantContentBlock::Text {
                    text: "hello world".into(),
                }],
                advisor_model: None,
                is_stream_continuation: false,
            }),
            container_width: Some(20),
            add_margin: true,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: true,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 20,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };

        let widget = RenderedMessageWidget::new(
            &input,
            &MessageRenderTheme::plain(),
            20,
            timestamp.as_ref(),
            model.as_ref(),
        );

        let mut buf = buffer(20, 4);
        widget.render(Rect::new(0, 0, 20, 4), &mut buf);
        assert!(line(&buf, 0).ends_with("01:23 PM sonnet"));
        // With add_margin, gutter label and content are pushed down by 1 row.
        assert!((1..4).any(|y| line(&buf, y).starts_with("●")));
        assert!((1..4).any(|y| line(&buf, y).contains("hello world")));
    }

    #[test]
    fn widget_height_counts_metadata_plus_wrapped_body() {
        let input = RenderMessageInput {
            message: MessageRow::User(UserMessage {
                uuid: "u-1".into(),
                is_compact_summary: false,
                content: vec![UserContentBlock::Text {
                    text: "12345678901234567890".into(),
                }],
                image_paste_ids: Vec::new(),
                plan_content: None,
                timestamp: None,
            }),
            container_width: Some(10),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 10,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&input, &MessageRenderTheme::plain(), 10, None, None);
        assert!(widget.height(10) >= 2);
    }

    #[test]
    fn widget_folds_long_user_text_to_head_separator_and_tail() {
        let text = (1..=46)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let input = RenderMessageInput {
            message: MessageRow::User(UserMessage {
                uuid: "u-long".into(),
                is_compact_summary: false,
                content: vec![UserContentBlock::Text { text }],
                image_paste_ids: Vec::new(),
                plan_content: None,
                timestamp: None,
            }),
            container_width: Some(50),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&input, &MessageRenderTheme::plain(), 50, None, None);

        assert_eq!(widget.height(50), 22);
        let mut buf = buffer(50, 22);
        widget.render(Rect::new(0, 0, 50, 22), &mut buf);
        assert_eq!(line(&buf, 0), "❯ line 1");
        assert_eq!(line(&buf, 9), "  line 10");
        assert!(line(&buf, 10).starts_with("  ──── (26 lines hidden) ─"));
        assert_eq!(line(&buf, 10).chars().count(), 50);
        assert_eq!(line(&buf, 11), "");
        assert_eq!(line(&buf, 12), "  line 37");
        assert_eq!(line(&buf, 21), "  line 46");
    }

    #[test]
    fn widget_height_matches_wrapped_chinese_assistant_text() {
        let text = "所以这次修复不是让所有旧 thinking 都常驻显示，而是让 coordinator 产生的内部消息不错误地把当前 thinking 当成旧 thinking 折叠掉。";
        let input = RenderMessageInput {
            message: MessageRow::Assistant(AssistantMessage {
                uuid: "a-cjk".into(),
                content: vec![AssistantContentBlock::Text { text: text.into() }],
                advisor_model: None,
                is_stream_continuation: false,
            }),
            container_width: Some(24),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: true,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 24,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&input, &MessageRenderTheme::plain(), 24, None, None);
        let height = widget.height(24);
        assert!(
            height > 2,
            "CJK text should wrap to multiple rows, got {height}"
        );

        let mut buf = buffer(24, height);
        widget.render(Rect::new(0, 0, 24, height), &mut buf);
        let rendered = (0..height)
            .map(|y| line(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("叠 掉") || rendered.contains("折 叠"),
            "tail text should not be clipped: {rendered:?}"
        );
    }

    #[test]
    fn widget_height_includes_lines_after_wrapped_assistant_text() {
        let text = "这个问题不是最后几个字被截断，而是第一段因为中文换行后高度少算，导致后面整段内容都没有机会被画出来。\n后续内容必须仍然显示。";
        let input = RenderMessageInput {
            message: MessageRow::Assistant(AssistantMessage {
                uuid: "a-cjk-tail".into(),
                content: vec![AssistantContentBlock::Text { text: text.into() }],
                advisor_model: None,
                is_stream_continuation: false,
            }),
            container_width: Some(24),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: true,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 24,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&input, &MessageRenderTheme::plain(), 24, None, None);
        let height = widget.height(24);
        let mut buf = buffer(24, height);
        widget.render(Rect::new(0, 0, 24, height), &mut buf);
        let rendered = (0..height)
            .map(|y| line(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("后 续 内 容") || rendered.contains("必 须 仍 然 显 示"),
            "content after wrapped text should not be clipped: {rendered:?}"
        );
    }

    #[test]
    fn widget_dispatches_to_typed_body_for_attachment_rows() {
        use rebon_render::{
            AttachmentFileDisplay, AttachmentFileKind, AttachmentInput, AttachmentMessage,
        };

        fn base(message: MessageRow) -> RenderMessageInput {
            RenderMessageInput {
                message,
                container_width: Some(40),
                add_margin: false,
                verbose: false,
                style_condensed: false,
                is_transcript_mode: false,
                is_active_collapsed_group: false,
                is_user_continuation: false,
                last_thinking_block_id: None,
                latest_bash_output_uuid: None,
                terminal_columns: 40,
                fullscreen_env_enabled: false,
                compact_thinking_preview: false,
                show_thinking_expand_hint: false,
                frame_time_ms: 0,
                in_progress_tool_use_ids: Vec::new(),
                errored_tool_use_ids: Vec::new(),
                show_tool_expand_hint: false,
            }
        }

        // Attachment row with payload → Attachment body.
        let attachment_input = base(MessageRow::Attachment(AttachmentMessage {
            uuid: "att-1".into(),
            attachment: Some(Box::new(AttachmentInput::File(AttachmentFileDisplay {
                display_path: "README.md".into(),
                kind: AttachmentFileKind::Text {
                    num_lines: 7,
                    truncated: false,
                },
            }))),
        }));
        let widget = RenderedMessageWidget::new(
            &attachment_input,
            &MessageRenderTheme::plain(),
            40,
            None,
            None,
        );
        assert!(matches!(widget.body, MessageBodyKind::Attachment(_)));
        let mut buf = buffer(40, 2);
        widget.render(Rect::new(0, 0, 40, 2), &mut buf);
        assert!(line(&buf, 0).starts_with("● "));
        assert!(line(&buf, 0).contains("Read README.md"));

        // Attachment row WITHOUT payload → Text fallback body.
        let fallback_input = base(MessageRow::Attachment(AttachmentMessage {
            uuid: "att-2".into(),
            attachment: None,
        }));
        let widget = RenderedMessageWidget::new(
            &fallback_input,
            &MessageRenderTheme::plain(),
            40,
            None,
            None,
        );
        assert!(matches!(widget.body, MessageBodyKind::Text(_)));
    }

    #[test]
    fn widget_with_border_wraps_body_in_titled_frame() {
        let input = RenderMessageInput {
            message: MessageRow::User(UserMessage {
                uuid: "u-sel".into(),
                is_compact_summary: false,
                content: vec![UserContentBlock::Text { text: "hi".into() }],
                image_paste_ids: Vec::new(),
                plan_content: None,
                timestamp: None,
            }),
            container_width: Some(20),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 20,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&input, &MessageRenderTheme::plain(), 20, None, None)
                .with_border(MessageBorderDecoration {
                    title: " Selected ".into(),
                    style: Style::default(),
                });
        assert!(widget.border.is_some());
        let height = widget.height(20);
        // Body "USR hi" inside 18-col inner width → 1 row + 2 border lines.
        assert_eq!(height, 3);

        let mut buf = buffer(20, 4);
        widget.render(Rect::new(0, 0, 20, 4), &mut buf);
        // Top border should contain the title.
        assert!(line(&buf, 0).contains("Selected"));
        // Middle row should contain the USR prefix and body.
        assert!((1..3).any(|y| line(&buf, y).contains("❯ hi")));
        // Bottom border should be visible too.
        assert!(!line(&buf, 2).is_empty());
    }

    #[test]
    fn widget_dispatches_to_system_text_and_user_text_plan_widgets_when_payload_present() {
        use rebon_render::{StopHookSummaryDisplay, SystemVisualMarker};
        use rebon_render::{SystemMessage, SystemSubtype};

        // System row carrying a stop_hook_summary payload → SystemText body.
        let system_row = RenderMessageInput {
            message: MessageRow::System(SystemMessage {
                uuid: "sys-1".into(),
                subtype: SystemSubtype::Other,
                raw_subtype: Some("stop_hook_summary".into()),
                level: Some("warning".into()),
                content: String::new(),
                stop_hook_summary: Some(Box::new(StopHookSummaryDisplay::Default {
                    margin_top: 0,
                    background: None,
                    marker: SystemVisualMarker::BlackCircle,
                    summary: "Ran 2 stop hooks".into(),
                    detail_lines: vec![],
                    prevented_line: Some("\u{23BF}  Stopped".into()),
                    error_lines: vec![],
                    show_expand_hint: false,
                    width: 40,
                })),
            }),
            container_width: Some(50),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&system_row, &MessageRenderTheme::plain(), 50, None, None);
        assert!(matches!(widget.body, MessageBodyKind::SystemText(_)));
        let mut buf = buffer(50, 8);
        widget.render(Rect::new(0, 0, 50, 8), &mut buf);
        let rendered: Vec<String> = (0..8).map(|y| line(&buf, y)).collect();
        assert!(rendered[0].starts_with("Ran 2 stop hooks"));
        assert!(rendered.iter().any(|l| l.contains("Ran 2 stop hooks")));
        assert!(rendered.iter().any(|l| l.contains("Stop Hooks")));
        assert!(rendered.iter().any(|l| l.contains("Stopped")));

        // User row carrying plan_content → UserTextPlan body.
        let user_row = RenderMessageInput {
            message: MessageRow::User(UserMessage {
                uuid: "u-plan".into(),
                is_compact_summary: false,
                content: vec![],
                image_paste_ids: Vec::new(),
                plan_content: Some("# Plan\n- step a\n- step b".into()),
                timestamp: None,
            }),
            container_width: Some(50),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&user_row, &MessageRenderTheme::plain(), 50, None, None);
        assert!(matches!(widget.body, MessageBodyKind::UserTextPlan(_)));
        let mut buf = buffer(50, 8);
        widget.render(Rect::new(0, 0, 50, 8), &mut buf);
        let rendered: Vec<String> = (0..8).map(|y| line(&buf, y)).collect();
        // The opening rule carries the title and sits ABOVE the gutter row.
        assert!(
            rendered[0].starts_with("── Plan to implement ─"),
            "{rendered:?}"
        );
        assert!(rendered[1].starts_with("● "), "{rendered:?}");
        assert!(rendered.iter().any(|l| l.contains("# Plan")));
        assert!(rendered.iter().any(|l| l.contains("- step a")));
        // No side borders: nothing draws a vertical rule down either edge.
        assert!(
            !rendered[1..4].iter().any(|l| l.contains('│')),
            "{rendered:?}"
        );
    }

    /// An answered `AskUserQuestion` is not a typed prompt, and does not look
    /// like one: event glyph, ruled card, no prompt highlight.
    #[test]
    fn answered_questions_render_as_their_own_card() {
        let row = RenderMessageInput {
            message: MessageRow::User(UserMessage {
                uuid: "u-answers".into(),
                is_compact_summary: false,
                content: vec![rebon_render::UserContentBlock::Text {
                    text: concat!(
                        "Answered questions:\n",
                        "- Which database?\n  Answer: Postgres\n",
                        "- Which UI?\n  Answer: Cards\n  Notes: dense please",
                    )
                    .into(),
                }],
                image_paste_ids: Vec::new(),
                plan_content: None,
                timestamp: None,
            }),
            container_width: Some(50),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget = RenderedMessageWidget::new(&row, &MessageRenderTheme::plain(), 50, None, None);
        assert!(
            matches!(widget.body, MessageBodyKind::UserAskAnswers(_)),
            "{:?}",
            widget.body
        );
        let mut buf = buffer(50, 10);
        widget.render(Rect::new(0, 0, 50, 10), &mut buf);
        let rendered: Vec<String> = (0..10).map(|y| line(&buf, y)).collect();

        assert!(
            rendered[0].starts_with("── Answered questions ─"),
            "{rendered:?}"
        );
        assert_eq!(rendered[1], "● Which database?", "{rendered:?}");
        assert_eq!(rendered[2], "    ❯ Postgres", "{rendered:?}");
        assert_eq!(rendered[4], "  Which UI?", "{rendered:?}");
        assert_eq!(rendered[5], "    ❯ Cards", "{rendered:?}");
        assert_eq!(rendered[6], "      dense please", "{rendered:?}");
        let closing = rendered
            .iter()
            .find(|l| l.contains("2 answers"))
            .expect("closing rule carries the count");
        assert!(
            closing.starts_with('─') && closing.ends_with("──"),
            "{closing:?}"
        );

        // The prompt highlight belongs to messages someone typed.
        assert!(
            (0..10).all(|y| (0..50).all(|x| buf[(x, y)].bg == ratatui::style::Color::Reset)),
            "no cell may carry the user-prompt background"
        );
    }

    /// Only the exact formatter output takes the card.
    #[test]
    fn a_typed_prompt_that_merely_starts_the_same_stays_user_text() {
        let row = RenderMessageInput {
            message: MessageRow::User(UserMessage {
                uuid: "u-typed".into(),
                is_compact_summary: false,
                content: vec![rebon_render::UserContentBlock::Text {
                    text: "Answered questions: which ones did I miss?".into(),
                }],
                image_paste_ids: Vec::new(),
                plan_content: None,
                timestamp: None,
            }),
            container_width: Some(50),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget = RenderedMessageWidget::new(&row, &MessageRenderTheme::plain(), 50, None, None);
        assert!(matches!(widget.body, MessageBodyKind::UserText(_)));
    }

    fn provider_switch_input(provider: &str, model: &str) -> RenderMessageInput {
        RenderMessageInput {
            message: MessageRow::System(rebon_render::SystemMessage {
                uuid: "provider-switch".into(),
                subtype: rebon_render::SystemSubtype::Other,
                raw_subtype: Some("provider_switch".into()),
                level: Some("info".into()),
                content: format!("Switched to provider {provider}\nUsing model {model}"),
                stop_hook_summary: None,
            }),
            container_width: Some(80),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 80,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        }
    }

    #[test]
    fn provider_switch_renderers_keep_two_lines_accent_verbose_and_margin() {
        let theme = MessageRenderTheme::default_styled();
        let accent = crate::projection_render::parse_theme_color(
            rebon_design_system::theme::get_active_theme().permission,
        );
        // The shared tool theme must not acquire the provider's foreground.
        assert_eq!(child_theme_for(&theme).accent.fg, theme.assistant_text.fg);
        assert_ne!(child_theme_for(&theme).accent.fg, Some(accent));
        assert_eq!(theme.markdown_accent.fg, Some(accent));
        for verbose in [false, true] {
            for add_margin in [false, true] {
                let mut input = provider_switch_input("openai", "gpt-5");
                input.verbose = verbose;
                input.add_margin = add_margin;
                input.is_transcript_mode = verbose;
                let offset = usize::from(add_margin);
                let flat = render_message(&input, &theme);
                let mut expected = Vec::new();
                if add_margin {
                    expected.push("");
                }
                expected.extend(["● Switched to provider openai", "  ⎿ Using model gpt-5"]);
                assert_eq!(
                    flat.lines
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                    expected
                );
                let colored: Vec<_> = flat
                    .lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .filter(|span| span.style.fg == Some(accent))
                    .collect();
                assert_eq!(colored.len(), 1);
                assert_eq!(colored[0].content, "openai");

                let widget = RenderedMessageWidget::new(&input, &theme, 80, None, None);
                assert!(matches!(widget.body, MessageBodyKind::SystemText(_)));
                assert_eq!(widget.height(80), expected.len() as u16);
                let mut buf = buffer(80, expected.len() as u16);
                widget.render_to_buffer(buf.area, &mut buf);
                for (y, expected) in expected.iter().enumerate() {
                    assert_eq!(line(&buf, y as u16), *expected);
                }
                let provider_start = rebon_width::str_width("● Switched to provider ") as u16;
                for x in provider_start..provider_start + 6 {
                    assert_eq!(buf[(x, offset as u16)].fg, accent);
                }
                assert_ne!(buf[(2, offset as u16)].fg, accent);
                assert_ne!(buf[(4, offset as u16 + 1)].fg, accent);
            }
        }
    }

    #[test]
    fn provider_switch_renderers_wrap_narrow_without_losing_text_or_accent() {
        let theme = MessageRenderTheme::default_styled();
        let accent = crate::projection_render::parse_theme_color(
            rebon_design_system::theme::get_active_theme().permission,
        );
        let without_spaces = |text: &str| {
            text.chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        };
        for (provider, model) in [
            ("long-provider-name", "org/long-model-name-v2"),
            ("本地 gateway", "组织/model-with-版本"),
        ] {
            for width in [2, 3, 8, 16, 24, 40] {
                for verbose in [false, true] {
                    for add_margin in [false, true] {
                        let mut input = provider_switch_input(provider, model);
                        input.verbose = verbose;
                        input.add_margin = add_margin;
                        input.container_width = Some(width);
                        input.terminal_columns = width;
                        let widget = RenderedMessageWidget::new(&input, &theme, width, None, None);
                        let height = widget.height(width);
                        let mut flat_buf = buffer(width, 160);
                        Paragraph::new(render_message(&input, &theme))
                            .wrap(ratatui::widgets::Wrap { trim: false })
                            .render(flat_buf.area, &mut flat_buf);
                        let painted_height = (0..flat_buf.area.height)
                            .rfind(|&y| !line(&flat_buf, y).is_empty())
                            .expect("provider switch must be visible")
                            + 1;
                        assert_eq!(height, painted_height, "width={width}, provider={provider}");
                        let mut buf = buffer(width, height + 1);
                        widget.render_to_buffer(buf.area, &mut buf);
                        let rendered = (0..height).map(|y| line(&buf, y)).collect::<Vec<_>>();
                        assert_eq!(
                            rendered,
                            (0..height).map(|y| line(&flat_buf, y)).collect::<Vec<_>>()
                        );
                        assert_eq!(
                            without_spaces(&rendered.concat()),
                            without_spaces(&format!(
                                "● Switched to provider {provider}  ⎿ Using model {model}"
                            ))
                        );
                        let mut colored = String::new();
                        for y in 0..height {
                            for x in 0..width {
                                let cell = &buf[(x, y)];
                                if !cell.symbol().trim().is_empty() {
                                    assert_eq!(cell.fg, flat_buf[(x, y)].fg);
                                }
                                if cell.fg == accent {
                                    colored.push_str(cell.symbol());
                                }
                            }
                        }
                        assert_eq!(without_spaces(&colored), without_spaces(provider));
                        assert!(
                            line(&buf, height).is_empty(),
                            "height must include all wrapped rows"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn widget_dispatches_generic_system_rows_to_system_text_body_widget() {
        let system_row = RenderMessageInput {
            message: MessageRow::System(rebon_render::SystemMessage {
                uuid: "sys-generic".into(),
                subtype: rebon_render::SystemSubtype::Other,
                raw_subtype: Some("informational".into()),
                level: Some("warning".into()),
                content: "watch out".into(),
                stop_hook_summary: None,
            }),
            container_width: Some(50),
            add_margin: false,
            verbose: true,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&system_row, &MessageRenderTheme::plain(), 50, None, None);
        assert!(matches!(widget.body, MessageBodyKind::SystemText(_)));
        let mut buf = buffer(50, 4);
        widget.render(Rect::new(0, 0, 50, 4), &mut buf);
        let rendered: Vec<String> = (0..4).map(|y| line(&buf, y)).collect();
        assert!(rendered[0].starts_with("watch out"));
        assert!(rendered.iter().any(|line| line.contains("watch out")));

        for verbose in [false, true] {
            for add_margin in [false, true] {
                for (subtype, level, content, hidden) in [
                    ("turn_duration", "info", "Worked 6m 23s", false),
                    ("informational", "warning", "watch out", false),
                    ("informational", "info", "quiet notice", !verbose),
                    ("informational", "info", "", !verbose),
                ] {
                    let mut input = system_row.clone();
                    input.add_margin = add_margin;
                    input.verbose = verbose;
                    input.is_transcript_mode = true;
                    let MessageRow::System(system) = &mut input.message else {
                        unreachable!()
                    };
                    system.raw_subtype = Some(subtype.into());
                    system.level = Some(level.into());
                    system.content = content.into();
                    let widget = RenderedMessageWidget::new(
                        &input,
                        &MessageRenderTheme::plain(),
                        50,
                        None,
                        None,
                    );
                    assert_eq!(matches!(widget.body, MessageBodyKind::Hidden), hidden);
                    let height = if hidden { 0 } else { 1 + u16::from(add_margin) };
                    assert_eq!(
                        widget.height(50),
                        height,
                        "{subtype}, {add_margin}, {verbose}"
                    );
                    let mut actual = buffer(50, 4);
                    widget.render(actual.area, &mut actual);
                    if hidden || add_margin {
                        assert!(line(&actual, 0).is_empty());
                    }
                    if !hidden && !content.is_empty() {
                        assert!(line(&actual, u16::from(add_margin)).contains(content));
                    }
                    assert!((height..4).all(|y| line(&actual, y).is_empty()));
                }
            }
        }

        let api_error_row = RenderMessageInput {
            message: MessageRow::System(rebon_render::SystemMessage {
                uuid: "sys-api-error".into(),
                subtype: rebon_render::SystemSubtype::Other,
                raw_subtype: Some("api_error".into()),
                level: Some("error".into()),
                content: "Prompt turn failed: prompt executor failed: model stream error: boom"
                    .into(),
                stop_hook_summary: None,
            }),
            container_width: Some(50),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget = RenderedMessageWidget::new(
            &api_error_row,
            &MessageRenderTheme::plain(),
            50,
            None,
            None,
        );
        assert!(matches!(widget.body, MessageBodyKind::SystemText(_)));
        let mut buf = buffer(50, 2);
        widget.render(Rect::new(0, 0, 50, 2), &mut buf);
        let error_line = line(&buf, 0);
        assert!(error_line.starts_with("  ⎿  boom"), "{error_line}");
        assert!(!error_line.contains("Prompt turn failed"), "{error_line}");
    }

    #[test]
    fn hidden_generic_info_system_rows_are_zero_height() {
        let system_row = RenderMessageInput {
            message: MessageRow::System(rebon_render::SystemMessage {
                uuid: "sys-hidden".into(),
                subtype: rebon_render::SystemSubtype::Other,
                raw_subtype: Some("info".into()),
                level: Some("info".into()),
                content: "Switched to provider".into(),
                stop_hook_summary: None,
            }),
            container_width: Some(50),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&system_row, &MessageRenderTheme::plain(), 50, None, None);
        assert!(matches!(widget.body, MessageBodyKind::Hidden));
        assert_eq!(widget.height(50), 0);
        let mut buf = buffer(50, 2);
        widget.render(Rect::new(0, 0, 50, 2), &mut buf);
        assert_eq!(line(&buf, 0), "");
        assert_eq!(line(&buf, 1), "");
    }

    #[test]
    fn widget_dispatches_single_assistant_tool_use_to_typed_tool_widget() {
        let input = RenderMessageInput {
            message: MessageRow::Assistant(AssistantMessage {
                uuid: "a-tool".into(),
                content: vec![AssistantContentBlock::ToolUse {
                    id: Some("toolu-1".into()),
                    name: Some("Read".into()),
                    input_summary: Some("src/lib.rs".into()),
                    diff: None,
                    body_lines: Vec::new(),
                }],
                advisor_model: None,
                is_stream_continuation: false,
            }),
            container_width: Some(50),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&input, &MessageRenderTheme::plain(), 50, None, None);
        assert!(matches!(widget.body, MessageBodyKind::AssistantToolUse(_)));
        let mut buf = buffer(50, 4);
        widget.render(Rect::new(0, 0, 50, 4), &mut buf);
        let rendered: Vec<String> = (0..4).map(|y| line(&buf, y)).collect();
        assert!(rendered[0].starts_with("●"));
        assert!(rendered
            .iter()
            .any(|line| line.contains("Read (src/lib.rs)")));
    }

    #[test]
    fn hidden_first_assistant_block_does_not_add_extra_margin() {
        let input = RenderMessageInput {
            message: MessageRow::Assistant(AssistantMessage {
                uuid: "a-hidden-first".into(),
                content: vec![
                    AssistantContentBlock::Thinking {
                        thinking: Some("".into()),
                    },
                    AssistantContentBlock::Text {
                        text: "visible answer".into(),
                    },
                ],
                advisor_model: None,
                is_stream_continuation: false,
            }),
            container_width: Some(50),
            add_margin: true,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&input, &MessageRenderTheme::plain(), 50, None, None);
        assert_eq!(widget.height(50), 2);
        let mut buf = buffer(50, 3);
        widget.render(Rect::new(0, 0, 50, 3), &mut buf);
        assert_eq!(line(&buf, 0), "");
        assert!(line(&buf, 1).contains("visible answer"));
        assert_eq!(line(&buf, 2), "");
    }

    #[test]
    fn widget_dispatches_single_user_tool_result_to_typed_result_widget() {
        let input = RenderMessageInput {
            message: MessageRow::User(UserMessage {
                uuid: "u-tool".into(),
                is_compact_summary: false,
                content: vec![UserContentBlock::ToolResult {
                    tool_use_id: Some("toolu-1".into()),
                    content: Some("line 1\nline 2".into()),
                    is_error: false,
                }],
                image_paste_ids: Vec::new(),
                plan_content: None,
                timestamp: None,
            }),
            container_width: Some(50),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&input, &MessageRenderTheme::plain(), 50, None, None);
        assert!(matches!(widget.body, MessageBodyKind::UserToolResult(_)));
        let mut buf = buffer(50, 8);
        widget.render(Rect::new(0, 0, 50, 8), &mut buf);
        let rendered: Vec<String> = (0..8).map(|y| line(&buf, y)).collect();
        assert!(rendered[0].starts_with("↳ "));
        assert!(rendered
            .iter()
            .any(|line| line.contains("[tool result toolu-1]")));
        assert!(rendered.iter().any(|line| line.contains("Result")));
        assert!(rendered.iter().any(|line| line.contains("line 2")));
    }

    #[test]
    fn widget_dispatches_single_assistant_text_to_typed_text_widget() {
        let input = RenderMessageInput {
            message: MessageRow::Assistant(AssistantMessage {
                uuid: "a-text".into(),
                content: vec![AssistantContentBlock::Text {
                    text: "hello\nworld".into(),
                }],
                advisor_model: None,
                is_stream_continuation: false,
            }),
            container_width: Some(50),
            add_margin: true,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&input, &MessageRenderTheme::plain(), 50, None, None);
        assert!(matches!(widget.body, MessageBodyKind::AssistantText(_)));
        let mut buf = buffer(50, 4);
        widget.render(Rect::new(0, 0, 50, 4), &mut buf);
        let rendered: Vec<String> = (0..4).map(|y| line(&buf, y)).collect();
        // add_margin=true → row 0 is blank margin, gutter+content at row 1+
        assert!(rendered.iter().any(|line| line.starts_with("●")));
        assert!(rendered.iter().any(|line| line.contains("hello")));
        assert!(rendered.iter().any(|line| line.contains("world")));
    }

    #[test]
    fn widget_returns_hyperlink_layers_for_composite_assistant_blocks() {
        let input = RenderMessageInput {
            message: MessageRow::Assistant(AssistantMessage {
                uuid: "a-links".into(),
                content: vec![
                    AssistantContentBlock::Text {
                        text: "`code` [one](https://one.test)".into(),
                    },
                    AssistantContentBlock::ConnectorText {
                        connector_text: "visit https://two.test.".into(),
                    },
                ],
                advisor_model: None,
                is_stream_continuation: false,
            }),
            container_width: Some(40),
            add_margin: false,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 40,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let theme = MessageRenderTheme {
            markdown_accent: Style::new().fg(ratatui::style::Color::Red),
            ..MessageRenderTheme::plain()
        };
        let widget = RenderedMessageWidget::new(&input, &theme, 40, None, None).with_border(
            MessageBorderDecoration {
                title: "links".into(),
                style: Style::new(),
            },
        );
        let height = widget.height(40);
        let mut buf = buffer(40, height);
        let layers = widget.render_to_buffer_with_hyperlinks(Rect::new(0, 0, 40, height), &mut buf);

        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].area, Rect::new(3, 1, 36, 1));
        assert_eq!(layers[1].area, Rect::new(3, 3, 36, 1));
        assert_eq!(layers[0].hyperlinks[0].target, "https://one.test");
        assert_eq!(layers[1].hyperlinks[0].target, "https://two.test");
        assert!(line(&buf, 1).starts_with("│● code one"));
        assert!(line(&buf, 3).starts_with("│● visit https://two.test."));
        assert_eq!(
            buf[(3, 1)].style().fg,
            Some(ratatui::style::Color::Red),
            "inline code must use the dedicated markdown accent"
        );
    }

    #[test]
    fn widget_dispatches_single_assistant_thinking_to_typed_thinking_widget() {
        let input = RenderMessageInput {
            message: MessageRow::Assistant(AssistantMessage {
                uuid: "a-thk".into(),
                content: vec![AssistantContentBlock::Thinking {
                    thinking: Some("step 1\nstep 2".into()),
                }],
                advisor_model: None,
                is_stream_continuation: false,
            }),
            container_width: Some(50),
            add_margin: false,
            verbose: true,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: Some("a-thk:0".into()),
            latest_bash_output_uuid: None,
            terminal_columns: 50,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let widget =
            RenderedMessageWidget::new(&input, &MessageRenderTheme::plain(), 50, None, None);
        assert!(matches!(widget.body, MessageBodyKind::AssistantThinking(_)));
        let mut buf = buffer(50, 6);
        widget.render(Rect::new(0, 0, 50, 6), &mut buf);
        let rendered: Vec<String> = (0..6).map(|y| line(&buf, y)).collect();
        assert!(rendered[0].starts_with("· "));
        assert!(rendered.iter().any(|line| line.contains("step 1")));
        assert!(rendered.iter().any(|line| line.contains("step 2")));
        assert!(!rendered.iter().any(|line| line.contains("Thinking")));
    }
}

/// The body for an assistant row carrying exactly one content block.
///
/// One block is the common case and the only one that can be rendered as
/// something other than plain text -- a tool call, a thinking block, an image
/// -- so it is decided here rather than falling through to the text path.
fn build_single_content_assistant_body(
    input: &RenderMessageInput,
    theme: &MessageRenderTheme,
    markdown_width: u16,
    markdown_options: crate::MarkdownRenderOptions,
    assistant: &rebon_render::types::AssistantMessage,
) -> MessageBodyKind {
    let block = assistant.content[0].clone();
    let projection = project_assistant_block(
        &block,
        input.add_margin,
        input.verbose,
        input.is_transcript_mode,
        input.last_thinking_block_id.as_deref(),
        &format!("{}:0", assistant.uuid),
    );
    match (&block, projection) {
        (
            AssistantContentBlock::ToolUse {
                id,
                name,
                input_summary,
                diff,
                body_lines,
            },
            AssistantBlockProjection::ToolUse { add_margin },
        ) => {
            if let Some((path, old_text, new_text)) = diff {
                // Edit tool with diff data — render using
                // FileEditBodyWidget for colored inline diff.
                use rebon_render::file_edit::{
                    project_file_edit_updated, FileEditUpdatedInput, StructuredPatchHunk,
                };
                let mut hunk_lines = Vec::new();
                if let Some(old) = old_text {
                    for line in old.lines() {
                        hunk_lines.push(format!("-{line}"));
                    }
                }
                for line in new_text.lines() {
                    hunk_lines.push(format!("+{line}"));
                }
                let hunks = vec![StructuredPatchHunk { lines: hunk_lines }];
                let projection = project_file_edit_updated(&FileEditUpdatedInput {
                    file_path: path.clone(),
                    structured_patch: hunks,
                    first_line: None,
                    file_content: None,
                    style_condensed: input.style_condensed,
                    verbose: input.verbose,
                    preview_hint: None,
                    columns: input.terminal_columns as usize,
                });
                MessageBodyKind::FileEdit(crate::widget_subtree::FileEditBodyWidget::from_updated(
                    projection,
                    child_theme_for(theme),
                    theme.assistant,
                ))
            } else {
                MessageBodyKind::AssistantToolUse(AssistantToolUseBodyWidget::from_raw(
                    id.as_deref(),
                    name.as_deref(),
                    input_summary.as_deref(),
                    body_lines,
                    add_margin,
                    child_theme_for(theme),
                    tool_gutter_style(id.as_deref(), input, theme),
                ))
            }
        }
        (
            AssistantContentBlock::Text { text },
            AssistantBlockProjection::Text {
                add_margin,
                verbose,
            },
        ) => MessageBodyKind::AssistantText(AssistantTextBodyWidget::from_raw_with_options(
            &crate::render::normalize_assistant_display_text(text),
            add_margin,
            verbose,
            !assistant.is_stream_continuation,
            assistant_text_child_theme_for(theme),
            theme.assistant,
            markdown_width,
            markdown_options,
        )),
        (
            AssistantContentBlock::ConnectorText { connector_text },
            AssistantBlockProjection::ConnectorText {
                add_margin,
                verbose,
            },
        ) => MessageBodyKind::AssistantText(AssistantTextBodyWidget::from_raw_with_options(
            connector_text,
            add_margin,
            verbose,
            true,
            assistant_text_child_theme_for(theme),
            theme.assistant,
            markdown_width,
            markdown_options,
        )),
        (
            AssistantContentBlock::RedactedThinking { .. },
            AssistantBlockProjection::RedactedThinking {
                hidden: false,
                add_margin,
            },
        ) => MessageBodyKind::AssistantThinking(AssistantThinkingBodyWidget::redacted_placeholder(
            add_margin,
            child_theme_for(theme),
            theme.hint,
        )),
        (
            AssistantContentBlock::Thinking { thinking },
            AssistantBlockProjection::Thinking {
                add_margin,
                is_transcript_mode,
                verbose,
                hide_in_transcript,
            },
        ) => MessageBodyKind::AssistantThinking(AssistantThinkingBodyWidget::from_raw(
            thinking.as_deref().unwrap_or_default(),
            add_margin,
            is_transcript_mode,
            verbose,
            hide_in_transcript,
            input.compact_thinking_preview,
            input.show_thinking_expand_hint,
            child_theme_for(theme),
            theme.hint,
        )),
        _ => MessageBodyKind::Text(render_message(input, theme)),
    }
}
