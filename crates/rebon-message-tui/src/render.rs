//! `ratatui` render helpers for message projections.

use crate::projection_render::{
    parse_theme_color, render_assistant_text_projection, render_assistant_thinking_projection,
    render_assistant_tool_use_projection, render_attachment_projection,
    render_compact_summary_display, render_system_text_projection, render_user_text_projection,
    render_user_tool_result_projection, MessagesRenderTheme,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use rebon_design_system::theme;
use rebon_render::{
    project_assistant_text_message, project_assistant_thinking_message, project_assistant_tool_use,
    project_attachment_message, project_compact_summary_card, project_system_text_message,
    project_user_text, project_user_tool_result, AssistantTextMessageInput,
    AssistantThinkingMessageInput, AssistantToolDefinition, AssistantToolRenderOutputs,
    AssistantToolUseInput, AssistantToolUseInvocation, AttachmentMessageInput, CompactSummaryInput,
    SystemTextMessageInput, SystemTextProjectionInput, UserTextInput, UserToolResultInput,
};
use rebon_width::WidthStr;

use rebon_render::{
    project_message, AssistantBlockProjection, AssistantContentBlock,
    FallbackToolUseErrorProjection, FileEditRejectedProjection, FileEditUpdatedProjection,
    ImageKey, MessageProjection, MetadataLabelProjection, NotebookEditRejectedProjection,
    RenderMessageInput, SystemProjection, UserBlockProjection, UserContentBlock,
};

/// Strip any leading `<system-reminder>…</system-reminder>` blocks from `text`.
pub fn strip_leading_system_reminders(text: &str) -> String {
    const CLOSE: &str = "</system-reminder>";
    let mut current = text.trim_start();
    while current.starts_with("<system-reminder>") {
        let Some(end) = current.find(CLOSE) else {
            break;
        };
        current = current[end + CLOSE.len()..].trim_start();
    }
    current.to_string()
}

/// Normalize an assistant text block for display: strip leading
/// system-reminder blocks, then collapse a leaked side-channel
/// `{"summary":"…"}` envelope (see
/// [`rebon_render::summary_envelope`]) to its summary sentence so
/// affected transcripts don't render raw JSON.
pub fn normalize_assistant_display_text(text: &str) -> String {
    let stripped = strip_leading_system_reminders(text);
    rebon_render::summary_envelope::display_text_for_summary_envelope(&stripped)
}

/// Minimal style bag for ratatui message rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageRenderTheme {
    /// User gutter / label style.
    pub user: Style,
    /// Assistant gutter / label style (dim).
    pub assistant: Style,
    /// Assistant body text style (normal color).
    pub assistant_text: Style,
    /// Accent used only by assistant markdown links and inline code.
    pub markdown_accent: Style,
    /// System text style.
    pub system: Style,
    /// Dim metadata style.
    pub metadata: Style,
    /// Error style.
    pub error: Style,
    /// Secondary hint style.
    pub hint: Style,
    /// Placeholder style.
    pub placeholder: Style,
}

impl MessageRenderTheme {
    /// Deterministic theme without role colors. The markdown accent keeps a
    /// neutral background so inline code remains visually distinct.
    pub const fn plain() -> Self {
        Self {
            user: Style::new(),
            assistant: Style::new(),
            assistant_text: Style::new(),
            markdown_accent: Style::new().bg(Color::Indexed(236)),
            system: Style::new(),
            metadata: Style::new(),
            error: Style::new(),
            hint: Style::new(),
            placeholder: Style::new(),
        }
    }

    /// Default colored theme derived from the design-system **active**
    /// palette (set via `rebon_design_system::theme::set_active_theme`).
    /// Each field is taken from one palette entry.
    pub fn default_styled() -> Self {
        let t = theme::get_active_theme();
        Self {
            // The `❯` pointer takes the theme's `suggestion` colour.
            user: Style::new()
                .fg(parse_theme_color(t.suggestion))
                .add_modifier(Modifier::BOLD),
            // The `⎿` prefix takes the inactive colour.
            assistant: Style::new().fg(parse_theme_color(t.inactive)),
            // Assistant body text takes the default text colour.
            // Use terminal default (no explicit fg) instead of
            // `Color::Rgb(255,255,255)` — some terminals collapse
            // explicit white to their theme's ANSI "white" which is
            // often a mid-gray on dark themes.
            assistant_text: Style::new(),
            markdown_accent: Style::new().fg(parse_theme_color(t.permission)),
            // System text takes the inactive colour.
            system: Style::new().fg(parse_theme_color(t.inactive)),
            metadata: Style::new().fg(parse_theme_color(t.subtle)),
            error: Style::new()
                .fg(parse_theme_color(t.error))
                .add_modifier(Modifier::BOLD),
            hint: Style::new().fg(parse_theme_color(t.warning)),
            placeholder: Style::new().fg(parse_theme_color(t.autoAccept)),
        }
    }
}

impl Default for MessageRenderTheme {
    fn default() -> Self {
        Self::default_styled()
    }
}

/// Render the `Message`-level projection to `ratatui::text::Text`.
pub fn render_message(input: &RenderMessageInput, theme: &MessageRenderTheme) -> Text<'static> {
    match (&input.message, project_message(input)) {
        (
            rebon_render::MessageRow::Attachment(attachment),
            MessageProjection::Attachment {
                add_margin,
                verbose,
                is_transcript_mode,
                ..
            },
        ) => attachment
            .attachment
            .as_ref()
            .map(|attachment_payload| {
                Text::from(prefixed_child_lines(
                    "◆",
                    render_attachment_projection(
                        &project_attachment_message(&AttachmentMessageInput {
                            add_margin,
                            verbose,
                            is_transcript_mode,
                            background: None,
                            path_separator: std::path::MAIN_SEPARATOR_STR.into(),
                            dot_glyph: "●".into(),
                            attachment: (*attachment_payload.clone()),
                        }),
                        &child_theme(theme),
                    ),
                    theme.placeholder,
                ))
            })
            .unwrap_or_else(|| single_line("●", "[attachment]", theme.placeholder)),
        (
            rebon_render::MessageRow::User(message),
            MessageProjection::UserCompactSummary { transcript_screen },
        ) => {
            let summary_text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    UserContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let display = project_compact_summary_card(&CompactSummaryInput {
                text_content: summary_text,
                is_transcript_mode: transcript_screen,
                metadata: None,
                history_shortcut: "Ctrl+O".into(),
            });
            Text::from(prefixed_child_lines(
                "❯",
                render_compact_summary_display(&display, &child_theme(theme)),
                theme.user,
            ))
        }
        (
            rebon_render::MessageRow::User(message),
            MessageProjection::UserContent { blocks, .. },
        ) => {
            let mut lines = Vec::new();
            for (block, projection) in message.content.iter().zip(blocks.iter()) {
                lines.extend(render_user_block(block, projection, theme));
            }
            Text::from(lines)
        }
        (
            rebon_render::MessageRow::Assistant(message),
            MessageProjection::Assistant { blocks, .. },
        ) => {
            let mut lines = Vec::new();
            for (block, projection) in message.content.iter().zip(blocks.iter()) {
                lines.extend(render_assistant_block(
                    block,
                    projection,
                    theme,
                    !message.is_stream_continuation,
                ));
            }
            Text::from(lines)
        }
        (_, MessageProjection::System(system)) => render_system_projection(&system, theme),
        _ => Text::default(),
    }
}

/// Render the transcript metadata line used by `MessageRow`.
pub fn render_metadata_line(
    width: u16,
    timestamp: Option<&MetadataLabelProjection>,
    model: Option<&MetadataLabelProjection>,
    theme: &MessageRenderTheme,
) -> Option<Line<'static>> {
    let mut parts = Vec::new();
    if let Some(timestamp) = timestamp {
        parts.push(timestamp.text.clone());
    }
    if let Some(model) = model {
        parts.push(model.text.clone());
    }
    if parts.is_empty() {
        return None;
    }
    let text = parts.join(" ");
    let text_width = WidthStr::width(text.as_str());
    let padding = width.saturating_sub(text_width as u16) as usize;
    Some(Line::from(vec![
        Span::raw(" ".repeat(padding)),
        Span::styled(text, theme.metadata),
    ]))
}

/// Render a `FallbackToolUseErrorProjection`.
pub fn render_fallback_tool_use_error(
    projection: &FallbackToolUseErrorProjection,
    theme: &MessageRenderTheme,
) -> Text<'static> {
    let mut lines = projection
        .error_text
        .lines()
        .map(|line| Line::from(Span::styled(line.to_owned(), theme.error)))
        .collect::<Vec<_>>();
    if let Some(footer) = projection.footer.as_ref() {
        lines.push(Line::from(Span::styled(footer.clone(), theme.hint)));
    }
    Text::from(lines)
}

/// Render a `FileEditUpdatedProjection`.
pub fn render_file_edit_updated(
    projection: &FileEditUpdatedProjection,
    theme: &MessageRenderTheme,
) -> Text<'static> {
    match projection {
        FileEditUpdatedProjection::PreviewHint { hint } => {
            Text::from(Line::from(Span::styled(hint.clone(), theme.hint)))
        }
        FileEditUpdatedProjection::SummaryOnly { summary } => {
            Text::from(Line::from(Span::styled(summary.clone(), theme.user)))
        }
        FileEditUpdatedProjection::Detailed {
            summary,
            file_path,
            diff_width,
            ..
        } => Text::from(vec![
            Line::from(Span::styled(summary.clone(), theme.user)),
            Line::from(Span::styled(
                format!("[diff {file_path} width={diff_width}]"),
                theme.placeholder,
            )),
        ]),
    }
}

/// Render a `FileEditRejectedProjection`.
pub fn render_file_edit_rejected(
    projection: &FileEditRejectedProjection,
    theme: &MessageRenderTheme,
) -> Text<'static> {
    match projection {
        FileEditRejectedProjection::SummaryOnly { summary } => {
            Text::from(Line::from(Span::styled(summary.clone(), theme.error)))
        }
        FileEditRejectedProjection::WritePreview {
            summary,
            preview,
            hidden_line_count,
            ..
        } => {
            let mut lines = vec![Line::from(Span::styled(summary.clone(), theme.error))];
            lines.extend(
                preview
                    .lines()
                    .map(|line| Line::from(Span::raw(line.to_owned()))),
            );
            if *hidden_line_count > 0 {
                lines.push(Line::from(Span::styled(
                    format!("... +{hidden_line_count} lines"),
                    theme.hint,
                )));
            }
            Text::from(lines)
        }
        FileEditRejectedProjection::DiffPreview {
            summary,
            file_path,
            diff_width,
            ..
        } => Text::from(vec![
            Line::from(Span::styled(summary.clone(), theme.error)),
            Line::from(Span::styled(
                format!("[rejected diff {file_path} width={diff_width}]"),
                theme.placeholder,
            )),
        ]),
    }
}

/// Render a `NotebookEditRejectedProjection`.
pub fn render_notebook_edit_rejected(
    projection: &NotebookEditRejectedProjection,
    theme: &MessageRenderTheme,
) -> Text<'static> {
    let mut lines = vec![Line::from(Span::styled(
        projection.summary.clone(),
        theme.error,
    ))];
    if let Some(preview) = projection.preview.as_ref() {
        lines.extend(
            preview
                .lines()
                .map(|line| Line::from(Span::raw(line.to_owned()))),
        );
    }
    Text::from(lines)
}

fn render_user_block(
    block: &UserContentBlock,
    projection: &UserBlockProjection,
    theme: &MessageRenderTheme,
) -> Vec<Line<'static>> {
    let mut lines = margin_lines(user_block_add_margin(projection));
    match (block, projection) {
        (
            UserContentBlock::Text { text },
            UserBlockProjection::Text {
                add_margin,
                verbose,
                plan_content,
                timestamp,
                is_transcript_mode,
            },
        ) => {
            let child_projection = project_user_text(&UserTextInput {
                add_margin: *add_margin,
                text: text.clone(),
                verbose: *verbose,
                plan_content: plan_content.clone(),
                is_transcript_mode: *is_transcript_mode,
                timestamp: timestamp.clone(),
                github_webhooks_enabled: false,
                fork_subagent_enabled: false,
                uds_inbox_enabled: false,
                channels_enabled: false,
                agent_swarms_enabled: true,
            });
            let prefix = if child_projection.is_teammate_task_completion() {
                "●"
            } else if child_projection.is_plain_teammate_message() {
                "›"
            } else {
                "❯"
            };
            let child = render_user_text_projection(&child_projection, &child_theme(theme));
            lines.extend(prefixed_child_lines(prefix, child, theme.user));
        }
        (UserContentBlock::Image { .. }, UserBlockProjection::Image { image_id, .. }) => {
            // Render the image as a `⎿` continuation line under the
            // user text, mirroring the tool-result chevron style. The
            // number matches the `[Image #N]` chip the user typed —
            // `ImageKey::PasteId` carries the id when the caller threaded
            // it through; `ImageKey::Position` is the 1-based fallback.
            let label = match image_id {
                ImageKey::PasteId(id) => format!("[Image #{id}]"),
                ImageKey::Position(position) => format!("[Image #{position}]"),
            };
            lines.push(Line::from(vec![
                Span::styled("  ⎿  ", theme.assistant),
                Span::raw(label),
            ]));
        }
        (
            UserContentBlock::ToolResult {
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
        ) => {
            let child = render_user_tool_result_projection(
                &project_user_tool_result(&UserToolResultInput {
                    tool_use_id: tool_use_id.clone().unwrap_or_else(|| "tool".into()),
                    content: content.clone().unwrap_or_default(),
                    is_error: *is_error,
                    tool_exists: tool_use_id.is_some(),
                    tool_has_custom_reject_renderer: false,
                    tool_has_custom_error_renderer: false,
                    renders_as_assistant_text: false,
                    input_summary: None,
                    verbose: *verbose,
                    is_transcript_mode: *is_transcript_mode,
                    width: width.to_string(),
                    classifier_rule: None,
                    yolo_reason: None,
                    classifier_denial: false,
                }),
                &child_theme(theme),
            );
            lines.extend(prefixed_child_lines(
                "↳",
                child,
                if *is_error { theme.error } else { theme.user },
            ));
            if let Some(content) = content.as_ref().filter(|content| !content.is_empty()) {
                lines.extend(
                    content
                        .lines()
                        .map(|line| Line::from(Span::raw(line.to_owned()))),
                );
            }
        }
        _ => {}
    }
    lines
}

fn render_assistant_block(
    block: &AssistantContentBlock,
    projection: &AssistantBlockProjection,
    theme: &MessageRenderTheme,
    show_gutter_dot: bool,
) -> Vec<Line<'static>> {
    let mut lines = margin_lines(assistant_block_add_margin(projection));
    match (block, projection) {
        (
            AssistantContentBlock::ToolUse {
                name,
                input_summary,
                id,
                diff,
                body_lines,
            },
            AssistantBlockProjection::ToolUse { add_margin },
        ) => {
            if let Some((path, old_text, new_text)) = diff {
                // Edit/Write tool with diff data — render as file-edit diff.
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
                    style_condensed: false,
                    verbose: true,
                    preview_hint: None,
                    columns: 80,
                });
                let edit_text = render_file_edit_updated(&projection, theme);
                lines.extend(prefixed_child_lines("●", edit_text, theme.assistant));
            } else {
                let name = name.clone().unwrap_or_else(|| "tool_use".into());
                let child = render_assistant_tool_use_projection(
                    &project_assistant_tool_use(&AssistantToolUseInput {
                        tool_use: AssistantToolUseInvocation {
                            id: id.clone().unwrap_or_else(|| format!("{name}-id")),
                            name: name.clone(),
                        },
                        tools_available: true,
                        tool: Some(AssistantToolDefinition {
                            name: name.clone(),
                            user_facing_name: name,
                            user_facing_name_background_color: None,
                            is_transparent_wrapper: false,
                            renders: AssistantToolRenderOutputs {
                                message: input_summary.clone(),
                                tag: None,
                                progress_message: None,
                                queued_message: None,
                                hook_progress_message: None,
                            },
                        }),
                        add_margin: *add_margin,
                        in_progress_tool_use_ids: Default::default(),
                        resolved_tool_use_ids: Default::default(),
                        errored_tool_use_ids: Default::default(),
                        should_animate: false,
                        should_show_dot: false,
                        background: None,
                        pending_worker_tool_use_id: None,
                        dot_glyph: "●".into(),
                    }),
                    &child_theme(theme),
                );
                lines.extend(prefixed_child_lines("●", child, theme.assistant));
                for line in body_lines {
                    lines.extend(prefixed_lines("⎿", line, theme.assistant));
                }
            }
        }
        (
            AssistantContentBlock::Text { text },
            AssistantBlockProjection::Text {
                add_margin,
                verbose,
            },
        ) => {
            let child = render_assistant_text_projection(
                &project_assistant_text_message(&AssistantTextMessageInput {
                    text: normalize_assistant_display_text(text),
                    add_margin: *add_margin,
                    should_show_dot: show_gutter_dot,
                    verbose: *verbose,
                    is_selected: false,
                    dot_glyph: "●".into(),
                    rate_limit_message: None,
                    upgrade_hint: None,
                    default_sonnet_model_name: "Sonnet".into(),
                    is_keychain_locked: false,
                    api_timeout_ms: None,
                }),
                &child_theme(theme),
            );
            lines.extend(prefixed_child_lines(
                if show_gutter_dot { "●" } else { " " },
                child,
                theme.assistant,
            ));
        }
        (
            AssistantContentBlock::RedactedThinking { .. },
            AssistantBlockProjection::RedactedThinking { hidden, .. },
        ) => {
            if !hidden {
                lines.extend(prefixed_lines("·", "[redacted thinking]", theme.hint));
            }
        }
        (
            AssistantContentBlock::Thinking { thinking },
            AssistantBlockProjection::Thinking {
                add_margin,
                is_transcript_mode,
                verbose,
                hide_in_transcript,
            },
        ) => {
            let child = render_assistant_thinking_projection(
                &project_assistant_thinking_message(&AssistantThinkingMessageInput {
                    thinking: thinking.clone().unwrap_or_default(),
                    add_margin: *add_margin,
                    is_transcript_mode: *is_transcript_mode,
                    verbose: *verbose,
                    hide_in_transcript: *hide_in_transcript,
                    compact_preview: false,
                    show_expand_hint: false,
                }),
                &child_theme(theme),
            );
            lines.extend(prefixed_child_lines("·", child, theme.hint));
        }
        (
            AssistantContentBlock::ConnectorText { connector_text },
            AssistantBlockProjection::ConnectorText { .. },
        ) => {
            lines.extend(prefixed_lines("●", connector_text, theme.assistant));
        }
        (
            AssistantContentBlock::AdvisorBlock { raw_type },
            AssistantBlockProjection::Advisor { .. },
        ) => {
            lines.extend(prefixed_lines(
                "ADV",
                &format!("[advisor {raw_type}]"),
                theme.placeholder,
            ));
        }
        (_, AssistantBlockProjection::Null) => {}
        _ => {}
    }
    lines
}

fn render_system_projection(
    projection: &SystemProjection,
    theme: &MessageRenderTheme,
) -> Text<'static> {
    match projection {
        SystemProjection::Hidden => Text::default(),
        SystemProjection::CompactBoundary => single_line("─", "[compact boundary]", theme.system),
        SystemProjection::LocalCommandAsUserText { text, .. } => {
            Text::from(prefixed_first_line("❯", text, theme.user))
        }
        SystemProjection::SystemText {
            add_margin,
            verbose,
            raw_subtype,
            level,
            text,
        } => {
            let projection = project_system_text_message(&SystemTextMessageInput {
                add_margin: *add_margin,
                verbose: *verbose,
                is_transcript_mode: false,
                background: None,
                terminal_columns: None,
                message: SystemTextProjectionInput::Generic {
                    subtype: raw_subtype
                        .clone()
                        .unwrap_or_else(|| "informational".into()),
                    level: level.clone().unwrap_or_else(|| "info".into()),
                    content: Some(text.clone()),
                },
            });
            let child = render_system_text_projection(&projection, &child_theme(theme));
            if child.lines.is_empty() {
                Text::default()
            } else {
                child
            }
        }
    }
}

fn single_line(prefix: &str, text: &str, style: Style) -> Text<'static> {
    Text::from(Line::from(vec![
        Span::styled(format!("{prefix} "), style),
        Span::raw(text.to_owned()),
    ]))
}

fn prefixed_first_line(prefix: &str, text: &str, style: Style) -> Vec<Line<'static>> {
    let mut lines = text.lines();
    let prefix_str = format!("{prefix} ");
    let Some(first) = lines.next() else {
        return vec![Line::from(vec![Span::styled(prefix_str, style)])];
    };
    let mut out = vec![Line::from(vec![
        Span::styled(prefix_str, style),
        Span::raw(first.to_owned()),
    ])];
    out.extend(lines.map(|line| Line::from(Span::raw(line.to_owned()))));
    out
}

fn prefixed_lines(prefix: &str, text: &str, style: Style) -> Vec<Line<'static>> {
    let mut lines = text.lines();
    let prefix_str = format!("{prefix} ");
    let Some(first) = lines.next() else {
        return vec![Line::from(vec![Span::styled(prefix_str, style)])];
    };
    let indent = " ".repeat(WidthStr::width(prefix_str.as_str()));
    let mut out = vec![Line::from(vec![
        Span::styled(prefix_str, style),
        Span::raw(first.to_owned()),
    ])];
    out.extend(
        lines.map(|line| Line::from(vec![Span::raw(indent.clone()), Span::raw(line.to_owned())])),
    );
    out
}

fn prefixed_child_lines(prefix: &str, text: Text<'static>, style: Style) -> Vec<Line<'static>> {
    let mut lines = text.lines.into_iter();
    let Some(first) = lines.next() else {
        return vec![Line::from(vec![Span::styled(format!("{prefix} "), style)])];
    };
    let prefix_str = format!("{prefix} ");
    let indent = " ".repeat(WidthStr::width(prefix_str.as_str()));
    let mut first_spans = vec![Span::styled(prefix_str, style)];
    first_spans.extend(first.spans);
    let mut out = vec![Line::from(first_spans)];
    for line in lines {
        let mut spans = vec![Span::raw(indent.clone())];
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    out
}

fn child_theme(theme: &MessageRenderTheme) -> MessagesRenderTheme {
    MessagesRenderTheme {
        text: theme.assistant_text,
        dim: theme.assistant,
        error: theme.error,
        warning: theme.hint,
        // Tool names take the default text colour plus bold, not the
        // user/suggestion colour.
        accent: theme.assistant_text.add_modifier(Modifier::BOLD),
    }
}

fn margin_lines(add_margin: bool) -> Vec<Line<'static>> {
    if add_margin {
        vec![Line::default()]
    } else {
        Vec::new()
    }
}

fn user_block_add_margin(projection: &UserBlockProjection) -> bool {
    match projection {
        UserBlockProjection::Text { add_margin, .. } => *add_margin,
        UserBlockProjection::Image { add_margin, .. } => *add_margin,
        UserBlockProjection::ToolResult { .. } => false,
    }
}

fn assistant_block_add_margin(projection: &AssistantBlockProjection) -> bool {
    match projection {
        AssistantBlockProjection::ToolUse { add_margin }
        | AssistantBlockProjection::Text { add_margin, .. }
        | AssistantBlockProjection::RedactedThinking { add_margin, .. }
        | AssistantBlockProjection::Thinking { add_margin, .. }
        | AssistantBlockProjection::ConnectorText { add_margin, .. }
        | AssistantBlockProjection::Advisor { add_margin, .. } => *add_margin,
        AssistantBlockProjection::Null => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_render::{
        project_message_row, AssistantContentBlock, AssistantMessage, FileEditOperation,
        FileEditRejectedInput, FileEditUpdatedInput, MessageRow, MessageRowProjectionInput,
        NotebookCellType, NotebookEditMode, NotebookEditRejectedInput, SystemMessage,
        SystemSubtype, TimelineLookups, TimelineMessage, TimelineScreen, UserContentBlock,
        UserMessage,
    };

    fn plain_lines(text: Text<'static>) -> Vec<String> {
        text.lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn normalize_assistant_display_text_collapses_summary_envelopes() {
        assert_eq!(
            normalize_assistant_display_text(
                r#"{"summary":"fixed mobile chat refresh when desktop session is paused"}"#
            ),
            "fixed mobile chat refresh when desktop session is paused"
        );
        // Reminder stripping still applies before the envelope check.
        assert_eq!(
            normalize_assistant_display_text(
                "<system-reminder>noise</system-reminder>\n{\"summary\":\"tidy\"}"
            ),
            "tidy"
        );
        // Ordinary answers — including ones that merely mention such
        // JSON — pass through unchanged.
        assert_eq!(
            normalize_assistant_display_text(r#"Done: {"summary":"x"}"#),
            r#"Done: {"summary":"x"}"#
        );
        assert_eq!(normalize_assistant_display_text("plain"), "plain");
    }

    #[test]
    fn child_themes_use_inactive_for_dim_details_in_light_and_dark() {
        for theme_name in [theme::ThemeName::Light, theme::ThemeName::Dark] {
            let palette = theme::get_theme(theme_name);
            let inactive = Style::new().fg(parse_theme_color(palette.inactive));
            let subtle = Style::new().fg(parse_theme_color(palette.subtle));
            let parent = MessageRenderTheme {
                assistant: inactive,
                metadata: subtle,
                ..MessageRenderTheme::plain()
            };

            assert_ne!(inactive, subtle);
            assert_eq!(child_theme(&parent).dim, inactive);
            assert_eq!(
                crate::widget_subtree::child_theme_for(&parent).dim,
                inactive
            );
        }
    }

    #[test]
    fn render_message_renders_user_text_and_tool_result_bodies() {
        let input = RenderMessageInput {
            message: MessageRow::User(UserMessage {
                uuid: "u1".into(),
                is_compact_summary: false,
                content: vec![
                    UserContentBlock::Text {
                        text: "hello".into(),
                    },
                    UserContentBlock::ToolResult {
                        tool_use_id: Some("toolu-1".into()),
                        content: Some("line1\nline2".into()),
                        is_error: false,
                    },
                ],
                image_paste_ids: Vec::new(),
                plan_content: None,
                timestamp: None,
            }),
            container_width: Some(80),
            add_margin: true,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 120,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let lines = plain_lines(render_message(&input, &MessageRenderTheme::plain()));
        assert!(lines.iter().any(|line| line.contains("❯ hello")));
        assert!(lines
            .iter()
            .any(|line| line.contains("[tool result toolu-1]")));
        assert!(lines.iter().any(|line| line.contains("line2")));
    }

    #[test]
    fn render_message_keeps_the_image_chip_directly_under_the_user_text() {
        let input = RenderMessageInput {
            message: MessageRow::User(UserMessage {
                uuid: "u1".into(),
                is_compact_summary: false,
                content: vec![
                    UserContentBlock::Text {
                        text: "look at this".into(),
                    },
                    UserContentBlock::Image { source_hint: None },
                ],
                image_paste_ids: vec![Some("1".into())],
                plan_content: None,
                timestamp: None,
            }),
            container_width: Some(80),
            add_margin: true,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 120,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let lines = plain_lines(render_message(&input, &MessageRenderTheme::plain()));
        let text_at = lines
            .iter()
            .position(|line| line.contains("look at this"))
            .unwrap_or_else(|| panic!("{lines:?}"));
        let chip_at = lines
            .iter()
            .position(|line| line.contains("[Image #1]"))
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert_eq!(chip_at, text_at + 1, "{lines:?}");
    }

    #[test]
    fn render_message_shows_redacted_in_transcript_and_visible_last_thinking() {
        let input = RenderMessageInput {
            message: MessageRow::Assistant(AssistantMessage {
                uuid: "a1".into(),
                content: vec![
                    AssistantContentBlock::RedactedThinking { data: None },
                    AssistantContentBlock::Thinking {
                        thinking: Some("secret".into()),
                    },
                ],
                advisor_model: None,
                is_stream_continuation: false,
            }),
            container_width: Some(80),
            add_margin: true,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: true,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: Some("a1:1".into()),
            latest_bash_output_uuid: None,
            terminal_columns: 120,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let lines = plain_lines(render_message(&input, &MessageRenderTheme::plain()));
        assert!(lines
            .iter()
            .any(|line| line.contains("[redacted thinking]")));
        assert!(lines.iter().any(|line| line.contains("secret")));
    }

    #[test]
    fn render_message_renders_system_compact_boundary_and_group_placeholders() {
        let boundary = RenderMessageInput {
            message: MessageRow::System(SystemMessage {
                uuid: "s1".into(),
                subtype: SystemSubtype::CompactBoundary,
                raw_subtype: Some("compact_boundary".into()),
                level: Some("info".into()),
                content: String::new(),
                stop_hook_summary: None,
            }),
            container_width: Some(80),
            add_margin: true,
            verbose: false,
            style_condensed: false,
            is_transcript_mode: false,
            is_active_collapsed_group: false,
            is_user_continuation: false,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            terminal_columns: 120,
            fullscreen_env_enabled: false,
            compact_thinking_preview: false,
            show_thinking_expand_hint: false,
            frame_time_ms: 0,
            in_progress_tool_use_ids: Vec::new(),
            errored_tool_use_ids: Vec::new(),
            show_tool_expand_hint: false,
        };
        let lines = plain_lines(render_message(&boundary, &MessageRenderTheme::plain()));
        assert!(lines[0].contains("[compact boundary]"));
    }

    #[test]
    fn render_metadata_line_right_aligns_timestamp_and_model() {
        let timestamp = MetadataLabelProjection {
            text: "01:23 PM".into(),
            min_width: 8,
        };
        let model = MetadataLabelProjection {
            text: "sonnet".into(),
            min_width: 6,
        };
        let line = render_metadata_line(
            20,
            Some(&timestamp),
            Some(&model),
            &MessageRenderTheme::plain(),
        )
        .unwrap();
        let text = line
            .spans
            .into_iter()
            .map(|span| span.content.to_string())
            .collect::<String>();
        assert!(text.ends_with("01:23 PM sonnet"));
        assert_eq!(text.len(), 20);
    }

    #[test]
    fn render_fallback_and_file_edit_projections_produce_ratatui_text() {
        let fallback = FallbackToolUseErrorProjection {
            error_text: "boom".into(),
            hidden_line_count: 2,
            footer: Some("... +2 lines".into()),
        };
        let lines = plain_lines(render_fallback_tool_use_error(
            &fallback,
            &MessageRenderTheme::plain(),
        ));
        assert!(lines.iter().any(|line| line.contains("boom")));
        assert!(lines.iter().any(|line| line.contains("... +2 lines")));

        let updated = rebon_render::project_file_edit_updated(&FileEditUpdatedInput {
            file_path: "src/lib.rs".into(),
            structured_patch: vec![rebon_render::StructuredPatchHunk {
                lines: vec!["+a".into(), "-b".into()],
            }],
            first_line: None,
            file_content: None,
            style_condensed: false,
            verbose: true,
            preview_hint: None,
            columns: 80,
        });
        let lines = plain_lines(render_file_edit_updated(
            &updated,
            &MessageRenderTheme::plain(),
        ));
        assert!(lines.iter().any(|line| line.contains("Edit (src/lib.rs)")
            && line.contains("Added 1 line, removed 1 line")));

        let rejected = rebon_render::project_file_edit_rejected(&FileEditRejectedInput {
            file_path: "C:/repo/src/lib.rs".into(),
            operation: FileEditOperation::Update,
            patch: vec![rebon_render::StructuredPatchHunk {
                lines: vec!["+a".into()],
            }],
            first_line: None,
            file_content: None,
            content: None,
            style_condensed: false,
            verbose: false,
            columns: 80,
            cwd: Some("C:/repo".into()),
        });
        let lines = plain_lines(render_file_edit_rejected(
            &rejected,
            &MessageRenderTheme::plain(),
        ));
        assert!(lines.iter().any(|line| line.contains("[rejected diff")));

        let notebook = rebon_render::project_notebook_edit_rejected(&NotebookEditRejectedInput {
            notebook_path: "C:/repo/demo.ipynb".into(),
            cell_id: Some("cell-1".into()),
            new_source: "print(1)".into(),
            cell_type: Some(NotebookCellType::Code),
            edit_mode: NotebookEditMode::Replace,
            verbose: false,
            cwd: Some("C:/repo".into()),
        });
        let lines = plain_lines(render_notebook_edit_rejected(
            &notebook,
            &MessageRenderTheme::plain(),
        ));
        assert!(lines
            .iter()
            .any(|line| line.contains("User rejected replace cell in")));
        assert!(lines.iter().any(|line| line.contains("print(1)")));
    }

    #[test]
    fn row_projection_and_render_can_form_transcript_metadata_layout() {
        let timeline_message = TimelineMessage::Assistant(rebon_render::AssistantTimelineMessage {
            uuid: "a-row".into(),
            is_api_error_message: false,
            timestamp: Some("2026-04-08T12:00:00Z".into()),
            model: Some("sonnet".into()),
            content: vec![rebon_render::TimelineContentBlock::Text(
                rebon_render::TextBlock {
                    text: "hello".into(),
                },
            )],
        });
        let row = project_message_row(&MessageRowProjectionInput {
            message: timeline_message.clone(),
            display_message: timeline_message.clone(),
            has_content_after: false,
            in_progress_tool_use_ids: Default::default(),
            streaming_tool_use_ids: Default::default(),
            sibling_tool_use_ids: Default::default(),
            screen: TimelineScreen::Transcript,
            can_animate: false,
            columns: 40,
            is_loading: false,
            lookups: TimelineLookups::default(),
        });
        assert!(row.has_metadata);
        let timestamp =
            rebon_render::project_message_timestamp(&timeline_message, true, "01:23 PM");
        let model = rebon_render::project_message_model(&timeline_message, true);
        let line = render_metadata_line(
            20,
            timestamp.as_ref(),
            model.as_ref(),
            &MessageRenderTheme::plain(),
        )
        .unwrap();
        let text = line
            .spans
            .into_iter()
            .map(|span| span.content.to_string())
            .collect::<String>();
        assert!(text.ends_with("01:23 PM sonnet"));
    }
}
