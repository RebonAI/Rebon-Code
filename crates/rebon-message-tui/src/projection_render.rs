//! `ratatui` renderers for high-value child projections used by the
//! message composition layer.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use rebon_design_system::{format_shortcut_hint, theme};

use rebon_render::{
    assistant_text::{AssistantResponseBlock, AssistantTextMessageProjection},
    assistant_tool_use::{
        AssistantToolLeadingDisplay, AssistantToolSecondaryDisplay, AssistantToolUseProjection,
    },
    attachment::{
        AttachmentProjection, AttachmentTaskStatusDisplay, AttachmentTeammateMailboxItemDisplay,
    },
    compact_summary::CompactSummaryDisplay,
    plan_approval::{PlanApprovalRenderable, PlanApprovalResponseDisplay},
    simple_messages::StatusColor,
    system_text::{
        StopHookSummaryDisplay, SystemGenericTextDisplay, SystemTextProjection, SystemVisualMarker,
    },
    teammate_messages::{TeammateMessageContentDisplay, TeammateRenderable},
    thinking::{
        AssistantThinkingMessageProjection, HighlightedThinkingTextProjection, ThinkingThemeColor,
    },
    tool_results::{UserToolErrorProjection, UserToolResultProjection},
    user_text::{
        format_user_prompt_hidden_separator, project_user_prompt_display_lines,
        UserPromptDisplayLine, UserTextProjection, USER_PROMPT_FOLD_DEFAULT_WIDTH,
    },
};

/// Theme-aware neutral-gray style for fold separators (`──── (N lines hidden) ─`).
/// Light themes need the darker ANSI gray to remain visible on the user-message
/// background; dark themes keep the brighter gray.
pub fn fold_separator_style() -> Style {
    Style::new().fg(fold_separator_color(theme::active_theme_name()))
}

fn fold_separator_color(theme_name: theme::ThemeName) -> Color {
    match theme_name {
        theme::ThemeName::Light
        | theme::ThemeName::LightDaltonized
        | theme::ThemeName::LightAnsi => Color::DarkGray,
        theme::ThemeName::Dark | theme::ThemeName::DarkDaltonized | theme::ThemeName::DarkAnsi => {
            Color::Gray
        }
    }
}

/// Small style bag for child renderers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessagesRenderTheme {
    /// Primary text.
    pub text: Style,
    /// Secondary/dim text.
    pub dim: Style,
    /// Error text.
    pub error: Style,
    /// Warning text.
    pub warning: Style,
    /// Accent text.
    pub accent: Style,
}

impl MessagesRenderTheme {
    /// Deterministic no-style theme.
    pub const fn plain() -> Self {
        Self {
            text: Style::new(),
            dim: Style::new(),
            error: Style::new(),
            warning: Style::new(),
            accent: Style::new(),
        }
    }

    /// Default colored theme derived from the design-system **active**
    /// palette (set via `rebon_design_system::theme::set_active_theme`).
    pub fn default_styled() -> Self {
        let t = theme::get_active_theme();
        Self {
            text: Style::new(),
            dim: Style::new().fg(parse_theme_color(t.inactive)),
            error: Style::new()
                .fg(parse_theme_color(t.error))
                .add_modifier(Modifier::BOLD),
            warning: Style::new().fg(parse_theme_color(t.warning)),
            accent: Style::new().fg(parse_theme_color(t.suggestion)),
        }
    }
}
fn tool_name_style(theme: &MessagesRenderTheme) -> Style {
    theme.accent.add_modifier(Modifier::BOLD)
}

impl Default for MessagesRenderTheme {
    fn default() -> Self {
        Self::default_styled()
    }
}

/// Render `CompactSummaryDisplay`.
pub fn render_compact_summary_display(
    display: &CompactSummaryDisplay,
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let title_text = display
        .user_prompt
        .as_deref()
        .unwrap_or(display.title.as_str());
    lines.push(Line::from(Span::styled(
        title_text.to_owned(),
        theme.accent,
    )));

    if !display.file_entries.is_empty() {
        for entry in &display.file_entries {
            let label = if let Some(ref count) = entry.line_count {
                format!("\u{23bf}  Read {} ({count})", entry.path)
            } else {
                format!("\u{23bf}  Referenced file {}", entry.path)
            };
            lines.push(Line::from(Span::styled(label, theme.dim)));
        }
    } else if let Some(ref metadata) = display.metadata_line {
        for l in metadata.lines() {
            lines.push(Line::from(Span::styled(l.to_owned(), theme.dim)));
        }
    }

    if let Some(ref context) = display.context_line {
        lines.push(Line::from(Span::styled(context.clone(), theme.dim)));
    }

    if let Some(ref hint) = display.shortcut_hint {
        lines.push(Line::from(Span::styled(hint.clone(), theme.dim)));
    }

    if let Some(ref text) = display.transcript_text {
        for l in text.lines() {
            lines.push(Line::from(Span::raw(l.to_owned())));
        }
    }

    Text::from(lines)
}

/// Render `UserTextProjection`.
pub fn render_user_text_projection(
    projection: &UserTextProjection,
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    match projection {
        UserTextProjection::HiddenNoContent
        | UserTextProjection::HiddenTick
        | UserTextProjection::HiddenLocalCommandCaveat => Text::default(),
        UserTextProjection::Plan { plan_content, .. } => Text::from(
            std::iter::once(Line::from(Span::styled("Plan to implement", theme.accent)))
                .chain(
                    plan_content
                        .lines()
                        .map(|line| Line::from(Span::raw(line.to_owned()))),
                )
                .collect::<Vec<_>>(),
        ),
        UserTextProjection::Interrupted => Text::from(Line::from(Span::styled(
            "[interrupted by user]",
            theme.warning,
        ))),
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
                            format_user_prompt_hidden_separator(
                                hidden_line_count,
                                USER_PROMPT_FOLD_DEFAULT_WIDTH,
                            ),
                            fold_separator_style(),
                        ))
                    }
                })
                .collect::<Vec<_>>(),
        ),
        UserTextProjection::Teammate(renderables) => {
            render_teammate_renderables(renderables, theme)
        }
        UserTextProjection::TaskNotification(notification) => {
            let style = match notification.color {
                StatusColor::Success => theme.accent,
                StatusColor::Error => theme.error,
                StatusColor::Warning => theme.warning,
                StatusColor::Text => theme.text,
            };
            Text::from(
                notification
                    .summary
                    .lines()
                    .map(|line| Line::from(Span::styled(line.to_owned(), style)))
                    .collect::<Vec<_>>(),
            )
        }
        other => Text::from(Line::from(Span::styled(format!("{other:?}"), theme.dim))),
    }
}

/// Render `AttachmentProjection`.
pub fn render_attachment_projection(
    projection: &AttachmentProjection,
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    match projection {
        AttachmentProjection::Hidden => Text::default(),
        AttachmentProjection::Lines(lines) => Text::from(
            lines
                .iter()
                .map(|line| {
                    let style = match line.tone {
                        Some(rebon_render::attachment::AttachmentTone::Error) => theme.error,
                        Some(rebon_render::attachment::AttachmentTone::Warning) => theme.warning,
                        None if line.dim => theme.dim,
                        None => theme.text,
                    };
                    Line::from(Span::styled(line.text.clone(), style))
                })
                .collect::<Vec<_>>(),
        ),
        AttachmentProjection::RelevantMemories(display) => {
            let expand_hint = display
                .show_expand_hint
                .then(|| {
                    format!(
                        " ({})",
                        format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
                    )
                })
                .unwrap_or_default();
            let mut lines = vec![Line::from(Span::styled(
                format!(
                    "Recalled {} {}{}",
                    display.count, display.count_word, expand_hint
                ),
                theme.dim,
            ))];
            if display.show_entries {
                lines.extend(display.entries.iter().map(|entry| {
                    let suffix = entry
                        .transcript_body
                        .as_ref()
                        .map(|content| format!(": {content}"))
                        .unwrap_or_default();
                    Line::from(Span::styled(
                        format!("{}{}", entry.basename, suffix),
                        theme.dim,
                    ))
                }));
            }
            Text::from(lines)
        }
        AttachmentProjection::QueuedCommand(display) => Text::from(
            std::iter::once(Line::from(Span::styled(
                display.prompt_text.clone(),
                theme.text,
            )))
            .chain(
                display
                    .image_paste_ids
                    .iter()
                    .map(|id| Line::from(Span::styled(format!("[image #{id}]"), theme.dim))),
            )
            .collect::<Vec<_>>(),
        ),
        AttachmentProjection::Diagnostics(display) => match display {
            rebon_render::diagnostics::DiagnosticsProjection::Summary {
                total_issues,
                file_count,
                issue_word,
                file_word,
            } => Text::from(Line::from(Span::styled(
                format!("{total_issues} {issue_word} across {file_count} {file_word}"),
                theme.warning,
            ))),
            rebon_render::diagnostics::DiagnosticsProjection::Verbose { files } => Text::from(
                files
                    .iter()
                    .flat_map(|file| {
                        std::iter::once(Line::from(Span::styled(
                            format!("{} {}:", file.display_path, file.uri_suffix),
                            theme.dim,
                        )))
                        .chain(
                            file.diagnostics
                                .iter()
                                .map(|line| Line::from(Span::styled(line.clone(), theme.text))),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
        },
        AttachmentProjection::TaskStatus(display) => match display {
            AttachmentTaskStatusDisplay::Generic {
                dot_glyph,
                description,
                status_text,
                ..
            } => Text::from(Line::from(Span::styled(
                format!("{dot_glyph} Task \"{description}\" {status_text}"),
                theme.dim,
            ))),
            AttachmentTaskStatusDisplay::Teammate {
                dot_glyph,
                agent_name,
                status_text,
                ..
            } => Text::from(Line::from(Span::styled(
                format!("{dot_glyph} Teammate @{agent_name} {status_text}"),
                theme.dim,
            ))),
        },
        AttachmentProjection::TeammateMailbox(display) => Text::from(
            display
                .items
                .iter()
                .flat_map(|item| render_teammate_mailbox_item(item, theme))
                .collect::<Vec<_>>(),
        ),
    }
}

// 通用 child accent 是工具标题的粗体样式，不能为 provider 提示改变它的颜色。
pub(crate) fn provider_switch_lines(
    provider: &str,
    model: &str,
    theme: &MessagesRenderTheme,
) -> [Line<'static>; 2] {
    let provider_style = theme
        .text
        .fg(parse_theme_color(theme::get_active_theme().permission));
    [
        Line::from(vec![
            Span::styled("● ", theme.dim),
            Span::styled("Switched to provider ", theme.text),
            Span::styled(provider.to_owned(), provider_style),
        ]),
        Line::from(Span::styled(format!("  ⎿ Using model {model}"), theme.dim)),
    ]
}

/// Render `SystemTextProjection`.
pub fn render_system_text_projection(
    projection: &SystemTextProjection,
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    match projection {
        SystemTextProjection::Hidden => Text::default(),
        SystemTextProjection::ProviderSwitch {
            margin_top,
            provider,
            model,
            ..
        } => Text::from(
            std::iter::repeat_with(Line::default)
                .take(usize::from(*margin_top))
                .chain(provider_switch_lines(provider, model, theme))
                .collect::<Vec<_>>(),
        ),
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
            if let Some(summary) = &display.background_task_summary {
                if !text.is_empty() {
                    text.push_str(" · ");
                }
                text.push_str(summary);
            }
            Text::from(render_system_line(Some(display.marker), text, theme.dim))
        }
        SystemTextProjection::MemorySaved(display) => {
            Text::from(
                std::iter::once(Line::from(Span::styled(
                    format!("{} {}", display.verb, display.parts.join(" · ")),
                    theme.text,
                )))
                .chain(display.entries.iter().map(|entry| {
                    Line::from(Span::styled(format!("  {}", entry.basename), theme.dim))
                }))
                .collect::<Vec<_>>(),
            )
        }
        SystemTextProjection::AwaySummary(display)
        | SystemTextProjection::AgentsKilled(display)
        | SystemTextProjection::ScheduledTaskFire(display)
        | SystemTextProjection::Generic(display)
        | SystemTextProjection::Thinking(display) => {
            Text::from(render_generic_system_display(display, theme))
        }
        SystemTextProjection::BridgeStatus(display) => Text::from(
            std::iter::once(Line::from(Span::styled(display.intro, theme.text)))
                .chain(std::iter::once(Line::from(Span::styled(
                    display.url.clone(),
                    theme.accent,
                ))))
                .chain(
                    display
                        .upgrade_nudge
                        .as_ref()
                        .map(|line| Line::from(Span::styled(line.clone(), theme.dim))),
                )
                .collect::<Vec<_>>(),
        ),
        SystemTextProjection::PermissionRetry {
            commands, marker, ..
        } => Text::from(render_system_line(
            Some(*marker),
            format!("Allowed {commands}"),
            theme.text,
        )),
        SystemTextProjection::ApiError(display) => {
            let mut lines = Vec::new();
            if let Some(error) = display.displayed_error.as_ref() {
                lines.push(Line::from(Span::styled(
                    format!("  \u{23bf}  {error}"),
                    theme.error,
                )));
            }
            if display.show_expand_hint {
                lines.push(Line::from(Span::styled(
                    format!(
                        "     {}",
                        format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
                    ),
                    theme.dim,
                )));
            }
            if let Some(line) = display.retry_text.as_ref() {
                lines.push(Line::from(Span::styled(format!("     {line}"), theme.dim)));
            }
            Text::from(lines)
        }
        SystemTextProjection::StopHookSummary(display) => match display {
            StopHookSummaryDisplay::Labeled {
                summary,
                transcript_lines,
            } => Text::from(
                std::iter::once(Line::from(Span::styled(summary.clone(), theme.dim)))
                    .chain(
                        transcript_lines
                            .iter()
                            .map(|line| Line::from(Span::styled(line.clone(), theme.dim))),
                    )
                    .collect::<Vec<_>>(),
            ),
            StopHookSummaryDisplay::Default {
                summary,
                detail_lines,
                prevented_line,
                error_lines,
                show_expand_hint,
                marker,
                ..
            } => Text::from(
                std::iter::once(render_system_line(
                    Some(*marker),
                    summary.clone(),
                    theme.text,
                ))
                .chain((*show_expand_hint).then(|| {
                    Line::from(Span::styled(
                        format_shortcut_hint("ctrl+o", "expand", false, false).plain_text,
                        theme.dim,
                    ))
                }))
                .chain(
                    detail_lines
                        .iter()
                        .map(|line| Line::from(Span::styled(line.clone(), theme.dim))),
                )
                .chain(
                    prevented_line
                        .as_ref()
                        .map(|line| Line::from(Span::styled(line.clone(), theme.warning))),
                )
                .chain(
                    error_lines
                        .iter()
                        .map(|line| Line::from(Span::styled(line.clone(), theme.error))),
                )
                .collect::<Vec<_>>(),
            ),
        },
    }
}

fn render_teammate_mailbox_item(
    item: &AttachmentTeammateMailboxItemDisplay,
    theme: &MessagesRenderTheme,
) -> Vec<Line<'static>> {
    match item {
        AttachmentTeammateMailboxItemDisplay::TaskAssignment {
            dot_glyph,
            task_id,
            subject,
            from,
        } => {
            let mut text = format!("{dot_glyph} Task assigned: #{task_id}");
            if let Some(subject) = subject.as_ref().filter(|subject| !subject.is_empty()) {
                text.push_str(" - ");
                text.push_str(subject);
            }
            text.push_str(&format!(" (from {from})"));
            vec![Line::from(Span::styled(text, theme.text))]
        }
        AttachmentTeammateMailboxItemDisplay::PlanApproval(renderable) => {
            render_plan_approval_renderable(renderable, theme)
        }
        AttachmentTeammateMailboxItemDisplay::Plain(plain) => {
            render_teammate_plain_content(plain, theme)
        }
    }
}

fn render_plan_approval_renderable(
    renderable: &PlanApprovalRenderable,
    theme: &MessagesRenderTheme,
) -> Vec<Line<'static>> {
    match renderable {
        PlanApprovalRenderable::Request(request) => std::iter::once(Line::from(Span::styled(
            request.title.clone(),
            theme.accent,
        )))
        .chain(
            request
                .plan_content
                .lines()
                .map(|line| Line::from(Span::styled(line.to_owned(), theme.text))),
        )
        .chain(std::iter::once(Line::from(Span::styled(
            format!("Plan file: {}", request.plan_file_path),
            theme.dim,
        ))))
        .collect(),
        PlanApprovalRenderable::Response(response) => {
            render_plan_approval_response(response, theme)
        }
    }
}

fn render_plan_approval_response(
    response: &PlanApprovalResponseDisplay,
    theme: &MessagesRenderTheme,
) -> Vec<Line<'static>> {
    match response {
        PlanApprovalResponseDisplay::Approved { title, body } => vec![
            Line::from(Span::styled(title.clone(), theme.accent)),
            Line::from(Span::styled(*body, theme.text)),
        ],
        PlanApprovalResponseDisplay::Rejected {
            title,
            feedback,
            footer,
        } => std::iter::once(Line::from(Span::styled(title.clone(), theme.error)))
            .chain(feedback.as_ref().map(|feedback| {
                Line::from(Span::styled(format!("Feedback: {feedback}"), theme.text))
            }))
            .chain(std::iter::once(Line::from(Span::styled(
                *footer, theme.dim,
            ))))
            .collect(),
    }
}

fn render_teammate_plain_content(
    plain: &TeammateMessageContentDisplay,
    theme: &MessagesRenderTheme,
) -> Vec<Line<'static>> {
    let name_style = if plain.color.is_some() {
        theme.accent
    } else {
        theme.text
    };
    let header = Line::from(
        std::iter::once(Span::styled(
            format!("@{}\u{276f}", plain.display_name),
            name_style,
        ))
        .chain(
            plain
                .summary
                .as_ref()
                .map(|summary| Span::raw(format!(" {summary}"))),
        )
        .collect::<Vec<_>>(),
    );
    std::iter::once(header)
        .chain(
            plain
                .is_transcript_mode
                .then_some(plain.content.lines())
                .into_iter()
                .flatten()
                .map(|line| Line::from(Span::styled(format!("  {line}"), theme.text))),
        )
        .collect()
}

fn render_user_teammate_plain_content(
    plain: &TeammateMessageContentDisplay,
    theme: &MessagesRenderTheme,
) -> Vec<Line<'static>> {
    let name_style = if plain.color.is_some() {
        theme.accent
    } else {
        theme.text
    };
    let header = Line::from(vec![
        Span::styled("Message from ", theme.dim),
        Span::styled(format!("@{}", plain.display_name), name_style),
    ]);
    std::iter::once(header)
        .chain(std::iter::once(Line::default()))
        .chain(
            plain
                .content
                .lines()
                .map(|line| Line::from(Span::styled(line.to_owned(), theme.text))),
        )
        .collect()
}

/// Render the `TeammateRenderable` list
/// [`rebon_render::teammate_messages::project_user_teammate_messages`]
/// produces: one idle notification, plain message, or plan-approval
/// summary per entry.
fn render_teammate_renderables(
    renderables: &[TeammateRenderable],
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    let lines: Vec<Line<'static>> = renderables
        .iter()
        .flat_map(|renderable| match renderable {
            TeammateRenderable::IdleNotification {
                display_name,
                status_text,
            } => vec![Line::from(vec![
                Span::styled("Teammate ", theme.dim),
                Span::styled(format!("@{display_name}"), theme.accent),
                Span::styled(format!(" {status_text}"), theme.dim),
            ])],
            TeammateRenderable::Plain(plain) => render_user_teammate_plain_content(plain, theme),
            TeammateRenderable::PlanApprovalSummary(summary) => {
                vec![Line::from(Span::styled(summary.clone(), theme.accent))]
            }
            TeammateRenderable::ShutdownSummary(summary) => {
                vec![Line::from(Span::styled(summary.clone(), theme.dim))]
            }
            TeammateRenderable::TaskAssignmentSummary(summary) => {
                vec![Line::from(Span::styled(summary.clone(), theme.text))]
            }
            TeammateRenderable::TaskCompleted { display_name, .. } => vec![Line::from(vec![
                Span::styled("Teammate ", theme.dim),
                Span::styled(format!("@{display_name}"), theme.accent),
                Span::styled(" finished", theme.dim),
            ])],
        })
        .collect();
    Text::from(lines)
}

fn render_generic_system_display(
    display: &SystemGenericTextDisplay,
    theme: &MessagesRenderTheme,
) -> Line<'static> {
    render_system_line(
        display.marker,
        display.content.clone(),
        match display.color.as_deref() {
            Some("warning") => theme.warning,
            Some("error") => theme.error,
            _ if display.dim_color => theme.dim,
            _ => theme.text,
        },
    )
}

fn render_system_line(
    _marker: Option<SystemVisualMarker>,
    text: String,
    style: Style,
) -> Line<'static> {
    Line::from(Span::styled(text, style))
}

/// Render `AssistantTextMessageProjection`.
pub fn render_assistant_text_projection(
    projection: &AssistantTextMessageProjection,
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    match projection {
        AssistantTextMessageProjection::Hidden => Text::default(),
        AssistantTextMessageProjection::RateLimit(rate_limit) => Text::from(
            std::iter::once(Line::from(Span::styled(
                rate_limit.text.clone(),
                theme.warning,
            )))
            .chain(
                rate_limit
                    .upsell_message
                    .as_ref()
                    .map(|hint| Line::from(Span::styled(hint.clone(), theme.dim))),
            )
            .collect::<Vec<_>>(),
        ),
        AssistantTextMessageProjection::Response(display) => Text::from(
            display
                .blocks
                .iter()
                .map(|block| render_assistant_response_block(block, theme))
                .collect::<Vec<_>>(),
        ),
        AssistantTextMessageProjection::Markdown(display) => Text::from(
            display
                .markdown
                .lines()
                .map(|line| Line::from(Span::styled(line.to_owned(), theme.text)))
                .collect::<Vec<_>>(),
        ),
    }
}

/// Render `AssistantThinkingMessageProjection`.
pub fn render_assistant_thinking_projection(
    projection: &AssistantThinkingMessageProjection,
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    match projection {
        AssistantThinkingMessageProjection::Hidden => Text::default(),
        AssistantThinkingMessageProjection::Collapsed(display) => {
            let mut lines = vec![Line::from(Span::styled(
                display.markdown.clone(),
                theme.dim,
            ))];
            if display.show_expand_hint {
                lines.push(Line::from(Span::styled(
                    format_shortcut_hint("ctrl+o", "expand", false, false).plain_text,
                    theme.dim,
                )));
            }
            Text::from(lines)
        }
        AssistantThinkingMessageProjection::Expanded(display) => {
            let mut lines = Vec::new();
            if display.show_label {
                lines.push(Line::from(Span::styled(display.label, theme.dim)));
            }
            lines.extend(
                display
                    .markdown
                    .lines()
                    .map(|line| Line::from(Span::styled(line.to_owned(), theme.dim))),
            );
            if display.show_expand_hint {
                lines.push(Line::from(Span::styled(
                    format_shortcut_hint("ctrl+o", "expand", false, false).plain_text,
                    theme.dim,
                )));
            }
            Text::from(lines)
        }
    }
}

/// Render `HighlightedThinkingTextProjection`.
pub fn render_highlighted_thinking_projection(
    projection: &HighlightedThinkingTextProjection,
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    match projection {
        HighlightedThinkingTextProjection::Brief(display) => Text::from(vec![
            Line::from(vec![
                Span::styled(
                    display.label,
                    map_thinking_color(display.label_color, theme),
                ),
                Span::raw(
                    display
                        .timestamp
                        .as_ref()
                        .map(|timestamp| format!(" {timestamp}"))
                        .unwrap_or_default(),
                ),
            ]),
            Line::from(Span::styled(
                display.text.clone(),
                map_thinking_color(display.text_color, theme),
            )),
        ]),
        HighlightedThinkingTextProjection::Inline(display) => Text::from(Line::from(
            std::iter::once(Span::styled(
                display.pointer.glyph,
                map_thinking_color(display.pointer.color, theme),
            ))
            .chain(display.segments.iter().map(|segment| {
                Span::styled(
                    segment.text.clone(),
                    map_thinking_color(segment.color, theme),
                )
            }))
            .collect::<Vec<_>>(),
        )),
    }
}

/// Render `UserToolResultProjection`.
pub fn render_user_tool_result_projection(
    projection: &UserToolResultProjection,
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    match projection {
        UserToolResultProjection::MissingToolUse => Text::from(Line::from(Span::styled(
            "[missing tool use]",
            theme.warning,
        ))),
        UserToolResultProjection::Canceled => {
            Text::from(Line::from(Span::styled("[tool canceled]", theme.warning)))
        }
        UserToolResultProjection::RejectedPlan { plan } => Text::from(vec![
            Line::from(Span::styled("[rejected plan]", theme.error)),
            Line::from(Span::raw(plan.clone())),
        ]),
        UserToolResultProjection::RejectedToolUse => {
            Text::from(Line::from(Span::styled("[rejected tool use]", theme.error)))
        }
        UserToolResultProjection::Error(error) => render_user_tool_error_projection(error, theme),
        UserToolResultProjection::Success(success) => Text::from(
            std::iter::once(Line::from(Span::styled(
                format!("[tool result {}]", success.tool_use_id),
                theme.accent,
            )))
            .chain(
                success
                    .classifier_rule
                    .as_ref()
                    .map(|line| Line::from(Span::styled(line.clone(), theme.dim))),
            )
            .chain(
                success
                    .yolo_reason
                    .as_ref()
                    .map(|line| Line::from(Span::styled(line.clone(), theme.warning))),
            )
            .collect::<Vec<_>>(),
        ),
    }
}

/// Render `AssistantToolUseProjection`.
pub fn render_assistant_tool_use_projection(
    projection: &AssistantToolUseProjection,
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    match projection {
        AssistantToolUseProjection::Hidden(reason) => Text::from(Line::from(Span::styled(
            format!("{reason:?}"),
            theme.warning,
        ))),
        AssistantToolUseProjection::Transparent(progress) => Text::from(
            std::iter::empty()
                .chain(
                    progress
                        .hook_progress_message
                        .as_ref()
                        .map(|line| Line::from(Span::styled(line.clone(), theme.dim))),
                )
                .chain(
                    progress
                        .progress_message
                        .as_ref()
                        .map(|line| Line::from(Span::styled(line.clone(), theme.dim))),
                )
                .collect::<Vec<_>>(),
        ),
        AssistantToolUseProjection::Row(display) => {
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    display
                        .header
                        .leading
                        .as_ref()
                        .map(render_tool_leading)
                        .unwrap_or_default(),
                    theme.accent,
                ),
                Span::styled(
                    display.header.user_facing_name.clone(),
                    tool_name_style(theme),
                ),
                Span::styled(
                    display
                        .header
                        .rendered_message
                        .as_ref()
                        .map(|msg| format!(" ({msg})"))
                        .unwrap_or_default(),
                    theme.dim,
                ),
                Span::raw(
                    display
                        .header
                        .tag
                        .as_ref()
                        .map(|tag| format!(" [{tag}]"))
                        .unwrap_or_default(),
                ),
            ])];
            if let Some(progress) = display.progress.as_ref() {
                lines.extend(render_tool_secondary(progress, theme).lines);
            }
            if let Some(queued) = display.queued_message.as_ref() {
                lines.push(Line::from(Span::styled(queued.clone(), theme.dim)));
            }
            Text::from(lines)
        }
    }
}

fn render_assistant_response_block(
    block: &AssistantResponseBlock,
    theme: &MessagesRenderTheme,
) -> Line<'static> {
    match block {
        AssistantResponseBlock::ErrorLine(line) => {
            Line::from(Span::styled(line.clone(), theme.error))
        }
        AssistantResponseBlock::TextLine(line) => {
            Line::from(Span::styled(line.clone(), theme.text))
        }
        AssistantResponseBlock::DimTextLine(line) => {
            Line::from(Span::styled(line.clone(), theme.dim))
        }
        AssistantResponseBlock::InterruptedByUser => {
            Line::from(Span::styled("[interrupted by user]", theme.warning))
        }
        AssistantResponseBlock::ExpandHint => Line::from(Span::styled(
            format_shortcut_hint("ctrl+o", "expand", false, false).plain_text,
            theme.dim,
        )),
    }
}

fn render_user_tool_error_projection(
    projection: &UserToolErrorProjection,
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    match projection {
        UserToolErrorProjection::Interrupted => Text::from(Line::from(Span::styled(
            "[interrupted by user]",
            theme.warning,
        ))),
        UserToolErrorProjection::RejectedPlan { plan } => Text::from(vec![
            Line::from(Span::styled("[rejected plan]", theme.error)),
            Line::from(Span::raw(plan.clone())),
        ]),
        UserToolErrorProjection::RejectedToolUse => {
            Text::from(Line::from(Span::styled("[rejected tool use]", theme.error)))
        }
        UserToolErrorProjection::ClassifierDenied => Text::from(Line::from(Span::styled(
            "[classifier denied]",
            theme.warning,
        ))),
        UserToolErrorProjection::Fallback { result, .. }
        | UserToolErrorProjection::Custom { result, .. } => Text::from(
            result
                .lines()
                .map(|line| Line::from(Span::styled(line.to_owned(), theme.error)))
                .collect::<Vec<_>>(),
        ),
    }
}

fn map_thinking_color(color: ThinkingThemeColor, theme: &MessagesRenderTheme) -> Style {
    let t = theme::get_active_theme();
    match color {
        ThinkingThemeColor::Suggestion | ThinkingThemeColor::BriefLabelYou => theme.accent,
        ThinkingThemeColor::Subtle => theme.dim,
        ThinkingThemeColor::Text => theme.text,
        ThinkingThemeColor::RainbowRed => Style::new().fg(parse_theme_color(t.rainbow_red)),
        ThinkingThemeColor::RainbowRedShimmer => {
            Style::new().fg(parse_theme_color(t.rainbow_red_shimmer))
        }
        ThinkingThemeColor::RainbowOrange => Style::new().fg(parse_theme_color(t.rainbow_orange)),
        ThinkingThemeColor::RainbowOrangeShimmer => {
            Style::new().fg(parse_theme_color(t.rainbow_orange_shimmer))
        }
        ThinkingThemeColor::RainbowYellow => Style::new().fg(parse_theme_color(t.rainbow_yellow)),
        ThinkingThemeColor::RainbowYellowShimmer => {
            Style::new().fg(parse_theme_color(t.rainbow_yellow_shimmer))
        }
        ThinkingThemeColor::RainbowGreen => Style::new().fg(parse_theme_color(t.rainbow_green)),
        ThinkingThemeColor::RainbowGreenShimmer => {
            Style::new().fg(parse_theme_color(t.rainbow_green_shimmer))
        }
        ThinkingThemeColor::RainbowBlue => Style::new().fg(parse_theme_color(t.rainbow_blue)),
        ThinkingThemeColor::RainbowBlueShimmer => {
            Style::new().fg(parse_theme_color(t.rainbow_blue_shimmer))
        }
        ThinkingThemeColor::RainbowIndigo => Style::new().fg(parse_theme_color(t.rainbow_indigo)),
        ThinkingThemeColor::RainbowIndigoShimmer => {
            Style::new().fg(parse_theme_color(t.rainbow_indigo_shimmer))
        }
        ThinkingThemeColor::RainbowViolet => Style::new().fg(parse_theme_color(t.rainbow_violet)),
        ThinkingThemeColor::RainbowVioletShimmer => {
            Style::new().fg(parse_theme_color(t.rainbow_violet_shimmer))
        }
    }
}

/// Parse a design-system color string (`"rgb(r,g,b)"`, `"#hex"`,
/// `"ansi256(n)"`, `"ansi:name"`) into a ratatui `Color`.
/// Falls back to `Color::Reset` on parse failure.
pub fn parse_theme_color(s: &str) -> Color {
    if let Some(inner) = s.strip_prefix("rgb(").and_then(|r| r.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() == 3 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                parts[0].trim().parse::<u8>(),
                parts[1].trim().parse::<u8>(),
                parts[2].trim().parse::<u8>(),
            ) {
                return Color::Rgb(r, g, b);
            }
        }
    }
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[0..2], 16),
                u8::from_str_radix(&hex[2..4], 16),
                u8::from_str_radix(&hex[4..6], 16),
            ) {
                return Color::Rgb(r, g, b);
            }
        }
    }
    if let Some(inner) = s.strip_prefix("ansi256(").and_then(|r| r.strip_suffix(')')) {
        if let Ok(n) = inner.trim().parse::<u8>() {
            return Color::Indexed(n);
        }
    }
    if let Some(name) = s.strip_prefix("ansi:") {
        return match name {
            "black" => Color::Black,
            "red" => Color::Red,
            "green" => Color::Green,
            "yellow" => Color::Yellow,
            "blue" => Color::Blue,
            "magenta" => Color::Magenta,
            "cyan" => Color::Cyan,
            "white" => Color::White,
            "blackBright" | "gray" | "grey" => Color::DarkGray,
            "redBright" => Color::LightRed,
            "greenBright" => Color::LightGreen,
            "yellowBright" => Color::LightYellow,
            "blueBright" => Color::LightBlue,
            "magentaBright" => Color::LightMagenta,
            "cyanBright" => Color::LightCyan,
            "whiteBright" => Color::White,
            _ => Color::Reset,
        };
    }
    Color::Reset
}

fn render_tool_leading(leading: &AssistantToolLeadingDisplay) -> String {
    match leading {
        AssistantToolLeadingDisplay::QueuedDot { glyph, .. } => format!("{glyph} "),
        AssistantToolLeadingDisplay::Loader {
            is_unresolved,
            is_error,
            should_animate,
        } => format!(
            "{} ",
            if *is_error {
                "ERR"
            } else if *is_unresolved && *should_animate {
                "..."
            } else if *is_unresolved {
                ".."
            } else {
                "OK"
            }
        ),
    }
}

fn render_tool_secondary(
    display: &AssistantToolSecondaryDisplay,
    theme: &MessagesRenderTheme,
) -> Text<'static> {
    match display {
        AssistantToolSecondaryDisplay::WaitingForPermission { message, .. } => {
            Text::from(Line::from(Span::styled(*message, theme.warning)))
        }
        AssistantToolSecondaryDisplay::Progress(display) => Text::from(
            std::iter::empty()
                .chain(
                    display
                        .hook_progress_message
                        .as_ref()
                        .map(|line| Line::from(Span::styled(line.clone(), theme.dim))),
                )
                .chain(
                    display
                        .progress_message
                        .as_ref()
                        .map(|line| Line::from(Span::styled(line.clone(), theme.dim))),
                )
                .collect::<Vec<_>>(),
        ),
    }
}

#[cfg(test)]
mod render_attachment_system_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_render::{
        assistant_text::{AssistantMarkdownDisplay, AssistantResponseDisplay},
        simple_messages::AgentNotificationDisplay,
        thinking::{
            AssistantThinkingExpandedDisplay, BriefThinkingDisplay, InlinePointerDisplay,
            InlineThinkingDisplay, ThinkingTextSegment,
        },
        tool_results::UserToolSuccessProjection,
    };

    fn lines(text: Text<'static>) -> Vec<String> {
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
    fn fold_separator_gray_contrasts_with_light_and_dark_theme_backgrounds() {
        for theme_name in [
            theme::ThemeName::Light,
            theme::ThemeName::LightDaltonized,
            theme::ThemeName::LightAnsi,
        ] {
            assert_eq!(fold_separator_color(theme_name), Color::DarkGray);
        }

        for theme_name in [
            theme::ThemeName::Dark,
            theme::ThemeName::DarkDaltonized,
            theme::ThemeName::DarkAnsi,
        ] {
            assert_eq!(fold_separator_color(theme_name), Color::Gray);
        }
    }

    #[test]
    fn assistant_tool_use_renderer_bolds_tool_name_and_dims_summary() {
        let detail_color = Color::Rgb(96, 96, 96);
        let theme = MessagesRenderTheme {
            dim: Style::new().fg(detail_color),
            ..MessagesRenderTheme::plain()
        };
        let text = render_assistant_tool_use_projection(
            &AssistantToolUseProjection::Row(
                rebon_render::assistant_tool_use::AssistantToolUseRowDisplay {
                    margin_top: 0,
                    background: None,
                    header: rebon_render::assistant_tool_use::AssistantToolHeaderDisplay {
                        min_width: 4,
                        leading: None,
                        user_facing_name: "Bash".into(),
                        user_facing_name_background_color: None,
                        inverse_text: false,
                        rendered_message: Some("cargo test".into()),
                        tag: None,
                    },
                    progress: None,
                    queued_message: None,
                },
            ),
            &theme,
        );

        let spans = &text.lines[0].spans;
        let name = spans
            .iter()
            .find(|span| span.content.as_ref() == "Bash")
            .expect("tool name span");
        let summary = spans
            .iter()
            .find(|span| span.content.as_ref() == " (cargo test)")
            .expect("summary span");
        assert!(name.style.add_modifier.contains(Modifier::BOLD));
        assert_ne!(name.style.fg, Some(detail_color));
        assert_eq!(summary.style.fg, Some(detail_color));
        assert!(!summary.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn teammate_plain_renderer_uses_message_header_and_body() {
        let rendered = render_user_text_projection(
            &UserTextProjection::Teammate(vec![TeammateRenderable::Plain(
                TeammateMessageContentDisplay {
                    display_name: "fix-d1-transport".into(),
                    color: Some("blue".into()),
                    content: "D1 complete\nHandoff ready.".into(),
                    summary: Some("ignored summary".into()),
                    is_transcript_mode: false,
                },
            )]),
            &MessagesRenderTheme::plain(),
        );

        assert_eq!(
            lines(rendered),
            vec![
                "Message from @fix-d1-transport",
                "",
                "D1 complete",
                "Handoff ready.",
            ]
        );
    }

    #[test]
    fn teammate_idle_notification_renderer_uses_finished_summary() {
        let rendered = render_user_text_projection(
            &UserTextProjection::Teammate(vec![TeammateRenderable::IdleNotification {
                display_name: "epub-native-audit".into(),
                status_text: "finished".into(),
            }]),
            &MessagesRenderTheme::plain(),
        );

        assert_eq!(
            lines(rendered),
            vec!["Teammate @epub-native-audit finished"]
        );
    }

    #[test]
    fn teammate_task_completion_renderer_uses_finished_summary() {
        let rendered = render_user_text_projection(
            &UserTextProjection::Teammate(vec![TeammateRenderable::TaskCompleted {
                display_name: "fix-d1-transport".into(),
                task_id: "7".into(),
                task_subject: Some("Fix transport".into()),
            }]),
            &MessagesRenderTheme::plain(),
        );

        assert_eq!(lines(rendered), vec!["Teammate @fix-d1-transport finished"]);
    }

    #[test]
    fn compact_user_assistant_and_tool_result_renderers_emit_text() {
        let compact = render_compact_summary_display(
            &CompactSummaryDisplay {
                user_prompt: None,
                title: "Compact summary".into(),
                metadata_line: Some("meta".into()),
                context_line: None,
                shortcut_hint: Some("(ctrl+o)".into()),
                transcript_text: Some("body".into()),
                file_entries: Vec::new(),
            },
            &MessagesRenderTheme::plain(),
        );
        assert!(lines(compact).iter().any(|line| line.contains("body")));

        let user = render_user_text_projection(
            &UserTextProjection::Prompt {
                add_margin: false,
                text: "hello".into(),
                verbose: false,
                is_transcript_mode: false,
                timestamp: None,
            },
            &MessagesRenderTheme::plain(),
        );
        assert_eq!(lines(user), vec!["hello"]);

        let notification = render_user_text_projection(
            &UserTextProjection::TaskNotification(AgentNotificationDisplay {
                summary: "Agent \"研究临时 fixture\" failed".into(),
                color: StatusColor::Error,
                margin_top: 1,
            }),
            &MessagesRenderTheme::plain(),
        );
        assert_eq!(
            lines(notification),
            vec!["Agent \"研究临时 fixture\" failed"]
        );

        let assistant = render_assistant_text_projection(
            &AssistantTextMessageProjection::Markdown(AssistantMarkdownDisplay {
                margin_top: 0,
                background: None,
                dot: None,
                markdown: "answer".into(),
            }),
            &MessagesRenderTheme::plain(),
        );
        assert_eq!(lines(assistant), vec!["answer"]);

        let tool_result = render_user_tool_result_projection(
            &UserToolResultProjection::Success(UserToolSuccessProjection {
                tool_use_id: "toolu-1".into(),
                verbose: false,
                is_transcript_mode: false,
                width: "100".into(),
                classifier_rule: Some("rule".into()),
                yolo_reason: None,
                renders_as_assistant_text: false,
            }),
            &MessagesRenderTheme::plain(),
        );
        assert!(lines(tool_result)
            .iter()
            .any(|line| line.contains("toolu-1")));
    }

    #[test]
    fn response_and_thinking_renderers_emit_expand_hints_and_text() {
        let response = render_assistant_text_projection(
            &AssistantTextMessageProjection::Response(AssistantResponseDisplay {
                height: Some(1),
                blocks: vec![
                    AssistantResponseBlock::ErrorLine("boom".into()),
                    AssistantResponseBlock::ExpandHint,
                ],
            }),
            &MessagesRenderTheme::plain(),
        );
        let rendered = lines(response);
        assert!(rendered.iter().any(|line| line.contains("boom")));
        assert!(rendered.iter().any(|line| line.contains("Ctrl+O")));

        let thinking = render_assistant_thinking_projection(
            &AssistantThinkingMessageProjection::Expanded(AssistantThinkingExpandedDisplay {
                margin_top: 0,
                label: "∴ Thinking…",
                markdown: "step 1".into(),
                padding_left: 2,
                gap: 1,
                width: "100%",
                show_label: false,
                show_expand_hint: false,
            }),
            &MessagesRenderTheme::plain(),
        );
        let rendered = lines(thinking);
        assert!(!rendered.iter().any(|line| line.contains("Thinking")));
        assert!(rendered.iter().any(|line| line.contains("step 1")));
    }

    #[test]
    fn highlighted_thinking_uses_brief_and_inline_shapes() {
        let brief = render_highlighted_thinking_projection(
            &HighlightedThinkingTextProjection::Brief(BriefThinkingDisplay {
                label: "You",
                label_color: ThinkingThemeColor::BriefLabelYou,
                timestamp: Some("1:23 PM".into()),
                text: "thinking".into(),
                text_color: ThinkingThemeColor::Text,
                padding_left: 2,
            }),
            &MessagesRenderTheme::plain(),
        );
        assert!(lines(brief).iter().any(|line| line.contains("You 1:23 PM")));

        let inline = render_highlighted_thinking_projection(
            &HighlightedThinkingTextProjection::Inline(InlineThinkingDisplay {
                pointer: InlinePointerDisplay {
                    glyph: "❯",
                    color: ThinkingThemeColor::Suggestion,
                },
                segments: vec![ThinkingTextSegment {
                    text: "abc".into(),
                    color: ThinkingThemeColor::Text,
                }],
            }),
            &MessagesRenderTheme::plain(),
        );
        assert_eq!(lines(inline), vec!["❯abc"]);
    }
}
