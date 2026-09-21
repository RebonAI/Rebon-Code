use super::*;
use rebon_render::auto_mode_allowed_note;

fn is_streaming_plan_ledger(tool: &crate::streaming::StreamingToolUse) -> bool {
    tool.tool_name == PLAN_LEDGER_TOOL_NAME
        || (tool.tool_name == "InvokeDeferredTool"
            && tool
                .raw_input
                .as_ref()
                .and_then(|input| input.get("tool_name"))
                .and_then(Value::as_str)
                == Some(PLAN_LEDGER_TOOL_NAME))
}

fn strip_agent_summary_prefix(
    tool: &crate::streaming::StreamingToolUse,
    display_name: &str,
    summary: String,
) -> String {
    if tool.tool_name != "Agent" {
        return summary;
    }

    let subtype_name = streaming_tool_display_name(&tool.tool_name, tool.raw_input.as_ref());
    for label in [display_name, subtype_name] {
        if let Some(remainder) = summary
            .strip_prefix(label)
            .and_then(|remainder| remainder.strip_prefix(':'))
        {
            return remainder.trim_start().to_string();
        }
    }

    summary
}

fn streaming_plan_ledger_input_value<'a>(
    tool: &'a crate::streaming::StreamingToolUse,
    key: &str,
) -> Option<&'a Value> {
    let input = tool.raw_input.as_ref()?;
    if tool.tool_name == PLAN_LEDGER_TOOL_NAME {
        input.get(key)
    } else if is_streaming_plan_ledger(tool) {
        input
            .get("arguments")
            .and_then(|arguments| arguments.get(key))
    } else {
        None
    }
}

fn streaming_plan_ledger_fallback_result(
    tool: &crate::streaming::StreamingToolUse,
) -> Option<Value> {
    plan_ledger_result_from_content(tool.content.as_deref())
}

fn push_tool_body_line(
    body_lines: &mut Vec<Line<'static>>,
    body_text: &mut Vec<String>,
    line: String,
    style: Style,
) {
    body_lines.push(Line::from(Span::styled(line.clone(), style)));
    body_text.push(line);
}

fn plan_ledger_streaming_body_lines(
    tool: &crate::streaming::StreamingToolUse,
    verbosity: ToolOutputVerbosity,
) -> Vec<String> {
    let fallback_result = streaming_plan_ledger_fallback_result(tool);
    let output_value = |key: &str| {
        tool.raw_output
            .as_ref()
            .and_then(|output| output.get(key))
            .or_else(|| fallback_result.as_ref().and_then(|output| output.get(key)))
    };
    let operation = streaming_plan_ledger_input_value(tool, "operation").and_then(Value::as_str);

    if tool.status == ToolCallStatus::Failed {
        let mut lines = tool
            .content
            .as_deref()
            .into_iter()
            .flatten()
            .flat_map(|item| plan_ledger_error_lines(&render_tool_call_content(item)))
            .collect::<Vec<_>>();
        if lines.is_empty() {
            for key in ["error", "message", "reason", "detail", "details"] {
                if let Some(value) = output_value(key) {
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
        Some("set_requirements" | "add") => streaming_plan_ledger_input_value(tool, "items"),
        Some("list") => output_value("requirements"),
        _ => None,
    };
    let mut lines = plan_ledger_requirement_lines(requirement_source, verbosity);
    if let Some(status) = plan_ledger_status_line(
        operation,
        output_value("round"),
        output_value("interviewRevision"),
        output_value("understandingSeal"),
        tool.raw_output.is_some() || fallback_result.is_some(),
    ) {
        lines.push(status);
    }
    lines
}

pub(super) fn workflow_render_tool_name(
    tool: &crate::streaming::StreamingToolUse,
) -> Option<&'static str> {
    crate::streaming::is_workflow_tool_use(&tool.tool_name, tool.raw_input.as_ref())
        .then_some("Workflow")
}

fn workflow_streaming_tool(
    tool: &crate::streaming::StreamingToolUse,
) -> crate::streaming::StreamingToolUse {
    let mut workflow_tool = tool.clone();
    workflow_tool.tool_name = "Workflow".to_string();
    if tool.tool_name == "InvokeDeferredTool" {
        workflow_tool.raw_input = tool
            .raw_input
            .as_ref()
            .and_then(|input| input.get("arguments"))
            .and_then(Value::as_object)
            .map(|args| {
                args.iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            });
    }
    workflow_tool
}

/// Tools hidden from the transcript by the transcript visibility rules.
///
/// - `TeamCreate`: has no display name → hidden.
/// - `TeamDelete`: same pattern as TeamCreate.
/// - `SendMessage`: plain text messages render nothing; plan-approval
///   responses get custom rendering, but that is not yet implemented.
/// - `SyntheticOutput`: coordinator-internal, never user-facing.
// The hidden-tool list now lives in the shared `rebon-render` (the GPUI
// app gates on the same list). Re-exported so its readers (committed_tool.rs,
// stream_segments.rs, this file) keep one source of truth.
pub(super) use rebon_render::hidden::TRANSCRIPT_HIDDEN_TOOLS;

const EDIT_COMPACT_FULL_CONTEXT_CHANGE_LINE_LIMIT: usize = 8;

fn has_foldable_unchanged_run(diff_lines: &[String]) -> bool {
    let mut run_len = 0;
    for line in diff_lines {
        if line.starts_with(' ') {
            run_len += 1;
            if run_len >= rebon_render::USER_PROMPT_FOLD_THRESHOLD_LINES {
                return true;
            }
        } else {
            run_len = 0;
        }
    }
    false
}

fn tool_name_style(theme: &RenderTheme) -> Style {
    theme.tool_header.add_modifier(Modifier::BOLD)
}

/// Dedicated rendering for Edit tools in all lifecycle states.
///
/// ```text
/// ● Edit(path/to/file.rs)
///   ⎿  Added 4 lines
///       10  context before
///       11 +    new line 1
///       12 +    new line 2
///       13  context after
/// ```
/// Paint the "rebon's auto mode ran this for you" note as its own gutter row
/// after the tool output and metadata.
///
/// Its own row rather than part of the body block: the body is tool *output*,
/// and a permission note that sorts in with command output would read as
/// something the tool printed. Returns the rows consumed (0 when the call was
/// not auto-allowed, or the card already filled `area`).
pub(super) fn render_auto_mode_allowed_row(
    auto_mode_allowed: Option<rebon_types::AutoModeAllowSource>,
    area: Rect,
    consumed: u16,
    buf: &mut Buffer,
    theme: &RenderTheme,
) -> u16 {
    let Some(source) = auto_mode_allowed else {
        return 0;
    };
    if consumed >= area.height || area.width == 0 {
        return 0;
    }
    let note = auto_mode_allowed_note(source);
    let sub = Rect {
        x: area.x,
        y: area.y.saturating_add(consumed),
        width: area.width,
        height: 1,
    };
    render_gutter_lines(
        vec![Line::from(Span::styled(
            note.to_string(),
            theme.system_info,
        ))],
        note,
        "\u{23bf}",
        theme.assistant_prefix,
        sub,
        buf,
    )
}

pub(super) fn render_edit_tool(
    tool: &crate::streaming::StreamingToolUse,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    force_verbose_preview: bool,
    auto_mode_allowed: Option<rebon_types::AutoModeAllowSource>,
) -> u16 {
    let name = &tool.tool_name;
    let is_failed = tool.status == ToolCallStatus::Failed;
    let is_in_progress = matches!(
        tool.status,
        ToolCallStatus::Pending | ToolCallStatus::InProgress
    );

    // Extract file path — prefer diff path, then raw_input.
    let diff = extract_diff_content(tool);
    let file_path = diff
        .map(|d| d.path.as_str())
        .or_else(|| {
            tool.raw_input
                .as_ref()
                .and_then(|m| m.get("file_path"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("");

    // Determine operation label from lifecycle / raw_output type field.
    let op_label = if is_in_progress && diff.is_none() {
        "Editing"
    } else {
        tool.raw_output
            .as_ref()
            .and_then(|m| m.get("type"))
            .and_then(|v| v.as_str())
            .map(|t| match t {
                "create" => "Write",
                _ => "Update",
            })
            .unwrap_or(name.as_str())
    };

    // Header line: ● Update(path)
    let header_file_path = if is_in_progress && diff.is_none() {
        ""
    } else {
        file_path
    };
    let header_text = if header_file_path.is_empty() {
        op_label.to_string()
    } else {
        format!("{op_label}({header_file_path})")
    };
    let tool_name_style = tool_name_style(theme);
    let tool_summary_style = theme.assistant_prefix;
    let header_line = if header_file_path.is_empty() {
        Line::from(Span::styled(op_label.to_string(), tool_name_style))
    } else {
        Line::from(vec![
            Span::styled(op_label.to_string(), tool_name_style),
            Span::styled("(", tool_summary_style),
            Span::styled(
                hyperlink_path_label(
                    header_file_path,
                    header_file_path.to_string(),
                    theme.supports_hyperlinks,
                ),
                tool_summary_style,
            ),
            Span::styled(")", tool_summary_style),
        ])
    };

    // Render the header with gutter.
    let gutter_style = if is_failed {
        tool_status_style(theme, ToolCallStatus::Failed)
    } else {
        theme.assistant_prefix
    };
    let gutter_glyph = tool_gutter_glyph(theme, is_failed, is_in_progress);
    let render_header_line = if theme.supports_hyperlinks {
        strip_osc8_from_lines(&[header_line.clone()])
            .into_iter()
            .next()
            .unwrap_or_else(|| header_line.clone())
    } else {
        header_line.clone()
    };
    let header_height =
        wrap_height_visible(&header_text, area.width.saturating_sub(GUTTER).max(1)).max(1);
    let header_painted = render_gutter_lines(
        vec![render_header_line],
        &header_text,
        gutter_glyph,
        gutter_style,
        area,
        buf,
    );
    if theme.supports_hyperlinks && header_painted > 0 {
        apply_line_hyperlinks(
            &[header_line],
            Rect {
                x: area.x.saturating_add(GUTTER),
                y: area.y,
                width: area.width.saturating_sub(GUTTER).max(1),
                height: header_painted,
            },
            buf,
        );
    }
    let mut total_consumed = header_height;

    if let Some(diff) = diff {
        // Build context-aware diff lines with line numbers.
        let original_file = tool
            .raw_output
            .as_ref()
            .and_then(|m| m.get("originalFile"))
            .and_then(|v| v.as_str());

        let (context_hunk_lines, context_start_line) =
            build_context_diff(diff.old_text.as_deref(), &diff.new_text, original_file);
        let (additions, removals) = diff_line_counts(diff.old_text.as_deref(), &diff.new_text);
        let change_line_count = additions.saturating_add(removals);
        let summary_text = diff_summary_from_counts(additions, removals);

        // Summary line: ⎿  Added N lines
        let summary_height = 1;
        let summary_area = Rect {
            x: area.x,
            y: area.y.saturating_add(total_consumed),
            width: area.width,
            height: area.height.saturating_sub(total_consumed),
        };
        if summary_area.height > 0 {
            let summary_line = Line::from(vec![
                Span::raw("  "),
                Span::styled("\u{23bf}  ", theme.streaming),
                Span::styled(summary_text, theme.streaming),
            ]);
            let sub = Rect {
                x: summary_area.x,
                y: summary_area.y,
                width: summary_area.width,
                height: 1,
            };
            clear_buffer_area(buf, sub);
            Paragraph::new(summary_line).render(sub, buf);
        }
        total_consumed = total_consumed.saturating_add(summary_height);

        // Render the context diff block.
        let diff_area = Rect {
            x: area.x,
            y: area.y.saturating_add(total_consumed),
            width: area.width,
            height: area.height.saturating_sub(total_consumed),
        };
        let diff_consumed = if matches!(verbosity, ToolOutputVerbosity::Compact)
            && !force_verbose_preview
            && context_hunk_lines.len() > TOOL_PREVIEW_MAX_LINES
            && change_line_count > EDIT_COMPACT_FULL_CONTEXT_CHANGE_LINE_LIMIT
            && !has_foldable_unchanged_run(&context_hunk_lines)
        {
            let preview_lines = context_hunk_lines
                .iter()
                .take(TOOL_PREVIEW_MAX_LINES)
                .cloned()
                .collect::<Vec<_>>();
            let diff_height = measure_diff_block_height(
                &preview_lines,
                context_start_line,
                diff_area.width,
                false,
            );
            let painted =
                render_diff_block_at(&preview_lines, context_start_line, diff_area, buf, false);
            let hidden = context_hunk_lines.len().saturating_sub(preview_lines.len());
            let hint_area = Rect {
                x: diff_area.x,
                y: diff_area.y.saturating_add(painted),
                width: diff_area.width,
                height: diff_area.height.saturating_sub(painted),
            };
            let hint_height = u16::from(hidden > 0);
            if hidden > 0 && hint_area.height > 0 {
                let hint = format!("  … +{hidden} lines");
                clear_buffer_area(
                    buf,
                    Rect {
                        height: 1,
                        ..hint_area
                    },
                );
                Paragraph::new(Line::from(Span::styled(hint, theme.thinking))).render(
                    Rect {
                        height: 1,
                        ..hint_area
                    },
                    buf,
                );
            }
            diff_height.saturating_add(hint_height)
        } else {
            // `force_verbose_preview` bypasses the compact line-count cap so
            // moderate inline edits stay complete, but it must not disable the
            // shared prompt-style fold. Only explicit verbose mode expands long
            // changed or unchanged runs.
            let fold_long_runs = !matches!(verbosity, ToolOutputVerbosity::Verbose);
            let diff_height = measure_diff_block_height(
                &context_hunk_lines,
                context_start_line,
                diff_area.width,
                fold_long_runs,
            );
            render_diff_block_at(
                &context_hunk_lines,
                context_start_line,
                diff_area,
                buf,
                fold_long_runs,
            );
            diff_height
        };
        total_consumed = total_consumed.saturating_add(diff_consumed);

        if let Some(notification) = extract_memory_notification(tool) {
            if total_consumed < area.height {
                let note_area = Rect {
                    x: area.x,
                    y: area.y.saturating_add(total_consumed),
                    width: area.width,
                    height: area.height.saturating_sub(total_consumed),
                };
                let note_line = Line::from(vec![
                    Span::raw("  "),
                    Span::styled("\u{23bf}  ", theme.streaming),
                    Span::styled(notification.clone(), theme.streaming),
                ]);
                let sub = Rect {
                    x: note_area.x,
                    y: note_area.y,
                    width: note_area.width,
                    height: 1,
                };
                clear_buffer_area(buf, sub);
                Paragraph::new(note_line).render(sub, buf);
            }
            total_consumed = total_consumed.saturating_add(1);
        }
    } else if is_failed {
        let error_text = ["Edit failed".to_string()];
        let err_width = area.width.saturating_sub(GUTTER).max(1);
        let wrapped_error_text = hard_wrap_text_lines(&error_text, err_width);
        let err_height = wrapped_error_text.len().min(u16::MAX as usize) as u16;
        if total_consumed < area.height {
            let err_area = Rect {
                x: area.x.saturating_add(GUTTER),
                y: area.y.saturating_add(total_consumed),
                width: area.width.saturating_sub(GUTTER),
                height: area.height.saturating_sub(total_consumed),
            };
            let paint_height = err_height.min(err_area.height);
            if paint_height > 0 {
                let sub = Rect {
                    height: paint_height,
                    ..err_area
                };
                let error_lines = wrapped_error_text
                    .iter()
                    .map(|line| Line::from(Span::styled(line.clone(), theme.system_error)))
                    .collect::<Vec<_>>();
                clear_buffer_area(buf, sub);
                Paragraph::new(error_lines)
                    .wrap(Wrap { trim: false })
                    .render(sub, buf);
            }
        }
        total_consumed = total_consumed.saturating_add(err_height);
    }

    total_consumed.saturating_add(render_auto_mode_allowed_row(
        auto_mode_allowed,
        area,
        total_consumed,
        buf,
        theme,
    ))
}

#[cfg(test)]
pub(super) fn render_streaming_tool_use(
    tool: &crate::streaming::StreamingToolUse,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    extras: TranscriptRenderExtras<'_>,
) -> u16 {
    render_streaming_tool_use_with_content(tool, None, area, buf, theme, verbosity, extras, false)
}

struct StreamingToolHeader {
    text: String,
    line: Line<'static>,
    is_completed_skill: bool,
}

fn build_streaming_tool_header(
    tool: &crate::streaming::StreamingToolUse,
    display_name: &str,
    is_web_search: bool,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    extras: TranscriptRenderExtras<'_>,
) -> StreamingToolHeader {
    // Non-Edit tools: use raw_input summary for the header.
    let name: &str = &tool.tool_name;
    let agent_verbose_description = (name == "Agent"
        && matches!(verbosity, ToolOutputVerbosity::Verbose))
    .then(|| agent_description(tool.raw_input.as_ref()))
    .flatten();
    let async_agent_summary = if agent_verbose_description.is_some() {
        None
    } else {
        async_agent_launch_header_summary(tool, extras)
    };
    // ShellOutput/ShellStop: prefer the originating command echoed back in
    // the result over the opaque shellId from the input.
    let shell_management_summary = tool.raw_output.as_ref().and_then(|raw_output| {
        rebon_render::shell_output::shell_management_header_summary(name, raw_output)
    });
    let summary = if is_streaming_plan_ledger(tool) {
        let fallback_result = streaming_plan_ledger_fallback_result(tool);
        plan_ledger_summary(
            streaming_plan_ledger_input_value(tool, "operation").and_then(Value::as_str),
            streaming_plan_ledger_input_value(tool, "items"),
            tool.raw_output
                .as_ref()
                .and_then(|output| output.get("requirements"))
                .or_else(|| {
                    fallback_result
                        .as_ref()
                        .and_then(|output| output.get("requirements"))
                }),
        )
    } else if is_web_search {
        tool.raw_input
            .as_ref()
            .map(|input| streaming_tool_summary(name, input))
            .unwrap_or_default()
    } else {
        shell_management_summary
            .or_else(|| agent_verbose_description.clone())
            .or_else(|| async_agent_summary.clone())
            .or_else(|| {
                if name == "Workflow" {
                    workflow_tool_summary(tool)
                } else {
                    tool.raw_input
                        .as_ref()
                        .map(|m| streaming_tool_summary(name, m))
                        .filter(|s| !s.is_empty())
                }
            })
            .or_else(|| tool.title.clone())
            .unwrap_or_default()
            .replace('\r', " ")
            .replace('\n', " ")
    };
    let summary = strip_agent_summary_prefix(tool, display_name, summary);
    let is_completed_skill = name == "Skill" && tool.status == ToolCallStatus::Completed;

    // Build the header line with split styling: tool name is bold
    // (tool_header), while the trailing summary uses the inactive color.
    let agent_verbose_header = name == "Agent" && matches!(verbosity, ToolOutputVerbosity::Verbose);
    let agent_subtype_header = !agent_verbose_header
        && name == "Agent"
        && (display_name != "Agent" || async_agent_summary.is_some());
    let agent_usage_in_body = tool.raw_output.as_ref().is_some_and(|raw_output| {
        is_agent_final_output(name, Some(raw_output))
            && !is_plan_agent_final_output(tool, Some(raw_output))
    });
    let agent_usage_summary =
        if name == "Agent" && tool.status == ToolCallStatus::Completed && !agent_usage_in_body {
            tool.raw_output
                .as_ref()
                .and_then(agent_usage_header_summary)
        } else {
            None
        };
    let mut header_text = if is_completed_skill {
        if summary.is_empty() {
            format!("{display_name} loaded")
        } else {
            format!("{display_name} ({summary}) loaded")
        }
    } else if summary.is_empty() {
        display_name.to_string()
    } else if agent_verbose_header {
        format!("{display_name}({summary})")
    } else if agent_subtype_header {
        format!("{display_name}: {summary}")
    } else {
        format!("{display_name} ({summary})")
    };
    if let Some(usage) = &agent_usage_summary {
        header_text.push_str(&format!(" [{usage}]"));
    }
    let tool_name_style = tool_name_style(theme);
    let tool_summary_style = theme.assistant_prefix;
    let mut header_line = if is_completed_skill {
        if summary.is_empty() {
            Line::from(vec![
                Span::styled(display_name.to_string(), tool_name_style),
                Span::styled(" loaded", tool_summary_style),
            ])
        } else {
            Line::from(vec![
                Span::styled(display_name.to_string(), tool_name_style),
                Span::styled(format!(" ({summary}) loaded"), tool_summary_style),
            ])
        }
    } else if summary.is_empty() {
        Line::from(Span::styled(display_name.to_string(), tool_name_style))
    } else if agent_verbose_header {
        Line::from(vec![
            Span::styled(display_name.to_string(), tool_name_style),
            Span::styled("(", tool_summary_style),
            Span::styled(summary.to_string(), tool_summary_style),
            Span::styled(")", tool_summary_style),
        ])
    } else if agent_subtype_header {
        Line::from(vec![
            Span::styled(display_name.to_string(), tool_name_style),
            Span::styled(format!(": {summary}"), tool_summary_style),
        ])
    } else {
        Line::from(vec![
            Span::styled(display_name.to_string(), tool_name_style),
            Span::styled(format!(" ({summary})"), tool_summary_style),
        ])
    };
    if let Some(usage) = agent_usage_summary {
        header_line
            .spans
            .push(Span::styled(format!(" [{usage}]"), tool_summary_style));
    }

    StreamingToolHeader {
        text: header_text,
        line: header_line,
        is_completed_skill,
    }
}

struct StreamingToolBody {
    lines: Vec<Line<'static>>,
    text: Vec<String>,
    gutter_style: Style,
}

struct StreamingToolBodyContext<'a> {
    tool: &'a crate::streaming::StreamingToolUse,
    full_live_shell_content: Option<&'a [ToolCallContent]>,
    area: Rect,
    theme: &'a RenderTheme,
    verbosity: ToolOutputVerbosity,
    extras: TranscriptRenderExtras<'a>,
    suppress_expand_hints: bool,
    is_web_search: bool,
    is_web_fetch: bool,
    is_failed: bool,
    is_live_shell: bool,
    is_completed_skill: bool,
}

fn streaming_tool_title_differing_from_header(
    tool: &crate::streaming::StreamingToolUse,
    is_web_search: bool,
) -> Option<String> {
    if is_web_search {
        return None;
    }
    tool.title.as_ref().and_then(|title| {
        let header_summary = tool
            .raw_input
            .as_ref()
            .map(|input| streaming_tool_summary(&tool.tool_name, input));
        if header_summary.as_ref() != Some(title) {
            Some(title.clone())
        } else {
            None
        }
    })
}

fn append_plan_ledger_streaming_tool_body(
    context: &StreamingToolBodyContext<'_>,
    body: &mut StreamingToolBody,
) {
    let body_style = if context.is_failed {
        context.theme.system_error
    } else {
        context.theme.streaming
    };
    for line in plan_ledger_streaming_body_lines(context.tool, context.verbosity) {
        push_tool_body_line(&mut body.lines, &mut body.text, line, body_style);
    }
}

fn append_failed_streaming_tool_body(
    context: &StreamingToolBodyContext<'_>,
    body: &mut StreamingToolBody,
) {
    let tool = context.tool;
    let name: &str = &tool.tool_name;
    let body_style = context.theme.system_error;
    if context.is_web_search {
        if let Some(answer) = tool
            .raw_output
            .as_ref()
            .and_then(|output| output.get("answer"))
            .and_then(Value::as_str)
        {
            for line in answer.lines() {
                body.lines
                    .push(Line::from(Span::styled(line.to_string(), body_style)));
                body.text.push(line.to_string());
            }
        }
    } else if let Some(lines) = collect_shell_tool_body_lines(
        name,
        tool.content.as_deref(),
        tool.raw_output
            .as_ref()
            .and_then(|output| output.get("stdout"))
            .and_then(Value::as_str),
        tool.raw_output
            .as_ref()
            .and_then(|output| output.get("stderr"))
            .and_then(Value::as_str),
        tool.raw_output
            .as_ref()
            .and_then(|output| output.get(rebon_tools_core::shell_stream_order::STREAM_ORDER_KEY))
            .and_then(Value::as_str),
        true,
    ) {
        let mut lines = tool_content_preview_lines(
            &lines,
            name,
            context.verbosity,
            context.area.width.saturating_sub(GUTTER).max(1),
        );
        if context.suppress_expand_hints {
            let suffix = format!(
                " ({})",
                format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
            );
            for line in &mut lines {
                if let Some(without_hint) = line.strip_suffix(&suffix) {
                    *line = without_hint.to_string();
                }
            }
        }
        for line in lines {
            push_tool_body_line(&mut body.lines, &mut body.text, line, body_style);
        }
    } else {
        if let Some(content) = &tool.content {
            for item in content {
                let s = render_tool_call_content(item);
                for line in s.split('\n') {
                    body.lines
                        .push(Line::from(Span::styled(line.to_string(), body_style)));
                    body.text.push(line.to_string());
                }
            }
        }
        if let Some(raw_output) = &tool.raw_output {
            if name == "Agent" {
                if let Some(activity_lines) = extract_agent_activity_lines(raw_output) {
                    for line in activity_lines {
                        body.lines
                            .push(Line::from(Span::styled(line.clone(), body_style)));
                        body.text.push(line);
                    }

                    let remaining_raw_output: HashMap<String, Value> = raw_output
                        .iter()
                        .filter(|(key, _)| key.as_str() != "activity_lines")
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    let output = compact_json_map(&remaining_raw_output);
                    if !output.is_empty() {
                        body.lines
                            .push(Line::from(Span::styled(output.clone(), body_style)));
                        body.text.push(output);
                    }
                } else {
                    let output = compact_json_map(raw_output);
                    if !output.is_empty() {
                        body.lines
                            .push(Line::from(Span::styled(output.clone(), body_style)));
                        body.text.push(output);
                    }
                }
            } else {
                let output = compact_json_map(raw_output);
                if !output.is_empty() {
                    body.lines
                        .push(Line::from(Span::styled(output.clone(), body_style)));
                    body.text.push(output);
                }
            }
        }
    }
}

fn append_compact_streaming_tool_body(
    context: &StreamingToolBodyContext<'_>,
    body: &mut StreamingToolBody,
) {
    let tool = context.tool;
    let name: &str = &tool.tool_name;
    let all = if let Some(raw_output) = tool
        .raw_output
        .as_ref()
        .filter(|raw_output| is_agent_final_output(name, Some(raw_output)))
    {
        if is_plan_agent_final_output(tool, Some(raw_output)) {
            agent_final_text_lines(raw_output)
        } else {
            vec![agent_final_summary_line(raw_output)]
        }
    } else {
        normalize_tool_body_lines(
            collect_tool_body_lines(
                tool,
                None,
                context.theme.supports_hyperlinks,
                context.extras,
            ),
            false,
        )
    };
    if all.is_empty() {
        return;
    }

    let (preview, hint) = if context.is_live_shell {
        let (preview, hidden) =
            live_shell_preview_lines(&all, context.area.width.saturating_sub(GUTTER).max(1));
        let hint = (hidden && !context.suppress_expand_hints)
            .then(|| format_shortcut_hint("ctrl+o", "show all", false, false).plain_text);
        (preview, hint)
    } else if is_plan_agent_final_output(tool, tool.raw_output.as_ref()) {
        (all.clone(), None)
    } else if is_agent_final_output(name, tool.raw_output.as_ref()) {
        let preview_count = all.len().min(TOOL_PREVIEW_MAX_LINES);
        let preview = all.iter().take(preview_count).cloned().collect();
        let hint =
            (all.len() > preview_count).then(|| format!("… +{} lines", all.len() - preview_count));
        (preview, hint)
    } else {
        let preview_source = visual_preview_lines(
            if context.is_web_fetch {
                "WebFetch"
            } else {
                name
            },
            &all,
            context.area.width.saturating_sub(GUTTER).max(1),
        );
        let (preview, mut hint) = compact_preview_lines(&preview_source, name);
        if context.suppress_expand_hints {
            let suffix = format!(
                " ({})",
                format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
            );
            if let Some(without_hint) = hint.as_deref().and_then(|hint| hint.strip_suffix(&suffix))
            {
                hint = Some(without_hint.to_string());
            }
        }
        (preview, hint)
    };
    for line in preview {
        push_tool_body_line(
            &mut body.lines,
            &mut body.text,
            line,
            context.theme.streaming,
        );
    }
    if let Some(hint) = hint {
        body.lines.push(Line::from(Span::styled(
            hint.clone(),
            context.theme.thinking,
        )));
        body.text.push(hint);
    }
}

fn append_normal_streaming_tool_body(
    context: &StreamingToolBodyContext<'_>,
    body: &mut StreamingToolBody,
) {
    let tool = context.tool;
    let name: &str = &tool.tool_name;
    let agent_final_raw_output = tool
        .raw_output
        .as_ref()
        .filter(|raw_output| is_agent_final_output(name, Some(raw_output)));
    if agent_final_raw_output.is_none() {
        if let Some(title) = streaming_tool_title_differing_from_header(tool, context.is_web_search)
        {
            body.lines.push(Line::from(Span::styled(
                title.clone(),
                context.theme.streaming,
            )));
            body.text.push(title);
        }
    }
    let all = if let Some(raw_output) = agent_final_raw_output {
        agent_final_text_lines(raw_output)
    } else if rebon_render::streaming::is_live_shell_tool(name) {
        collect_tool_body_lines(
            tool,
            None,
            context.theme.supports_hyperlinks,
            context.extras,
        )
    } else if context.is_web_search {
        tool.raw_output
            .as_ref()
            .and_then(|output| output.get("answer"))
            .and_then(Value::as_str)
            .map(|answer| answer.lines().map(str::to_string).collect())
            .unwrap_or_default()
    } else if context.is_web_fetch {
        collect_tool_body_lines(
            tool,
            None,
            context.theme.supports_hyperlinks,
            context.extras,
        )
    } else {
        tool.content
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
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    if all.is_empty() {
        return;
    }

    if context.is_live_shell {
        let (preview, _) =
            live_shell_preview_lines(&all, context.area.width.saturating_sub(GUTTER).max(1));
        for line in preview {
            push_tool_body_line(
                &mut body.lines,
                &mut body.text,
                line,
                context.theme.streaming,
            );
        }
    } else {
        let preview_source = visual_preview_lines(
            if context.is_web_fetch {
                "WebFetch"
            } else {
                name
            },
            &all,
            context.area.width.saturating_sub(GUTTER).max(1),
        );
        let show_full_plan_agent = is_plan_agent_final_output(tool, agent_final_raw_output);
        let preview_count = if show_full_plan_agent {
            preview_source.len()
        } else {
            preview_source.len().min(TOOL_PREVIEW_MAX_LINES)
        };
        let preview: Vec<String> = if name == "Agent" && !show_full_plan_agent {
            preview_source
                .iter()
                .rev()
                .take(preview_count)
                .cloned()
                .collect()
        } else {
            preview_source.iter().take(preview_count).cloned().collect()
        };
        for line in preview {
            push_tool_body_line(
                &mut body.lines,
                &mut body.text,
                line,
                context.theme.streaming,
            );
        }
        if preview_source.len() > preview_count {
            let remaining = preview_source.len() - preview_count;
            let hint = if name == "Agent" {
                if is_agent_final_output(name, tool.raw_output.as_ref()) {
                    format!("… +{remaining} lines")
                } else {
                    format!("… +{remaining} older lines")
                }
            } else {
                format!("… +{remaining} lines")
            };
            body.lines.push(Line::from(Span::styled(
                hint.clone(),
                context.theme.thinking,
            )));
            body.text.push(hint);
        }
    }
}

fn append_verbose_streaming_tool_body(
    context: &StreamingToolBodyContext<'_>,
    body: &mut StreamingToolBody,
) {
    let tool = context.tool;
    let name: &str = &tool.tool_name;
    let agent_final_raw_output = tool
        .raw_output
        .as_ref()
        .filter(|raw_output| is_agent_final_output(name, Some(raw_output)));
    if agent_final_raw_output.is_none() && name != "Agent" {
        if let Some(title) = streaming_tool_title_differing_from_header(tool, context.is_web_search)
        {
            body.lines.push(Line::from(Span::styled(
                title.clone(),
                context.theme.streaming,
            )));
            body.text.push(title);
        }
    }
    if name == "Agent" {
        for line in agent_full_detail_lines(
            tool.raw_input.as_ref(),
            tool.raw_output.as_ref(),
            tool.content.as_deref(),
        ) {
            push_tool_body_line(
                &mut body.lines,
                &mut body.text,
                line,
                context.theme.streaming,
            );
        }
    } else if name == "InvokeDeferredTool" {
        if !context.is_web_search {
            if let Some(verbose) = tool
                .raw_input
                .as_ref()
                .and_then(|input| deferred_tool_verbose_summary(input))
            {
                for line in verbose.lines() {
                    let text = line.to_string();
                    body.lines.push(Line::from(Span::styled(
                        text.clone(),
                        context.theme.streaming,
                    )));
                    body.text.push(text);
                }
            }
        }
        for line in collect_tool_body_lines(
            tool,
            None,
            context.theme.supports_hyperlinks,
            context.extras,
        ) {
            push_tool_body_line(
                &mut body.lines,
                &mut body.text,
                line,
                context.theme.streaming,
            );
        }
    } else if let Some(raw_output) = agent_final_raw_output {
        for line in agent_final_text_lines(raw_output) {
            push_tool_body_line(
                &mut body.lines,
                &mut body.text,
                line,
                context.theme.streaming,
            );
        }
    } else {
        for line in collect_tool_body_lines(
            tool,
            context.full_live_shell_content,
            context.theme.supports_hyperlinks,
            context.extras,
        ) {
            push_tool_body_line(
                &mut body.lines,
                &mut body.text,
                line,
                context.theme.streaming,
            );
        }
    }
}

fn append_completed_code_preview(
    context: &StreamingToolBodyContext<'_>,
    body: &mut StreamingToolBody,
) {
    let content = context.tool.content.as_deref().unwrap_or_default();
    let width = context.area.width.saturating_sub(GUTTER).max(1);
    let verbose = context.verbosity == ToolOutputVerbosity::Verbose;
    let summary = rebon_render::code_mode::completed_summary(content);
    push_tool_body_line(
        &mut body.lines,
        &mut body.text,
        if verbose {
            summary
        } else {
            rebon_width::truncate_to_ellipsis(&summary, width as usize)
        },
        tool_status_style(context.theme, ToolCallStatus::Completed),
    );

    // Progress has structured metadata; a console line that happens to read
    // "← Bash succeeded" is still output. Older, unannotated content is kept,
    // not guessed away. There may be several final blocks, or none at all.
    let output = content
        .iter()
        .filter(|item| rebon_render::tool_output::tool_progress_metadata(item).is_none())
        .flat_map(|item| {
            render_tool_call_content(item)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    let mut all = normalize_tool_body_lines(output, false);
    let no_output = all.is_empty();
    if verbose {
        append_verbose_streaming_tool_body(context, body);
        if no_output {
            push_tool_body_line(
                &mut body.lines,
                &mut body.text,
                "No final output received".into(),
                context.theme.thinking,
            );
        }
        return;
    }
    if no_output {
        all.push("No final output received".into());
    }
    let all = hard_wrap_text_lines(&all, width);
    // Keep the beginning and the conclusion of the whole output, with one
    // bounded preview budget across blocks and terminal-width wrapped lines.
    let hidden = all.len().saturating_sub(TOOL_PREVIEW_MAX_LINES);
    let head = if hidden > 0 {
        TOOL_PREVIEW_MAX_LINES - 1
    } else {
        all.len()
    };
    for line in &all[..head] {
        push_tool_body_line(
            &mut body.lines,
            &mut body.text,
            line.clone(),
            context.theme.streaming,
        );
    }
    if hidden > 0 {
        push_tool_body_line(
            &mut body.lines,
            &mut body.text,
            format!("… +{hidden} lines"),
            context.theme.thinking,
        );
        push_tool_body_line(
            &mut body.lines,
            &mut body.text,
            all[all.len() - 1].clone(),
            context.theme.streaming,
        );
    }
}

fn append_code_failure_preview(
    context: &StreamingToolBodyContext<'_>,
    body: &mut StreamingToolBody,
) {
    let error = context
        .tool
        .raw_output
        .as_ref()
        .and_then(|output| output.get("error"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            context
                .tool
                .content
                .as_ref()?
                .last()
                .map(render_tool_call_content)
        });
    if let Some(reason) = error
        .as_deref()
        .and_then(|error| error.lines().find(|line| !line.trim().is_empty()))
    {
        let reason = rebon_render::text::expand_tabs_for_tui(reason.trim());
        let preview = rebon_width::truncate_to_ellipsis(
            &reason,
            context.area.width.saturating_sub(GUTTER).max(1) as usize,
        );
        push_tool_body_line(
            &mut body.lines,
            &mut body.text,
            preview,
            context.theme.system_error,
        );
    }
}

fn append_code_program(context: &StreamingToolBodyContext<'_>, body: &mut StreamingToolBody) {
    let input = serde_json::to_value(&context.tool.raw_input).expect("tool input serializes");
    let Some(program) = rebon_render::code_mode::program_text(&context.tool.tool_name, &input)
    else {
        return;
    };
    if context.verbosity == ToolOutputVerbosity::Verbose {
        push_tool_body_line(
            &mut body.lines,
            &mut body.text,
            "JavaScript:".into(),
            context.theme.thinking,
        );
        for line in rebon_render::text::expand_tabs_for_tui(&program).lines() {
            push_tool_body_line(
                &mut body.lines,
                &mut body.text,
                line.to_string(),
                context.theme.streaming,
            );
        }
    } else if !context.is_failed && !context.suppress_expand_hints {
        let hint = format_shortcut_hint("ctrl+o", "expand", false, false).plain_text;
        push_tool_body_line(
            &mut body.lines,
            &mut body.text,
            hint,
            context.theme.thinking,
        );
    }
}

fn build_streaming_tool_body(mut context: StreamingToolBodyContext<'_>) -> StreamingToolBody {
    // Build the body block. What lands here depends on status + verbosity:
    //
    //   * Failed   — run_code shows one reason line; shell output uses the same
    //                width-aware Compact/Normal preview cap as successful output.
    //                Other tools and Verbose show the full error content.
    //   * Completed run_code retains a sequence overview and final output;
    //                earlier progress and source remain available in Verbose.
    //   * Compact  — preview the first `TOOL_PREVIEW_MAX_LINES` body
    //                lines and append `… +N lines (ctrl+o to expand)`
    //                when more remain. Empty body ⇒ no body block.
    //   * Normal   — title (if it adds information beyond the header)
    //                plus the same capped body preview Compact showed,
    //                so Ctrl+O expansion never removes visible detail.
    //   * Verbose  — title + full content + locations + raw_output.
    //
    // Each branch emits pre-styled `Line`s; the caller below paints
    // them with a shared `⎿` gutter and short-circuits when the parent
    // area has already been fully consumed.
    let mut body = StreamingToolBody {
        lines: Vec::new(),
        text: Vec::new(),
        gutter_style: if context.is_failed {
            tool_status_style(context.theme, ToolCallStatus::Failed)
        } else {
            context.theme.assistant_prefix
        },
    };

    let is_code = context.tool.tool_name == rebon_render::code_mode::RUN_CODE_TOOL_NAME;
    let suppress_expand_hints = context.suppress_expand_hints;
    context.suppress_expand_hints |= is_code;
    if is_streaming_plan_ledger(context.tool) {
        append_plan_ledger_streaming_tool_body(&context, &mut body);
    } else if context.is_failed {
        if is_code && context.verbosity != ToolOutputVerbosity::Verbose {
            append_code_failure_preview(&context, &mut body);
        } else {
            append_failed_streaming_tool_body(&context, &mut body);
        }
    } else if is_code && context.tool.status == ToolCallStatus::Completed {
        append_completed_code_preview(&context, &mut body);
    } else if !context.is_completed_skill {
        match context.verbosity {
            ToolOutputVerbosity::Compact => append_compact_streaming_tool_body(&context, &mut body),
            ToolOutputVerbosity::Normal => append_normal_streaming_tool_body(&context, &mut body),
            ToolOutputVerbosity::Verbose => append_verbose_streaming_tool_body(&context, &mut body),
        }
    }
    if is_code {
        context.suppress_expand_hints = suppress_expand_hints;
        append_code_program(&context, &mut body);
    }

    body
}

pub(super) fn render_streaming_tool_use_with_content(
    tool: &crate::streaming::StreamingToolUse,
    full_live_shell_content: Option<&[ToolCallContent]>,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    extras: TranscriptRenderExtras<'_>,
    suppress_expand_hints: bool,
) -> u16 {
    // Use the canonical tool_name for behaviour and summaries, but derive
    // the Agent instance label from its display metadata when available.
    let name: &str = &tool.tool_name;
    let display_name = agent_display_name(tool)
        .unwrap_or_else(|| streaming_tool_display_name(name, tool.raw_input.as_ref()));
    let is_web_search = display_name == "WebSearch";
    let is_web_fetch = display_name == "WebFetch";
    // Every card shape below gets the same note, so resolve it once here
    // rather than in each branch's own renderer.
    let auto_mode_allowed = extras
        .auto_mode_allowed_tool_ids
        .get(&tool.call_id)
        .copied();

    // Hide internal team-coordination tools from the transcript: these
    // tools have no transcript card.
    if TRANSCRIPT_HIDDEN_TOOLS.iter().any(|&hidden| hidden == name) {
        return 0;
    }

    if workflow_render_tool_name(tool).is_some() {
        let workflow_tool = workflow_streaming_tool(tool);
        return render_workflow_tool(
            &workflow_tool,
            area,
            buf,
            theme,
            verbosity,
            extras.inline_live_workflow_card_max_rows,
            auto_mode_allowed,
        );
    }

    // Edit tools get dedicated rendering in all states:
    // - With diff (completed successfully): show diff block
    // - In progress before diff arrives: show an Editing placeholder
    // - Failed without diff: show full file path + error
    // The final file path is never truncated to keep edit failures actionable.
    if tool.kind == ToolKind::Edit {
        return render_edit_tool(
            tool,
            area,
            buf,
            theme,
            verbosity,
            extras.force_verbose_edit_tool_previews,
            auto_mode_allowed,
        );
    }

    let is_failed = tool.status == ToolCallStatus::Failed;
    let is_in_progress = matches!(
        tool.status,
        ToolCallStatus::InProgress | ToolCallStatus::Pending
    );
    let is_live_shell = is_in_progress && rebon_render::streaming::is_live_shell_tool(name);
    let StreamingToolHeader {
        text: mut header_text,
        line: mut header_line,
        is_completed_skill,
    } = build_streaming_tool_header(tool, display_name, is_web_search, theme, verbosity, extras);
    if name == rebon_render::code_mode::RUN_CODE_TOOL_NAME && is_failed {
        header_text.push_str(" failed");
        header_line
            .spans
            .push(Span::styled(" failed", theme.system_error));
        if verbosity != ToolOutputVerbosity::Verbose && !suppress_expand_hints {
            let hint = format!(
                " ({})",
                format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
            );
            header_text.push_str(&hint);
            header_line.spans.push(Span::styled(hint, theme.thinking));
        }
    }

    let gutter_style = tool_gutter_style(theme, is_failed, is_in_progress);
    let gutter_glyph = tool_gutter_glyph(theme, is_failed, is_in_progress);

    // Render the header row (always shown) via its own gutter call so
    // the body block below can own a `⎿` glyph without fighting the
    // single-paragraph wrap used by the legacy all-in-one path.
    let mut consumed = render_gutter_lines(
        vec![header_line.clone()],
        &header_text,
        gutter_glyph,
        gutter_style,
        area,
        buf,
    );
    if theme.supports_hyperlinks {
        apply_line_hyperlinks(
            &[header_line],
            Rect {
                x: area.x.saturating_add(GUTTER),
                y: area.y,
                width: area.width.saturating_sub(GUTTER).max(1),
                height: consumed,
            },
            buf,
        );
    }

    let StreamingToolBody {
        lines: body_lines,
        text: body_text,
        gutter_style: body_gutter_style,
    } = build_streaming_tool_body(StreamingToolBodyContext {
        tool,
        full_live_shell_content,
        area,
        theme,
        verbosity,
        extras,
        suppress_expand_hints,
        is_web_search,
        is_web_fetch,
        is_failed,
        is_live_shell,
        is_completed_skill,
    });

    if !body_lines.is_empty() && consumed < area.height {
        let sub = Rect {
            x: area.x,
            y: area.y.saturating_add(consumed),
            width: area.width,
            height: area.height.saturating_sub(consumed),
        };
        let body_text_joined = body_text.join("\n");
        let body_height =
            wrap_height_visible(&body_text_joined, sub.width.saturating_sub(GUTTER).max(1))
                .max(1)
                .min(sub.height);
        let render_lines = if theme.supports_hyperlinks {
            strip_osc8_from_lines(&body_lines)
        } else {
            body_lines.clone()
        };
        consumed = consumed.saturating_add(render_gutter_lines(
            render_lines,
            &body_text_joined,
            "⎿",
            body_gutter_style,
            sub,
            buf,
        ));
        if theme.supports_hyperlinks {
            apply_line_hyperlinks(
                &body_lines,
                Rect {
                    x: sub.x.saturating_add(GUTTER),
                    y: sub.y,
                    width: sub.width.saturating_sub(GUTTER).max(1),
                    height: body_height,
                },
                buf,
            );
        }
    }

    // Inline ctrl+b hint under any in-progress Agent spawn so users
    // discover the background shortcut exactly where it's relevant.
    // Skipped for completed/failed agents (the static-dot branch above
    // handles styling; this block is the hint-line equivalent).
    if is_in_progress && name == "Agent" && consumed < area.height {
        let sub = Rect {
            x: area.x,
            y: area.y.saturating_add(consumed),
            width: area.width,
            height: area.height.saturating_sub(consumed),
        };
        let hint_text =
            format_shortcut_hint("ctrl+b", "run in background", false, false).plain_text;
        consumed = consumed.saturating_add(render_wrapped(
            &hint_text,
            " ",
            theme.thinking,
            theme.thinking,
            sub,
            buf,
        ));
    }

    if let Some(timeout_ms) = tool
        .raw_input
        .as_ref()
        .and_then(|input| input.get("timeout"))
        .and_then(Value::as_u64)
    {
        let timeout = format!(
            "Timeout: {}",
            rebon_render::workflow_body::format_duration_ms(timeout_ms)
        );
        let sub = Rect {
            x: area.x,
            y: area.y.saturating_add(consumed),
            width: area.width,
            height: area.height.saturating_sub(consumed).min(1),
        };
        consumed = consumed.saturating_add(render_gutter_lines(
            vec![Line::from(Span::styled(timeout.clone(), theme.thinking))],
            &timeout,
            "⎿",
            theme.assistant_prefix,
            sub,
            buf,
        ));
    }

    consumed.saturating_add(render_auto_mode_allowed_row(
        auto_mode_allowed,
        area,
        consumed,
        buf,
        theme,
    ))
}
