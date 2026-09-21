use super::*;
pub(super) use rebon_types::wall_clock_ms;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

// The fold moved to `rebon_render::fold_rows` so the app and the page can
// ask the question the terminal asks. Re-exported under the old paths so
// every call site in this crate reads as it did; what stays in this file
// is drawing and measuring.
pub use rebon_render::fold_rows::trailing_collapsible_tool_run_start;
pub(super) use rebon_render::fold_rows::{
    async_agent_launch_only_assistant_tools, fold_rows, is_async_agent_launch_value, is_meta_user,
    is_thinking_only_assistant, message_has_non_tool_user_content, tool_only_assistant,
    tool_result_only_user, transcript_segment_contains_collapsed, TranscriptSegment,
};

/// The terminal's own entry: it holds the extras, and the fold wants the
/// one field of them it reads.
pub(super) fn build_transcript_segments_with_extras(
    rows: &[Message],
    extras: TranscriptRenderExtras<'_>,
) -> Vec<TranscriptSegment> {
    fold_rows(rows, extras.render_thinking_only_rows)
}

pub(super) fn latest_running_transcript_hint_segment(
    rows: &[Message],
    segments: &[TranscriptSegment],
    turn_running: bool,
) -> Option<usize> {
    if !turn_running {
        return None;
    }

    let (idx, segment) = segments.iter().enumerate().next_back()?;
    if !transcript_segment_contains_collapsed(segment) {
        return None;
    }

    let latest_user_prompt = rows.iter().rposition(message_has_non_tool_user_content);
    if latest_user_prompt.is_some_and(|row| segment.first() <= row) {
        return None;
    }

    Some(idx)
}

fn hash_live_agent_tool_activity(
    activity: Option<&LiveAgentToolActivity>,
    hasher: &mut DefaultHasher,
) {
    let Some(activity) = activity else {
        0u8.hash(hasher);
        return;
    };

    1u8.hash(hasher);
    activity.status.hash(hasher);
    activity.text.hash(hasher);
    activity.title.hash(hasher);
    activity.display_name.hash(hasher);
    activity.start_time_ms.hash(hasher);
    activity.end_time_ms.hash(hasher);
    activity.tool_use_count.hash(hasher);
    activity.token_count.hash(hasher);
    activity
        .terminal_result
        .as_ref()
        .and_then(|result| {
            result
                .get("duration_ms")
                .or_else(|| result.get("durationMs"))
        })
        .and_then(Value::as_u64)
        .hash(hasher);
}

pub(super) fn message_live_agent_activity_signature(
    row: &Message,
    extras: TranscriptRenderExtras<'_>,
) -> u64 {
    let Message::Assistant(assistant) = row else {
        return 0;
    };

    let mut hasher = DefaultHasher::new();
    let mut has_async_agent = false;
    for block in &assistant.message.content {
        let AssistantContentBlock::ToolUse(tool) = block else {
            continue;
        };
        if !is_async_agent_launch_value(tool) {
            continue;
        }
        has_async_agent = true;
        tool.id.hash(&mut hasher);
        hash_live_agent_tool_activity(extras.live_agent_tool_activity.get(&tool.id), &mut hasher);
    }

    has_async_agent.then(|| hasher.finish()).unwrap_or(0)
}

pub(super) fn segment_live_agent_activity_signature(
    rows: &[Message],
    segment: &TranscriptSegment,
    extras: TranscriptRenderExtras<'_>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut has_async_agent = false;
    let mut hash_row = |idx: usize| {
        let signature = rows
            .get(idx)
            .map(|row| message_live_agent_activity_signature(row, extras))
            .unwrap_or(0);
        if signature != 0 {
            has_async_agent = true;
            idx.hash(&mut hasher);
            signature.hash(&mut hasher);
        }
    };

    match segment {
        TranscriptSegment::Single(idx) => hash_row(*idx),
        TranscriptSegment::Collapsed { indices } | TranscriptSegment::AgentGroup { indices } => {
            for idx in indices {
                hash_row(*idx);
            }
        }
        TranscriptSegment::ThinkingGroup { segments } => {
            for segment in segments {
                let signature = segment_live_agent_activity_signature(rows, segment, extras);
                if signature != 0 {
                    has_async_agent = true;
                    signature.hash(&mut hasher);
                }
            }
        }
    }

    has_async_agent.then(|| hasher.finish()).unwrap_or(0)
}

/// Paint a run of async background Agent launches as one live summary.
pub(super) fn render_committed_agent_group(
    rows: &[Message],
    indices: &[usize],
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    last_thinking_block_id: Option<&str>,
    extras: TranscriptRenderExtras<'_>,
    now_ms: u64,
) -> u16 {
    if area.height == 0 || area.width == 0 || indices.is_empty() {
        return 0;
    }
    if verbosity != ToolOutputVerbosity::Compact {
        let mut consumed: u16 = 0;
        for (k, &idx) in indices.iter().enumerate() {
            if is_thinking_only_assistant(&rows[idx]) && !extras.render_thinking_only_rows {
                continue;
            }
            if consumed >= area.height {
                break;
            }
            let sub = Rect {
                x: area.x,
                y: area.y.saturating_add(consumed),
                width: area.width,
                height: area.height.saturating_sub(consumed),
            };
            let margin_here = if k == 0 { add_margin } else { true };
            consumed = consumed.saturating_add(render_message_inner_with_context(
                &rows[idx],
                sub,
                buf,
                theme,
                verbosity,
                margin_here,
                last_thinking_block_id,
                true,
                extras,
            ));
        }
        return consumed;
    }

    let mut tools = Vec::new();
    for &idx in indices {
        if let Some(row_tools) = async_agent_launch_only_assistant_tools(&rows[idx]) {
            tools.extend(row_tools);
        }
    }
    if tools.is_empty() {
        return 0;
    }

    let mut consumed: u16 = 0;
    if add_margin {
        consumed = consumed.saturating_add(1);
        if consumed >= area.height {
            return consumed;
        }
    }

    let any_failed = tools.iter().any(|tool| {
        extras
            .live_agent_tool_activity
            .get(&tool.id)
            .is_some_and(|activity| activity.status == LiveAgentToolStatus::Failed)
    });
    let summary = format!(
        "{} launched {}",
        pluralize_count(tools.len(), "background agent", "background agents"),
        format_shortcut_hint("↓", "manage", true, false).plain_text
    );
    let header_area = Rect {
        x: area.x,
        y: area.y.saturating_add(consumed),
        width: area.width,
        height: area.height.saturating_sub(consumed),
    };
    consumed = consumed.saturating_add(render_gutter_lines(
        vec![Line::from(Span::styled(summary.clone(), theme.streaming))],
        &summary,
        tool_gutter_glyph(theme, any_failed, false),
        tool_gutter_style(theme, any_failed, false),
        header_area,
        buf,
    ));

    let content_width = area.width.saturating_sub(GUTTER).max(1) as usize;
    for (idx, tool) in tools.iter().enumerate() {
        if consumed >= area.height {
            break;
        }
        let (line_text, line) = agent_group_detail_line(tool, extras, theme, content_width, now_ms);
        let prefix = if idx + 1 == tools.len() { "└" } else { "├" };
        let sub = Rect {
            x: area.x,
            y: area.y.saturating_add(consumed),
            width: area.width,
            height: area.height.saturating_sub(consumed),
        };
        consumed = consumed.saturating_add(render_gutter_lines(
            vec![line],
            &line_text,
            prefix,
            theme.assistant_prefix,
            sub,
            buf,
        ));

        if consumed >= area.height {
            break;
        }
        let Some(activity) = agent_group_activity_line(tool, extras) else {
            continue;
        };
        let activity_line = format!("⎿  {activity}");
        let activity_sub = Rect {
            x: area.x,
            y: area.y.saturating_add(consumed),
            width: area.width,
            height: area.height.saturating_sub(consumed),
        };
        consumed = consumed.saturating_add(render_gutter_lines(
            vec![Line::from(Span::styled(
                activity_line.clone(),
                theme.assistant_prefix,
            ))],
            &activity_line,
            if idx + 1 == tools.len() { " " } else { "│" },
            theme.assistant_prefix,
            activity_sub,
            buf,
        ));
    }

    consumed
}

fn pluralize_count(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

fn agent_group_detail_line(
    tool: &crate::message::AssistantToolUseBlock,
    extras: TranscriptRenderExtras<'_>,
    theme: &RenderTheme,
    content_width: usize,
    now_ms: u64,
) -> (String, Line<'static>) {
    let input_object = tool.input.as_object();
    let input = input_object.map(|map| {
        map.iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>()
    });
    let output = tool.raw_output.as_ref().and_then(Value::as_object);
    let activity = extras.live_agent_tool_activity.get(&tool.id);
    let display_name =
        non_empty_str(activity.and_then(|activity| activity.display_name.as_deref()))
            .or_else(|| {
                non_empty_json_str(output.and_then(|output| {
                    output
                        .get("display_name")
                        .or_else(|| output.get("displayName"))
                }))
            })
            .or_else(|| non_empty_json_str(input_object.and_then(|input| input.get("name"))))
            .or_else(|| {
                input_object
                    .and_then(|input| input.get("metadata"))
                    .and_then(Value::as_object)
                    .and_then(|metadata| {
                        non_empty_json_str(metadata.get("display_name"))
                            .or_else(|| non_empty_json_str(metadata.get("displayName")))
                    })
            });
    let agent_type = non_empty_json_str(
        output.and_then(|output| output.get("agent_type").or_else(|| output.get("agentType"))),
    )
    .or_else(|| {
        non_empty_json_str(input_object.and_then(|input| {
            input
                .get("subagent_type")
                .or_else(|| input.get("subagentType"))
        }))
    })
    .filter(|agent_type| *agent_type != "general-purpose")
    .unwrap_or("Agent");
    let fallback_name = streaming_tool_display_name("Agent", input.as_ref()).to_string();
    let title = activity
        .and_then(|activity| activity.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(single_line)
        .or_else(|| agent_description(input.as_ref()).map(|description| single_line(&description)))
        .unwrap_or_else(|| "background agent".to_string());
    let (name, details) = display_name
        .map(str::trim)
        .map(|name| name.trim_start_matches('@'))
        .filter(|name| !name.is_empty())
        .map(|name| {
            (
                format!("@{}", single_line(name)),
                format!(" ({})", single_line(agent_type)),
            )
        })
        .unwrap_or_else(|| (fallback_name, format!("({title})")));
    let metrics = agent_group_metrics(activity, now_ms);

    agent_group_detail_layout(name, details, metrics, content_width, theme)
}

fn agent_group_elapsed(activity: &LiveAgentToolActivity, now_ms: u64) -> Option<String> {
    let elapsed_ms = if matches!(
        activity.status,
        LiveAgentToolStatus::Completed
            | LiveAgentToolStatus::Failed
            | LiveAgentToolStatus::Cancelled
    ) {
        activity
            .terminal_result
            .as_ref()
            .and_then(|result| {
                result
                    .get("duration_ms")
                    .or_else(|| result.get("durationMs"))
            })
            .and_then(Value::as_u64)
            .or_else(|| {
                activity
                    .start_time_ms
                    .filter(|start_time_ms| *start_time_ms > 0)
                    .zip(activity.end_time_ms)
                    .map(|(start_time_ms, end_time_ms)| end_time_ms.saturating_sub(start_time_ms))
            })
    } else {
        activity
            .start_time_ms
            .filter(|start_time_ms| *start_time_ms > 0)
            .map(|start_time_ms| now_ms.saturating_sub(start_time_ms))
    }?;

    Some(rebon_shell::format_duration(
        elapsed_ms,
        rebon_shell::DurationFormatOptions {
            hide_trailing_zeros: true,
            most_significant_only: false,
        },
    ))
}

fn agent_group_metrics(activity: Option<&LiveAgentToolActivity>, now_ms: u64) -> Option<String> {
    let activity = activity?;
    let mut parts = Vec::new();
    if let Some(elapsed) = agent_group_elapsed(activity, now_ms) {
        parts.push(elapsed);
    }
    if let Some(tokens) = activity.token_count {
        parts.push(format!("↓ {}", format_agent_token_count(tokens)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

pub(super) fn agent_group_running_elapsed_signature(
    rows: &[Message],
    segment: &TranscriptSegment,
    width: u16,
    extras: TranscriptRenderExtras<'_>,
    now_ms: u64,
) -> Option<u64> {
    if extras.static_agent_group_status {
        return None;
    }
    if let TranscriptSegment::ThinkingGroup { segments } = segment {
        let mut hasher = DefaultHasher::new();
        let mut has_visible_elapsed = false;
        for (idx, child) in segments.iter().enumerate() {
            let Some(signature) =
                agent_group_running_elapsed_signature(rows, child, width, extras, now_ms)
            else {
                continue;
            };
            idx.hash(&mut hasher);
            signature.hash(&mut hasher);
            has_visible_elapsed = true;
        }
        return has_visible_elapsed.then(|| hasher.finish());
    }
    let TranscriptSegment::AgentGroup { indices } = segment else {
        return None;
    };

    let content_width = width.saturating_sub(GUTTER).max(1) as usize;
    let mut hasher = DefaultHasher::new();
    let mut has_visible_elapsed = false;
    for tool in indices
        .iter()
        .filter_map(|idx| {
            rows.get(*idx)
                .and_then(async_agent_launch_only_assistant_tools)
        })
        .flatten()
    {
        let Some(activity) = extras.live_agent_tool_activity.get(&tool.id) else {
            continue;
        };
        if !matches!(
            activity.status,
            LiveAgentToolStatus::Running | LiveAgentToolStatus::Unknown
        ) || !activity.start_time_ms.is_some_and(|start| start > 0)
        {
            continue;
        }
        let Some(metrics) = agent_group_metrics(Some(activity), now_ms) else {
            continue;
        };
        if WidthStr::width(metrics.as_str()).saturating_add(4) > content_width {
            continue;
        }
        let Some(elapsed) = agent_group_elapsed(activity, now_ms) else {
            continue;
        };
        tool.id.hash(&mut hasher);
        elapsed.hash(&mut hasher);
        has_visible_elapsed = true;
    }

    has_visible_elapsed.then(|| hasher.finish())
}

fn agent_group_detail_layout(
    name: String,
    details: String,
    metrics: Option<String>,
    content_width: usize,
    theme: &RenderTheme,
) -> (String, Line<'static>) {
    let mut metrics = metrics.unwrap_or_default();
    if !metrics.is_empty() && WidthStr::width(metrics.as_str()).saturating_add(4) > content_width {
        metrics.clear();
    }
    let metrics_width = WidthStr::width(metrics.as_str());
    let left_budget =
        content_width.saturating_sub(metrics_width + usize::from(!metrics.is_empty()));
    let name_width = WidthStr::width(name.as_str());
    let details_width = WidthStr::width(details.as_str());
    let (rendered_name, rendered_details) = if name_width + details_width <= left_budget {
        (name, details)
    } else if details_width + 2 <= left_budget {
        (
            truncate_display_width(&name, left_budget.saturating_sub(details_width)),
            details,
        )
    } else {
        (
            truncate_display_width(&(name + &details), left_budget),
            String::new(),
        )
    };
    let left_width =
        WidthStr::width(rendered_name.as_str()) + WidthStr::width(rendered_details.as_str());
    let spacer_width = if metrics.is_empty() {
        0
    } else {
        content_width.saturating_sub(left_width + metrics_width)
    };
    let spacer = " ".repeat(spacer_width);
    let text = format!("{rendered_name}{rendered_details}{spacer}{metrics}");
    let mut spans = vec![Span::styled(
        rendered_name,
        theme.tool_header.add_modifier(Modifier::BOLD),
    )];
    if !rendered_details.is_empty() {
        spans.push(Span::styled(rendered_details, theme.assistant_prefix));
    }
    if !spacer.is_empty() {
        spans.push(Span::raw(spacer));
    }
    if !metrics.is_empty() {
        spans.push(Span::styled(metrics, theme.assistant_prefix));
    }
    (text, Line::from(spans))
}

fn truncate_display_width(text: &str, max_width: usize) -> String {
    if WidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let target_width = max_width - 1;
    let mut width = 0;
    let mut truncated = String::new();
    for ch in text.chars() {
        let ch_width = rebon_width::char_width(ch).unwrap_or(0);
        if width + ch_width > target_width {
            break;
        }
        width += ch_width;
        truncated.push(ch);
    }
    truncated.push('…');
    truncated
}

fn non_empty_json_str(value: Option<&Value>) -> Option<&str> {
    non_empty_str(value.and_then(Value::as_str))
}

fn non_empty_str(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn single_line(value: &str) -> String {
    value.replace('\r', " ").replace('\n', " ")
}

fn agent_group_activity_line(
    tool: &crate::message::AssistantToolUseBlock,
    extras: TranscriptRenderExtras<'_>,
) -> Option<String> {
    let activity = extras.live_agent_tool_activity.get(&tool.id);
    if let Some(text) = activity
        .and_then(|activity| activity.text.as_deref())
        .filter(|text| !text.trim().is_empty())
    {
        return Some(text.to_string());
    }

    match activity.map(|activity| activity.status) {
        Some(LiveAgentToolStatus::Completed) => Some("Done".to_string()),
        Some(LiveAgentToolStatus::Failed) => Some("Failed".to_string()),
        Some(LiveAgentToolStatus::Cancelled) => Some("Stopped".to_string()),
        Some(LiveAgentToolStatus::Running) => Some("Running".to_string()),
        Some(LiveAgentToolStatus::Unknown) | None if extras.static_agent_group_status => None,
        Some(LiveAgentToolStatus::Unknown) | None => Some("Launched".to_string()),
    }
}

/// Paint a run of `>= 2` collapsed committed tool uses as one summary
/// block — past-tense verbs ("Searched for N patterns, read M files"),
/// dimmed, with a `(ctrl+o to expand)` affordance in compact mode.
///
/// Settled groups keep the collapsed read/search summary rendering.
pub(super) fn render_committed_collapsed_group(
    rows: &[Message],
    indices: &[usize],
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    show_hint_lines: bool,
) -> u16 {
    render_committed_collapsed_group_with_expand_hint(
        rows,
        indices,
        area,
        buf,
        theme,
        verbosity,
        add_margin,
        show_hint_lines,
        true,
    )
}

fn render_committed_collapsed_group_with_expand_hint(
    rows: &[Message],
    indices: &[usize],
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    show_hint_lines: bool,
    show_expand_hint: bool,
) -> u16 {
    if area.height == 0 || area.width == 0 || indices.is_empty() {
        return 0;
    }

    // Optional leading blank line — matches the 1-row margin that the
    // per-message widget would have inserted if this group were a
    // single rendered message, so group boundaries line up with the
    // rest of the transcript's vertical rhythm.
    let mut consumed: u16 = 0;
    if add_margin {
        consumed = consumed.saturating_add(1);
        if consumed >= area.height {
            return consumed;
        }
    }

    let Some((mut display, any_failed)) = committed_collapsed_group_display(rows, indices) else {
        return consumed;
    };
    display.show_expand_hint &= show_expand_hint;

    render_collapsed_read_search_display(
        &display,
        Rect {
            x: area.x,
            y: area.y.saturating_add(consumed),
            width: area.width,
            height: area.height.saturating_sub(consumed),
        },
        buf,
        theme,
        verbosity,
        show_hint_lines,
        any_failed,
        false,
    )
    .saturating_add(consumed)
}

pub(super) fn measure_committed_collapsed_group_height(
    rows: &[Message],
    indices: &[usize],
    width: u16,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    show_hint_lines: bool,
    show_expand_hint: bool,
) -> u16 {
    if width == 0 || indices.is_empty() {
        return 0;
    }
    let mut consumed = u16::from(add_margin);
    let Some((mut display, _)) = committed_collapsed_group_display(rows, indices) else {
        return consumed;
    };
    display.show_expand_hint &= show_expand_hint;
    consumed = consumed.saturating_add(measure_collapsed_read_search_display_height(
        &display,
        width,
        verbosity,
        show_hint_lines,
    ));
    consumed
}

fn committed_collapsed_group_display(
    rows: &[Message],
    indices: &[usize],
) -> Option<(CollapsedReadSearchDisplay, bool)> {
    // First pass: build a map tool_use_id → is_error so we can mark
    // individual tools as Errored without having to re-scan rows for
    // each call.
    let mut errored_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for &i in indices {
        if let Some(results) = tool_result_only_user(&rows[i]) {
            for r in results {
                if r.is_error == Some(true) {
                    errored_ids.insert(r.tool_use_id.clone());
                }
            }
        }
    }

    // Second pass: push every collapsible tool_use into the aggregator
    // and record its lifecycle result. Any tool_use without a matching
    // tool_result is treated as resolved — this is the committed path,
    // so every call we see has already left the wire.
    let policy = MemoryPathPolicy::default();
    let options = ClassifyOptions::default();
    let mut agg = Aggregator::new();
    let mut any_failed = false;
    for &i in indices {
        if let Some(tools) = tool_only_assistant(&rows[i]) {
            for t in tools {
                let class = classify_tool_use(&t.name, &t.input, &policy, options);
                agg.push_tool_use(&t.id, &class, 1);
                if errored_ids.contains(&t.id) {
                    agg.record_result(&t.id, ResultStatus::Errored);
                    any_failed = true;
                } else {
                    agg.record_result(&t.id, ResultStatus::Resolved);
                }
            }
        }
    }

    let input = agg.finalize(FinalizeParams {
        previous_counts: Default::default(),
        // Past-tense + dimmed + static gutter — the whole point of the
        // committed collapse path is that it summarises completed work.
        is_active_group: false,
        verbose: false,
        should_animate: false,
        fullscreen_enabled: options.fullscreen,
        background: None,
    });
    let output = project_collapsed_read_search(&input);
    match output.projection {
        CollapsedReadSearchProjection::Summary(display) => Some((display, any_failed)),
        _ => None,
    }
}

pub(super) fn render_committed_thinking_group(
    rows: &[Message],
    segments: &[TranscriptSegment],
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    show_collapsed_hint_lines: bool,
    extras: TranscriptRenderExtras<'_>,
    now_ms: u64,
) -> u16 {
    let mut thinking_blocks = Vec::new();
    for segment in segments {
        collect_transcript_segment_thinking(rows, segment, &mut thinking_blocks);
    }
    if thinking_blocks.len() < 2 || area.height == 0 || area.width == 0 {
        return 0;
    }

    let mut consumed = u16::from(add_margin);
    if consumed >= area.height {
        return consumed;
    }

    let compact = verbosity == ToolOutputVerbosity::Compact && !extras.expand_thinking_rows;
    let header_area = Rect {
        x: area.x,
        y: area.y.saturating_add(consumed),
        width: area.width,
        height: area.height.saturating_sub(consumed),
    };
    consumed = consumed.saturating_add(render_thinking_group_header(
        thinking_blocks.len(),
        compact,
        header_area,
        buf,
        theme,
    ));

    for (idx, thinking) in thinking_blocks.iter().enumerate() {
        if consumed >= area.height {
            return consumed;
        }
        let source = if compact {
            thinking
                .trim()
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("")
        } else {
            thinking.trim()
        };
        if source.is_empty() {
            continue;
        }
        let sub = Rect {
            x: area.x,
            y: area.y.saturating_add(consumed),
            width: area.width,
            height: area.height.saturating_sub(consumed),
        };
        consumed = consumed.saturating_add(render_grouped_thinking_block(
            source,
            if idx + 1 == thinking_blocks.len() {
                "└"
            } else {
                "├"
            },
            sub,
            buf,
            theme,
        ));
    }

    let latest_collapsed_segment = show_collapsed_hint_lines
        .then(|| {
            segments
                .iter()
                .rposition(transcript_segment_contains_collapsed)
        })
        .flatten();
    for (segment_idx, segment) in segments.iter().enumerate() {
        if matches!(segment, TranscriptSegment::Single(idx) if is_thinking_only_assistant(&rows[*idx]))
        {
            continue;
        }
        if consumed >= area.height {
            break;
        }
        let sub = Rect {
            x: area.x,
            y: area.y.saturating_add(consumed),
            width: area.width,
            height: area.height.saturating_sub(consumed),
        };
        let child_show_collapsed_hint_lines = latest_collapsed_segment == Some(segment_idx);
        let used = match segment {
            TranscriptSegment::Collapsed { indices }
                if verbosity == ToolOutputVerbosity::Compact =>
            {
                render_committed_collapsed_group_with_expand_hint(
                    rows,
                    indices,
                    sub,
                    buf,
                    theme,
                    verbosity,
                    true,
                    child_show_collapsed_hint_lines,
                    false,
                )
            }
            _ => render_segment(
                rows,
                segment,
                sub,
                buf,
                theme,
                verbosity,
                true,
                child_show_collapsed_hint_lines,
                Some(THINKING_GROUP_SUPPRESSION_ID),
                extras,
                now_ms,
            ),
        };
        consumed = consumed.saturating_add(used);
    }

    consumed
}

pub(super) fn measure_committed_thinking_group_height(
    rows: &[Message],
    row_revisions: &[u64],
    segments: &[TranscriptSegment],
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    cache: &mut TranscriptMeasureCache,
    show_collapsed_hint_lines: bool,
    extras: TranscriptRenderExtras<'_>,
) -> u16 {
    let mut thinking_blocks = Vec::new();
    for segment in segments {
        collect_transcript_segment_thinking(rows, segment, &mut thinking_blocks);
    }
    if thinking_blocks.len() < 2 || area.width == 0 {
        return 0;
    }

    let compact = verbosity == ToolOutputVerbosity::Compact && !extras.expand_thinking_rows;
    let mut consumed = u16::from(add_margin);
    consumed = consumed.saturating_add(measure_thinking_group_header_height(
        thinking_blocks.len(),
        compact,
        area.width,
    ));
    for thinking in thinking_blocks {
        let source = if compact {
            thinking
                .trim()
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("")
        } else {
            thinking.trim()
        };
        if !source.is_empty() {
            consumed = consumed.saturating_add(measure_grouped_thinking_block_height(
                source, area.width, theme,
            ));
        }
    }

    let latest_collapsed_segment = show_collapsed_hint_lines
        .then(|| {
            segments
                .iter()
                .rposition(transcript_segment_contains_collapsed)
        })
        .flatten();
    for (segment_idx, segment) in segments.iter().enumerate() {
        if matches!(segment, TranscriptSegment::Single(idx) if is_thinking_only_assistant(&rows[*idx]))
        {
            continue;
        }
        let child_show_collapsed_hint_lines = latest_collapsed_segment == Some(segment_idx);
        let used = match segment {
            TranscriptSegment::Collapsed { indices }
                if verbosity == ToolOutputVerbosity::Compact =>
            {
                measure_committed_collapsed_group_height(
                    rows,
                    indices,
                    area.width,
                    verbosity,
                    true,
                    child_show_collapsed_hint_lines,
                    false,
                )
            }
            _ => measure_segment_height(
                rows,
                row_revisions,
                segment,
                area,
                buf,
                theme,
                verbosity,
                true,
                cache,
                child_show_collapsed_hint_lines,
                Some(THINKING_GROUP_SUPPRESSION_ID),
                extras,
            ),
        };
        consumed = consumed.saturating_add(used);
    }

    consumed
}

fn collect_transcript_segment_thinking<'a>(
    rows: &'a [Message],
    segment: &TranscriptSegment,
    thinking_blocks: &mut Vec<&'a str>,
) {
    match segment {
        TranscriptSegment::Single(idx) => collect_message_thinking(&rows[*idx], thinking_blocks),
        TranscriptSegment::Collapsed { indices } | TranscriptSegment::AgentGroup { indices } => {
            for idx in indices {
                collect_message_thinking(&rows[*idx], thinking_blocks);
            }
        }
        TranscriptSegment::ThinkingGroup { segments } => {
            for segment in segments {
                collect_transcript_segment_thinking(rows, segment, thinking_blocks);
            }
        }
    }
}

fn collect_message_thinking<'a>(message: &'a Message, thinking_blocks: &mut Vec<&'a str>) {
    let Message::Assistant(assistant) = message else {
        return;
    };
    for block in &assistant.message.content {
        if let AssistantContentBlock::Thinking(thinking) = block {
            if !thinking.thinking.trim().is_empty() {
                thinking_blocks.push(&thinking.thinking);
            }
        }
    }
}

pub(super) fn render_thinking_group_header(
    thinking_count: usize,
    show_expand_hint: bool,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
) -> u16 {
    let header = format!("Reasoning ({thinking_count} steps)");
    let hint = show_expand_hint.then(|| {
        format!(
            "({})",
            format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
        )
    });
    let mut spans = vec![Span::styled(header.clone(), theme.thinking)];
    if let Some(hint) = hint.as_deref() {
        spans.push(Span::styled(format!(" {hint}"), theme.thinking));
    }
    let plain_text = match hint {
        Some(hint) => format!("{header} {hint}"),
        None => header,
    };
    render_gutter_lines(
        vec![Line::from(spans)],
        &plain_text,
        "·",
        theme.thinking,
        area,
        buf,
    )
}

pub(super) fn measure_thinking_group_header_height(
    thinking_count: usize,
    show_expand_hint: bool,
    width: u16,
) -> u16 {
    if width == 0 {
        return 0;
    }
    let header = format!("Reasoning ({thinking_count} steps)");
    let plain_text = if show_expand_hint {
        format!(
            "{header} ({})",
            format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
        )
    } else {
        header
    };
    wrap_height_visible(&plain_text, width.saturating_sub(GUTTER).max(1)).max(1)
}

pub(super) fn render_grouped_thinking_block(
    source: &str,
    gutter: &str,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
) -> u16 {
    let msg_theme = rebon_message_tui::MessagesRenderTheme {
        text: theme.thinking,
        dim: theme.thinking,
        error: theme.system_error,
        warning: theme.thinking,
        accent: theme.thinking.add_modifier(ratatui::style::Modifier::BOLD),
    };
    let md_theme = rebon_message_tui::MarkdownTheme::from_messages(&msg_theme);
    let content_width = area.width.saturating_sub(GUTTER).max(1);
    let mut annotated = rebon_message_tui::render_markdown_blocks_annotated_with_width_and_options(
        source,
        &md_theme,
        content_width as usize,
        markdown_render_options(theme.math_display),
    );
    let rendered = std::mem::take(&mut annotated.text);
    let plain_text = rendered
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let used = render_gutter_lines(
        rendered.lines.clone(),
        &plain_text,
        gutter,
        theme.assistant_prefix,
        area,
        buf,
    );
    if used > 0 && (!annotated.hyperlinks.is_empty() || !annotated.formulas.is_empty()) {
        let layers = [rebon_message_tui::HyperlinkPaintLayer {
            area: Rect::new(area.x.saturating_add(GUTTER), area.y, content_width, used),
            text: rendered,
            hyperlinks: annotated.hyperlinks,
            formulas: annotated.formulas,
        }];
        if theme.supports_hyperlinks {
            apply_markdown_hyperlink_layers(&layers, buf);
        }
        apply_math_image_layers(&layers, theme, buf);
    }
    used
}

pub(super) fn measure_grouped_thinking_block_height(
    source: &str,
    width: u16,
    theme: &RenderTheme,
) -> u16 {
    if width == 0 || source.is_empty() {
        return 0;
    }
    let msg_theme = rebon_message_tui::MessagesRenderTheme {
        text: theme.thinking,
        dim: theme.thinking,
        error: theme.system_error,
        warning: theme.thinking,
        accent: theme.thinking.add_modifier(ratatui::style::Modifier::BOLD),
    };
    let md_theme = rebon_message_tui::MarkdownTheme::from_messages(&msg_theme);
    let content_width = width.saturating_sub(GUTTER).max(1);
    let rendered = rebon_message_tui::render_markdown_blocks_annotated_with_width_and_options(
        source,
        &md_theme,
        content_width as usize,
        markdown_render_options(theme.math_display),
    )
    .text;
    let plain_text = rendered
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    wrap_height_visible(&plain_text, content_width).max(1)
}

pub(super) fn latest_committed_thinking_block(rows: &[Message]) -> Option<String> {
    for row in rows.iter().rev() {
        if is_meta_user(row) {
            continue;
        }
        if message_has_non_tool_user_content(row) {
            return None;
        }
        if let Message::Assistant(a) = row {
            for (idx, block) in a.message.content.iter().enumerate().rev() {
                if let AssistantContentBlock::Thinking(t) = block {
                    if !t.thinking.trim().is_empty() {
                        return Some(format!("{}:{idx}", a.uuid));
                    }
                }
            }
        }
    }
    None
}

pub(super) fn find_latest_visible_thinking_block_id(
    rows: &[Message],
    overlay: &StreamingOverlay,
) -> Option<String> {
    let overlay_segments = build_stream_segments(overlay);
    if overlay_segments
        .iter()
        .rposition(stream_segment_has_visible_output)
        .is_some_and(|idx| matches!(overlay_segments.get(idx), Some(StreamSegment::Thinking(_))))
    {
        return Some("streaming".into());
    }

    if overlay.is_empty() {
        if let Some(thinking) = latest_committed_thinking_block(rows) {
            return Some(thinking);
        }
    }

    for row in rows.iter().rev() {
        if message_has_non_tool_user_content(row) {
            return Some("no-thinking".into());
        }
        if let Message::Assistant(a) = row {
            for (idx, block) in a.message.content.iter().enumerate().rev() {
                if let AssistantContentBlock::Thinking(t) = block {
                    if !t.thinking.trim().is_empty() {
                        return Some(format!("{}:{idx}", a.uuid));
                    }
                }
            }
        }
    }
    None
}

/// Render a [`TranscriptSegment`] into `area`.
///
/// - `Single` → `render_message_inner`.
/// - `Collapsed` in `Compact` → `render_committed_collapsed_group`
///   (one-line past-tense summary with `(ctrl+o to expand)`).
/// - `Collapsed` in `Normal` / `Verbose` → the summary's own promise.
///   The expand hint is a contract: flipping verbosity via Ctrl+O
///   must actually surface the underlying rows. So bypass the
///   aggregator and paint every index in the run back-to-back via
///   `render_message_inner`, with normal inter-message margins, so
///   the user sees each per-tool card (and any text/thinking
///   preamble that was hidden by the summary).
///
/// Returns the height consumed.
pub(super) fn render_segment(
    rows: &[Message],
    segment: &TranscriptSegment,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    show_collapsed_hint_lines: bool,
    last_thinking_block_id: Option<&str>,
    extras: TranscriptRenderExtras<'_>,
    now_ms: u64,
) -> u16 {
    match segment {
        TranscriptSegment::Single(i) => render_message_inner_with_context(
            &rows[*i],
            area,
            buf,
            theme,
            verbosity,
            add_margin,
            last_thinking_block_id,
            true,
            extras,
        ),
        TranscriptSegment::AgentGroup { indices } => render_committed_agent_group(
            rows,
            indices,
            area,
            buf,
            theme,
            verbosity,
            add_margin,
            last_thinking_block_id,
            extras,
            now_ms,
        ),
        TranscriptSegment::ThinkingGroup { segments } => render_committed_thinking_group(
            rows,
            segments,
            area,
            buf,
            theme,
            verbosity,
            add_margin,
            show_collapsed_hint_lines,
            extras,
            now_ms,
        ),
        TranscriptSegment::Collapsed { indices } if verbosity != ToolOutputVerbosity::Compact => {
            let mut consumed: u16 = 0;
            for (k, &idx) in indices.iter().enumerate() {
                if consumed >= area.height {
                    break;
                }
                let sub = Rect {
                    x: area.x,
                    y: area.y.saturating_add(consumed),
                    width: area.width,
                    height: area.height.saturating_sub(consumed),
                };
                // First child inherits the caller's intent; later
                // children always get the standard 1-row margin
                // between committed messages.
                let margin_here = if k == 0 { add_margin } else { true };
                consumed = consumed.saturating_add(render_message_inner_with_context(
                    &rows[idx],
                    sub,
                    buf,
                    theme,
                    verbosity,
                    margin_here,
                    last_thinking_block_id,
                    true,
                    extras,
                ));
            }
            consumed
        }
        TranscriptSegment::Collapsed { indices } => render_committed_collapsed_group(
            rows,
            indices,
            area,
            buf,
            theme,
            verbosity,
            add_margin,
            show_collapsed_hint_lines,
        ),
    }
}

pub(super) fn measure_segment_height(
    rows: &[Message],
    row_revisions: &[u64],
    segment: &TranscriptSegment,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    cache: &mut TranscriptMeasureCache,
    show_collapsed_hint_lines: bool,
    last_thinking_block_id: Option<&str>,
    extras: TranscriptRenderExtras<'_>,
) -> u16 {
    let now_ms = wall_clock_ms();
    match segment {
        TranscriptSegment::Single(i) => measure_message_height(
            rows,
            row_revisions,
            *i,
            area,
            buf,
            theme,
            verbosity,
            add_margin,
            cache,
            last_thinking_block_id,
            extras,
        ),
        TranscriptSegment::AgentGroup { indices } => render_committed_agent_group(
            rows,
            indices,
            area,
            buf,
            theme,
            verbosity,
            add_margin,
            last_thinking_block_id,
            extras,
            now_ms,
        ),
        TranscriptSegment::ThinkingGroup { segments } => measure_committed_thinking_group_height(
            rows,
            row_revisions,
            segments,
            area,
            buf,
            theme,
            verbosity,
            add_margin,
            cache,
            show_collapsed_hint_lines,
            extras,
        ),
        TranscriptSegment::Collapsed { indices } if verbosity != ToolOutputVerbosity::Compact => {
            let mut consumed: u16 = 0;
            for (k, &idx) in indices.iter().enumerate() {
                let margin_here = if k == 0 { add_margin } else { true };
                let used = measure_message_height(
                    rows,
                    row_revisions,
                    idx,
                    area,
                    buf,
                    theme,
                    verbosity,
                    margin_here,
                    cache,
                    last_thinking_block_id,
                    extras,
                );
                consumed = consumed.saturating_add(used);
            }
            consumed
        }
        TranscriptSegment::Collapsed { indices } => {
            let key = CollapsedMeasureKey::new(
                rows,
                indices,
                row_revisions,
                area.width,
                show_collapsed_hint_lines,
                add_margin,
            );
            if let Some(height) = cache.collapsed_group_heights.get(&key) {
                return *height;
            }
            let height = measure_committed_collapsed_group_height(
                rows,
                indices,
                area.width,
                verbosity,
                add_margin,
                show_collapsed_hint_lines,
                true,
            );
            cache.collapsed_group_heights.insert(key, height);
            height
        }
    }
}

#[cfg(test)]
mod agent_group_detail_tests {
    use super::*;

    fn running_activity() -> LiveAgentToolActivity {
        LiveAgentToolActivity {
            text: None,
            status: LiveAgentToolStatus::Running,
            title: None,
            display_name: Some("explore-engine".to_string()),
            start_time_ms: Some(1_000),
            end_time_ms: None,
            tool_use_count: Some(4),
            token_count: Some(30_600),
            terminal_result: None,
        }
    }

    fn agent_tool(input: Value) -> crate::message::AssistantToolUseBlock {
        crate::message::AssistantToolUseBlock {
            id: "toolu_agent".to_string(),
            name: "Agent".to_string(),
            input,
            tool_call_content: None,
            raw_output: None,
            title: None,
            locations: None,
            status: None,
        }
    }

    fn detail_text(input: Value) -> String {
        agent_group_detail_line(
            &agent_tool(input),
            TranscriptRenderExtras::empty(),
            &RenderTheme::plain(),
            120,
            10_000,
        )
        .0
    }

    #[test]
    fn multiline_description_fallback_is_normalized_before_layout() {
        let cases = [
            (
                "LF",
                "inspect first\ninspect second",
                "inspect first inspect second",
            ),
            (
                "CR",
                "inspect first\rinspect second",
                "inspect firstinspect second",
            ),
            (
                "CRLF",
                "inspect first\r\ninspect second",
                "inspect first inspect second",
            ),
        ];

        for (case, description, expected) in cases {
            let text = detail_text(serde_json::json!({"description": description}));
            assert!(!text.contains(['\r', '\n']), "case {case}: {text:?}");
            assert!(
                text.contains(&format!("({expected})")),
                "case {case}: {text:?}"
            );
        }
    }

    #[test]
    fn metadata_display_name_accepts_snake_and_camel_case() {
        let cases = [
            (
                "snake case only",
                serde_json::json!({"metadata": {"display_name": "snake-name"}}),
                "@snake-name",
            ),
            (
                "camel case only",
                serde_json::json!({"metadata": {"displayName": "camel-name"}}),
                "@camel-name",
            ),
            (
                "snake case takes precedence",
                serde_json::json!({
                    "metadata": {
                        "display_name": "snake-name",
                        "displayName": "camel-name"
                    }
                }),
                "@snake-name",
            ),
            (
                "empty snake case falls back to camel case",
                serde_json::json!({
                    "metadata": {"display_name": "", "displayName": "camel-name"}
                }),
                "@camel-name",
            ),
            (
                "null snake case falls back to camel case",
                serde_json::json!({
                    "metadata": {"display_name": null, "displayName": "camel-name"}
                }),
                "@camel-name",
            ),
        ];

        for (case, input, expected) in cases {
            let text = detail_text(input);
            assert!(text.starts_with(expected), "case {case}: {text:?}");
        }
    }

    #[test]
    fn metrics_show_elapsed_and_inbound_tokens() {
        assert_eq!(
            agent_group_metrics(Some(&running_activity()), 35_000).as_deref(),
            Some("34s · ↓ 30.6k tokens")
        );
    }

    #[test]
    fn stopped_metrics_use_snapshot_end_time_without_a_terminal_result() {
        let mut activity = running_activity();
        activity.status = LiveAgentToolStatus::Cancelled;
        activity.end_time_ms = Some(35_000);

        assert_eq!(
            agent_group_metrics(Some(&activity), 99_000).as_deref(),
            Some("34s · ↓ 30.6k tokens")
        );
    }

    #[test]
    fn detail_layout_right_aligns_metrics_and_truncates_the_name() {
        let metrics = Some("34s · ↓ 30.6k tokens".to_string());
        let (text, _) = agent_group_detail_layout(
            "@explore-engine-with-a-very-long-name".to_string(),
            " (Explore)".to_string(),
            metrics.clone(),
            48,
            &RenderTheme::plain(),
        );

        assert_eq!(WidthStr::width(text.as_str()), 48);
        assert!(text.contains('…'), "{text:?}");
        assert!(text.ends_with(metrics.as_deref().unwrap()), "{text:?}");
    }

    #[test]
    fn detail_layout_drops_metrics_before_wrapping_on_narrow_screens() {
        let (text, _) = agent_group_detail_layout(
            "@explore-engine-with-a-very-long-name".to_string(),
            " (Explore)".to_string(),
            Some("34s · ↓ 30.6k tokens".to_string()),
            20,
            &RenderTheme::plain(),
        );

        assert!(WidthStr::width(text.as_str()) <= 20, "{text:?}");
        assert!(!text.contains("tokens"), "{text:?}");
    }
}
