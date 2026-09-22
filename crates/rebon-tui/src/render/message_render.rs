use super::*;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Top-level dispatch — uses rebon-message-tui RenderedMessageWidget
// ---------------------------------------------------------------------------

/// Render a single message into `area`. Returns the number of
/// vertical lines actually consumed.
///
/// Delegates to [`RenderedMessageWidget`] from `rebon-message-tui` for
/// all committed message types. The widget handles:
/// - Proper gutter labels and indentation
/// - Inter-block spacing via `add_margin`
/// - Typed body widgets for assistant text, tool use, thinking, etc.
/// - User text classification (bash output, tick, etc.)
pub fn render_message(
    msg: &Message,
    area: Rect,
    buf: &mut Buffer,
    _theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
) -> u16 {
    render_message_inner(msg, area, buf, _theme, verbosity, true)
}

pub(super) fn render_message_inner(
    msg: &Message,
    area: Rect,
    buf: &mut Buffer,
    _theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
) -> u16 {
    render_message_inner_with_context(
        msg,
        area,
        buf,
        _theme,
        verbosity,
        add_margin,
        None,
        false,
        TranscriptRenderExtras::empty(),
    )
}

pub(super) fn render_generated_image_message(
    gi: &crate::message::AssistantGeneratedImageBlock,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    add_margin: bool,
) -> u16 {
    if area.height == 0 || area.width == 0 {
        return 0;
    }
    let mut consumed = 0u16;
    if add_margin {
        consumed = consumed.saturating_add(1.min(area.height));
    }
    if consumed >= area.height {
        return consumed;
    }

    let sub = Rect {
        x: area.x,
        y: area.y.saturating_add(consumed),
        width: area.width,
        height: area.height.saturating_sub(consumed),
    };
    let media_type = if gi.media_type.is_empty() {
        "image/png"
    } else {
        gi.media_type.as_str()
    };
    let header = format!("Generated image ({media_type})");
    let mut lines = vec![Line::from(Span::styled(header.clone(), theme.streaming))];
    let mut text_lines = vec![header];
    if let Some(path) = gi.saved_path.as_ref() {
        let line = generated_image_saved_line(path, theme.supports_hyperlinks);
        lines.push(Line::from(Span::styled(line.clone(), theme.streaming)));
        text_lines.push(line);
    }
    if let Some(prompt) = gi
        .revised_prompt
        .as_ref()
        .filter(|prompt| !prompt.trim().is_empty())
    {
        let line = format!("Prompt: {prompt}");
        lines.push(Line::from(Span::styled(line.clone(), theme.streaming)));
        text_lines.push(line);
    }

    let body_height = wrap_height_visible(
        &text_lines.join("\n"),
        sub.width.saturating_sub(GUTTER).max(1),
    )
    .max(1)
    .min(sub.height);
    let render_lines = if theme.supports_hyperlinks {
        strip_osc8_from_lines(&lines)
    } else {
        lines.clone()
    };
    consumed = consumed.saturating_add(render_gutter_lines(
        render_lines,
        &text_lines.join("\n"),
        "●",
        theme.assistant_prefix,
        sub,
        buf,
    ));
    if theme.supports_hyperlinks {
        apply_line_hyperlinks(
            &lines,
            Rect {
                x: sub.x.saturating_add(GUTTER),
                y: sub.y,
                width: sub.width.saturating_sub(GUTTER).max(1),
                height: body_height,
            },
            buf,
        );
    }
    consumed
}

pub(super) fn render_message_inner_with_context(
    msg: &Message,
    area: Rect,
    buf: &mut Buffer,
    _theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    last_thinking_block_id: Option<&str>,
    is_transcript_mode: bool,
    extras: TranscriptRenderExtras<'_>,
) -> u16 {
    if area.height == 0 || area.width == 0 {
        return 0;
    }
    if let Some(height) = render_committed_assistant_tool_step(
        msg,
        area,
        buf,
        _theme,
        verbosity,
        add_margin,
        last_thinking_block_id,
        extras,
    ) {
        return height;
    }
    if let Message::Assistant(assistant) = msg {
        if assistant.message.content.len() == 1 {
            if let AssistantContentBlock::GeneratedImage(gi) = &assistant.message.content[0] {
                return render_generated_image_message(gi, area, buf, _theme, add_margin);
            }
        }
    }
    let mut rm_theme = MessageRenderTheme::default();
    if _theme.math_display.math_enabled() {
        let palette = rebon_design_system::theme::get_theme(_theme.name);
        rm_theme.assistant_text = Style::new().fg(parse_theme_color(palette.text));
    }
    if let Some(input) = to_render_input_with_tool_state(
        msg,
        area.width,
        add_margin,
        verbosity,
        0,
        Vec::new(),
        Vec::new(),
        last_thinking_block_id,
        is_transcript_mode,
        extras,
    ) {
        let widget = RenderedMessageWidget::new_with_markdown_options(
            &input,
            &rm_theme,
            area.width,
            None,
            None,
            markdown_render_options(_theme.math_display),
        );
        let h = widget.height(area.width).min(area.height);
        if h == 0 {
            return 0;
        }
        let sub = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: h,
        };
        let layers = widget.render_to_buffer_with_hyperlinks(sub, buf);
        if _theme.supports_hyperlinks {
            apply_markdown_hyperlink_layers(&layers, buf);
        }
        apply_math_image_layers(&layers, _theme, buf);
        h
    } else {
        // Fallback for unknown rows.
        let label = match msg {
            Message::Attachment(_) => "[attachment]",
            _ => "[unknown message type]",
        };
        if area.height > 0 {
            let line = Line::from(Span::raw(label.to_string()));
            Paragraph::new(line).render(
                Rect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
            1
        } else {
            0
        }
    }
}

fn collect_plan_ledger_body_lines(
    tu: &AssistantToolUseBlock,
    verbosity: ToolOutputVerbosity,
) -> Vec<String> {
    let input = plan_ledger_input(&tu.name, &tu.input);
    let fallback_result = plan_ledger_result_from_content(tu.tool_call_content.as_deref());
    let output = tu.raw_output.as_ref().or(fallback_result.as_ref());
    let operation = input
        .and_then(|input| input.get("operation"))
        .and_then(Value::as_str);

    if tu.status == Some(ToolCallStatus::Failed) {
        let mut lines = tu
            .tool_call_content
            .as_deref()
            .into_iter()
            .flatten()
            .flat_map(|item| plan_ledger_error_lines(&render_tool_call_content(item)))
            .collect::<Vec<_>>();
        if lines.is_empty() {
            for key in ["error", "message", "reason", "detail", "details"] {
                if let Some(value) = output.and_then(|output| output.get(key)) {
                    lines.extend(plan_ledger_error_lines(&value.to_string()));
                }
            }
        }
        if lines.is_empty() {
            lines.push("PlanLedger failed".to_string());
        }
        return lines;
    }

    let requirement_source = match operation {
        Some("set_requirements" | "add") => input.and_then(|input| input.get("items")),
        Some("list") => output.and_then(|output| output.get("requirements")),
        _ => None,
    };
    let mut lines = plan_ledger_requirement_lines(requirement_source, verbosity);
    if let Some(status) = plan_ledger_status_line(
        operation,
        output.and_then(|output| output.get("round")),
        output.and_then(|output| output.get("interviewRevision")),
        output.and_then(|output| output.get("understandingSeal")),
        output.is_some(),
    ) {
        lines.push(status);
    }
    lines
}

#[cfg(test)]
pub(super) fn collect_tool_block_body_lines(
    tu: &AssistantToolUseBlock,
    verbosity: ToolOutputVerbosity,
) -> Vec<String> {
    collect_tool_block_body_lines_with_width(tu, verbosity, u16::MAX)
}

pub(super) fn collect_tool_block_body_lines_with_width(
    tu: &AssistantToolUseBlock,
    verbosity: ToolOutputVerbosity,
    content_width: u16,
) -> Vec<String> {
    if plan_ledger_input(&tu.name, &tu.input).is_some() {
        return collect_plan_ledger_body_lines(tu, verbosity);
    }
    if tu.name == rebon_render::code_mode::RUN_CODE_TOOL_NAME {
        return collect_code_mode_body_lines(tu, verbosity, content_width);
    }
    if tu.name == "Sleep" {
        return Vec::new();
    }
    if let Some(lines) = tu.raw_output.as_ref().and_then(|raw_output| {
        rebon_render::shell_output::shell_management_body_lines_from_value(&tu.name, raw_output)
    }) {
        let lines =
            normalize_tool_body_lines(lines, matches!(verbosity, ToolOutputVerbosity::Verbose));
        return tool_content_preview_lines(&lines, &tu.name, verbosity, content_width);
    }
    let raw_output_map = tu.raw_output.as_ref().and_then(Value::as_object);
    if let Some(lines) = collect_shell_tool_body_lines(
        &tu.name,
        tu.tool_call_content.as_deref(),
        raw_output_map
            .and_then(|output| output.get("stdout"))
            .and_then(Value::as_str),
        raw_output_map
            .and_then(|output| output.get("stderr"))
            .and_then(Value::as_str),
        raw_output_map
            .and_then(|output| output.get(rebon_tools_core::shell_stream_order::STREAM_ORDER_KEY))
            .and_then(Value::as_str),
        tu.status == Some(ToolCallStatus::Failed),
    ) {
        let lines =
            normalize_tool_body_lines(lines, matches!(verbosity, ToolOutputVerbosity::Verbose));
        return tool_content_preview_lines(&lines, &tu.name, verbosity, content_width);
    }
    let is_web_search = tu.name == "WebSearch"
        || (tu.name == "InvokeDeferredTool"
            && tu.input.get("tool_name").and_then(Value::as_str) == Some("WebSearch"));
    let is_web_fetch = tu.name == "WebFetch"
        || (tu.name == "InvokeDeferredTool"
            && tu.input.get("tool_name").and_then(Value::as_str) == Some("WebFetch"));
    let mut lines = Vec::new();
    if matches!(verbosity, ToolOutputVerbosity::Verbose) {
        if tu.name == "Agent" {
            let raw_input = tu.input.as_object().map(|map| {
                map.iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<HashMap<_, _>>()
            });
            let raw_output = tu.raw_output.as_ref().and_then(|value| {
                value.as_object().map(|map| {
                    map.iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<HashMap<_, _>>()
                })
            });
            let full = agent_full_detail_lines(
                raw_input.as_ref(),
                raw_output.as_ref(),
                tu.tool_call_content.as_deref(),
            );
            if !full.is_empty() {
                return full;
            }
        } else if tu.name == "InvokeDeferredTool" && !is_web_search {
            if let Some(map) = tu.input.as_object() {
                let hash: HashMap<String, serde_json::Value> =
                    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                if let Some(verbose) = deferred_tool_verbose_summary(&hash) {
                    for l in verbose.lines() {
                        lines.push(l.to_string());
                    }
                }
            }
        }
    }

    if !is_web_search {
        if let Some(title) = tu.title.as_ref() {
            let summary = tu
                .input
                .as_object()
                .map(|map| compact_json_map_value_for_tool(Some(&tu.name), map))
                .unwrap_or_default();
            if summary != *title {
                lines.push(title.clone());
            }
        }
    }

    let mut detail_lines: Vec<String> = if is_web_search {
        tu.raw_output
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|output| output.get("answer"))
            .and_then(Value::as_str)
            .map(|answer| answer.lines().map(str::to_string).collect())
            .unwrap_or_default()
    } else if is_web_fetch {
        tu.raw_output
            .as_ref()
            .and_then(|output| output.get("content"))
            .and_then(Value::as_str)
            .filter(|content| !content.is_empty())
            .map(|content| content.lines().map(str::to_string).collect())
            .or_else(|| {
                tu.tool_call_content.as_ref().map(|content| {
                    content
                        .iter()
                        .flat_map(|item| {
                            render_tool_call_content(item)
                                .split('\n')
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                        .collect()
                })
            })
            .or_else(|| {
                tu.raw_output
                    .as_ref()
                    .and_then(|output| output.get("note"))
                    .and_then(Value::as_str)
                    .map(|note| vec![format!("Note: {note}")])
            })
            .unwrap_or_default()
    } else {
        tu.tool_call_content
            .as_ref()
            .map(|content| {
                content
                    .iter()
                    .flat_map(|item| {
                        render_tool_call_content(item)
                            .split('\n')
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    if !matches!(verbosity, ToolOutputVerbosity::Normal) {
        let raw_output_is_read_image = tu
            .raw_output
            .as_ref()
            .and_then(|raw_output| raw_output.as_object())
            .map(|map| is_read_image_object_output(&tu.name, map))
            .unwrap_or(false);
        if !is_web_search && !is_web_fetch && !raw_output_is_read_image {
            if let Some(locations) = &tu.locations {
                for location in locations {
                    let s = location_label(location, false);
                    detail_lines.push(s);
                }
            }
        }
        if let Some(raw_output) = &tu.raw_output {
            if !is_web_search && !is_web_fetch {
                if let Some(map) = raw_output.as_object() {
                    let output = if raw_output_is_read_image {
                        String::new()
                    } else if is_image_generation_tool(&tu.name) {
                        let filtered: serde_json::Map<String, Value> = map
                            .iter()
                            .filter(|(key, _)| key.as_str() != "status")
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect();
                        compact_json_map_value_for_tool(Some(&tu.name), &filtered)
                    } else {
                        compact_json_map_value_for_tool(Some(&tu.name), map)
                    };
                    if !output.is_empty() {
                        detail_lines.push(output);
                    }
                }
            }
        }
    }

    let detail_lines = normalize_tool_body_lines(
        detail_lines,
        matches!(verbosity, ToolOutputVerbosity::Verbose),
    );
    lines.extend(tool_content_preview_lines(
        &detail_lines,
        if is_web_fetch { "WebFetch" } else { &tu.name },
        verbosity,
        content_width,
    ));
    normalize_tool_body_lines(lines, matches!(verbosity, ToolOutputVerbosity::Verbose))
}

/// Body lines for a committed `run_code` (Code Mode) call.
///
/// A row that also carries prose never reaches the streaming card, so this
/// mirrors that card's own rules — the shared completion summary, the final
/// output, and, in `Verbose`, the program — through the same `rebon_render`
/// helpers. Progress events are execution activity, not output: a sequence
/// that dispatched ten tools shows its summary and result rather than
/// replaying every dispatch into the transcript. A committed row has no
/// Ctrl+O expansion of its own, so the program only appears in `Verbose`.
fn collect_code_mode_body_lines(
    tu: &AssistantToolUseBlock,
    verbosity: ToolOutputVerbosity,
    content_width: u16,
) -> Vec<String> {
    let content = tu.tool_call_content.as_deref().unwrap_or_default();
    let mut lines = Vec::new();
    if tu.status == Some(ToolCallStatus::Failed) {
        if let Some(reason) = code_mode_failure_reason(tu, content) {
            lines.push(reason);
        }
    } else {
        lines.push(rebon_render::code_mode::completed_summary(content));
        let output = content
            .iter()
            .filter(|item| rebon_render::tool_output::tool_progress_metadata(item).is_none())
            .flat_map(|item| {
                render_tool_call_content(item)
                    .lines()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            });
        let mut output = normalize_tool_body_lines(output.collect(), false);
        if output.is_empty() {
            output.push("No final output received".into());
        }
        lines.extend(output);
    }
    if matches!(verbosity, ToolOutputVerbosity::Verbose) {
        if let Some(program) = rebon_render::code_mode::program_text(&tu.name, &tu.input) {
            lines.push("JavaScript:".to_string());
            lines.extend(
                rebon_render::text::expand_tabs_for_tui(&program)
                    .lines()
                    .map(str::to_string),
            );
        }
    }
    let lines = normalize_tool_body_lines(lines, matches!(verbosity, ToolOutputVerbosity::Verbose));
    tool_content_preview_lines(&lines, &tu.name, verbosity, content_width)
}

/// The one reason line a failed `run_code` shows: the error the tool reported,
/// or the last content block when the result carries no error of its own.
fn code_mode_failure_reason(
    tu: &AssistantToolUseBlock,
    content: &[ToolCallContent],
) -> Option<String> {
    let raw = tu
        .raw_output
        .as_ref()
        .and_then(|output| output.get("error"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| content.last().map(render_tool_call_content))?;
    let reason = raw.lines().find(|line| !line.trim().is_empty())?.trim();
    Some(rebon_render::text::expand_tabs_for_tui(reason))
}

/// Convert `StreamingToolUse.raw_input` (a `HashMap`) into a
/// `serde_json::Value::Object` suitable for [`classify_tool_use`].
pub(super) fn streaming_tool_input_json(tool: &crate::streaming::StreamingToolUse) -> Value {
    tool.raw_input
        .as_ref()
        .map(|m| {
            let obj: serde_json::Map<String, Value> =
                m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            Value::Object(obj)
        })
        .unwrap_or(Value::Null)
}

/// Classify a streaming tool use for collapse grouping.
pub(super) fn classify_streaming_tool(tool: &crate::streaming::StreamingToolUse) -> ToolClass {
    let policy = MemoryPathPolicy::default();
    let input = streaming_tool_input_json(tool);
    classify_tool_use(&tool.tool_name, &input, &policy, ClassifyOptions::default())
}

pub(super) fn render_collapsed_read_search_display(
    display: &CollapsedReadSearchDisplay,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    show_hint_lines: bool,
    any_failed: bool,
    any_in_progress: bool,
) -> u16 {
    let height = measure_collapsed_read_search_display_height(
        display,
        area.width,
        verbosity,
        show_hint_lines,
    );
    render_collapsed_read_search_display_bounded(
        display,
        area,
        buf,
        theme,
        verbosity,
        show_hint_lines,
        any_failed,
        any_in_progress,
        true,
    )
    .min(height)
}

pub(super) fn measure_collapsed_read_search_display_height(
    display: &CollapsedReadSearchDisplay,
    width: u16,
    verbosity: ToolOutputVerbosity,
    show_hint_lines: bool,
) -> u16 {
    if width == 0 {
        return 0;
    }

    let summary_line = collapsed_read_search_summary_line(display, verbosity);
    let content_width = width.saturating_sub(GUTTER).max(1);
    let mut consumed = wrap_height_visible(&summary_line, content_width).max(1);

    if show_hint_lines {
        for hint in display
            .hint_lines
            .iter()
            .filter(|hint| !hint.trim().is_empty())
        {
            consumed = consumed.saturating_add(wrap_height_visible(hint, content_width).max(1));
        }
    }

    consumed
}

fn collapsed_read_search_summary_line(
    display: &CollapsedReadSearchDisplay,
    verbosity: ToolOutputVerbosity,
) -> String {
    let mut summary_line = display.summary_text.clone();
    if display.show_ellipsis {
        summary_line.push('…');
    }
    if display.show_expand_hint && verbosity == ToolOutputVerbosity::Compact {
        summary_line.push_str(&format!(
            " ({})",
            format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
        ));
    }
    summary_line
}

fn render_collapsed_read_search_display_bounded(
    display: &CollapsedReadSearchDisplay,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    show_hint_lines: bool,
    any_failed: bool,
    any_in_progress: bool,
    clip_to_area: bool,
) -> u16 {
    if area.height == 0 || area.width == 0 {
        return 0;
    }

    let summary_line = collapsed_read_search_summary_line(display, verbosity);
    let header_style = if display.dim_summary {
        theme.assistant_prefix
    } else {
        theme.streaming
    };
    let gutter_style = tool_gutter_style(theme, any_failed, any_in_progress);
    let gutter_glyph = tool_gutter_glyph(theme, any_failed, any_in_progress);

    let mut consumed = render_gutter_lines(
        vec![Line::from(Span::styled(summary_line.clone(), header_style))],
        &summary_line,
        gutter_glyph,
        gutter_style,
        area,
        buf,
    );

    if show_hint_lines {
        for hint in display
            .hint_lines
            .iter()
            .filter(|hint| !hint.trim().is_empty())
        {
            if consumed >= area.height {
                break;
            }
            let sub = Rect {
                x: area.x,
                y: area.y.saturating_add(consumed),
                width: area.width,
                height: area.height.saturating_sub(consumed),
            };
            let line = Line::from(Span::styled(hint.clone(), theme.thinking));
            consumed = consumed.saturating_add(render_gutter_lines(
                vec![line],
                hint,
                "⎿",
                theme.assistant_prefix,
                sub,
                buf,
            ));
        }
    }

    if clip_to_area {
        consumed.min(area.height)
    } else {
        consumed
    }
}

/// Paint a run of `>= 2` consecutive collapsible tool uses as a single
/// summary block (e.g. `Searching for 2 patterns, reading 1 file…
/// (ctrl+o to expand)`) followed by the most recent `⎿ <hint>` line.
///
/// Shares the summary wording with the committed read/search collapse
/// ([`render_collapsed_read_search_display`]).
pub(super) fn render_collapsed_streaming_group(
    tools: &[&crate::streaming::StreamingToolUse],
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    show_hint_lines: bool,
    show_expand_hint: bool,
) -> u16 {
    if area.height == 0 || area.width == 0 || tools.is_empty() {
        return 0;
    }

    let policy = MemoryPathPolicy::default();
    let options = ClassifyOptions::default();
    let mut agg = Aggregator::new();
    let mut any_in_progress = false;
    let mut any_failed = false;

    for tool in tools {
        let input = streaming_tool_input_json(tool);
        let class = classify_tool_use(&tool.tool_name, &input, &policy, options);
        agg.push_tool_use(&tool.call_id, &class, 1);
        match tool.status {
            ToolCallStatus::Completed => {
                agg.record_result(&tool.call_id, ResultStatus::Resolved);
            }
            ToolCallStatus::Failed => {
                agg.record_result(&tool.call_id, ResultStatus::Errored);
                any_failed = true;
            }
            ToolCallStatus::InProgress | ToolCallStatus::Pending => {
                any_in_progress = true;
            }
        }
    }

    let input = agg.finalize(FinalizeParams {
        previous_counts: Default::default(),
        is_active_group: any_in_progress,
        verbose: false,
        should_animate: true,
        fullscreen_enabled: options.fullscreen,
        background: None,
    });

    let output = project_collapsed_read_search(&input);
    let mut display = match output.projection {
        CollapsedReadSearchProjection::Summary(d) => d,
        _ => return 0,
    };
    display.show_expand_hint &= show_expand_hint;

    render_collapsed_read_search_display(
        &display,
        area,
        buf,
        theme,
        verbosity,
        show_hint_lines,
        any_failed,
        any_in_progress,
    )
}
