use super::*;

/// Segment of the streaming overlay produced by the collapse scanner.
pub(super) enum StreamSegment<'a> {
    Text(&'a str),
    Thinking(&'a StreamingThinking),
    ThinkingGroup {
        segments: Vec<StreamSegment<'a>>,
    },
    /// A single tool use — either a lone collapsible one or a
    /// non-collapsible one. Renders with the per-tool card.
    SingleTool(&'a StreamingToolUse),
    /// A run of `>= 2` consecutive collapsible tool uses that fold
    /// into the summary block in compact mode, while preserving the
    /// original absorbed streaming items for expanded replay.
    Collapsed(CollapsedStreamRun<'a>),
}

pub(super) struct CollapsedStreamRun<'a> {
    pub(super) tools: Vec<&'a StreamingToolUse>,
    pub(super) items: Vec<CollapsedStreamItem<'a>>,
}

pub(super) enum CollapsedStreamItem<'a> {
    Tool(&'a StreamingToolUse),
    Thinking(&'a StreamingThinking),
    Text(&'a str),
}

/// Walk `overlay.blocks` and produce a list of render segments where
/// every maximal run of `>= 2` consecutive collapsible tool uses
/// becomes a single [`StreamSegment::Collapsed`] segment.
///
/// Target layout (matches the user's bug-report sketch):
///
/// ```text
///   assistant text
///   · thinking (compact)
///   Searched for N patterns, read M files (ctrl+o to expand)
///   · thinking (compact)
///   Searched for P patterns (ctrl+o to expand)
///   assistant text
/// ```
///
/// Reasoning models (gpt-5.x, extended-thinking Claude) emit a fresh
/// `Thinking` block between every tool_use in the stream — `ThinkingEnd`
/// fires after each tool call, so the next delta starts a new block.
/// The stream sequence is `[Think, Tool, Think, Tool, Think, Tool]`,
/// not `[Think, Tool, Tool, Tool]`. If intervening thinking blocks
/// broke the collapse run (the old behaviour), every tool_use became
/// a lone `● Search` card and the summary never fired.
///
/// Rule: once a collapsible run starts, `Thinking` blocks and
/// whitespace-only `Text` blocks are *absorbed* into the run — they
/// don't join the tool count but they also don't break the run, and
/// they're captured in the collapsed body (surfaced via Ctrl+O) rather
/// than rendered as separate segments between the collapsed tools.
///
/// Thinking blocks BEFORE the run starts (the leading reasoning above
/// a tool group) still render as their own compact segments, matching
/// the sketched layout. Non-whitespace `Text` (genuine narration) and
/// non-collapsible tool uses still break a run so narrative dividers
/// between tool chains stay visible.
fn build_base_stream_segments<'a>(overlay: &'a StreamingOverlay) -> Vec<StreamSegment<'a>> {
    use crate::streaming::StreamingContentBlock;

    let mut segments: Vec<StreamSegment<'a>> = Vec::new();
    let mut i = 0;
    while i < overlay.blocks.len() {
        match &overlay.blocks[i] {
            StreamingContentBlock::Text(t) => {
                segments.push(StreamSegment::Text(t));
                i += 1;
            }
            StreamingContentBlock::Thinking(th) => {
                segments.push(StreamSegment::Thinking(th));
                i += 1;
            }
            StreamingContentBlock::ToolUse(tool) => {
                if classify_streaming_tool(tool).is_collapsible() {
                    // Greedily extend the run. Collapsible tool_uses
                    // join it; intervening `Thinking` and whitespace
                    // `Text` are transparent (absorbed — see docstring);
                    // everything else ends the run.
                    let mut run_tools: Vec<&'a StreamingToolUse> = vec![tool];
                    let mut run_items: Vec<CollapsedStreamItem<'a>> =
                        vec![CollapsedStreamItem::Tool(tool)];
                    let mut j = i + 1;
                    while j < overlay.blocks.len() {
                        match &overlay.blocks[j] {
                            StreamingContentBlock::ToolUse(next) => {
                                if classify_streaming_tool(next).is_collapsible() {
                                    run_tools.push(next);
                                    run_items.push(CollapsedStreamItem::Tool(next));
                                    j += 1;
                                    continue;
                                }
                                break;
                            }
                            StreamingContentBlock::Thinking(thinking) => {
                                run_items.push(CollapsedStreamItem::Thinking(thinking));
                                j += 1;
                                continue;
                            }
                            StreamingContentBlock::Text(t) if t.trim().is_empty() => {
                                run_items.push(CollapsedStreamItem::Text(t));
                                j += 1;
                                continue;
                            }
                            StreamingContentBlock::Text(_) => break,
                        }
                    }
                    if run_tools.len() >= 2 {
                        segments.push(StreamSegment::Collapsed(CollapsedStreamRun {
                            tools: run_tools,
                            items: run_items,
                        }));
                        i = j;
                    } else {
                        segments.push(StreamSegment::SingleTool(tool));
                        // A failed collapse candidate must not consume the
                        // intervening blocks we scanned speculatively. In
                        // particular, `[ToolUse, Thinking, ...]` should render
                        // the thinking normally when no second collapsible tool
                        // completes the run.
                        i += 1;
                    }
                } else {
                    segments.push(StreamSegment::SingleTool(tool));
                    i += 1;
                }
            }
        }
    }
    segments
}

pub(super) fn build_stream_segments<'a>(overlay: &'a StreamingOverlay) -> Vec<StreamSegment<'a>> {
    group_stream_thinking_segments(build_base_stream_segments(overlay))
}

fn stream_segment_thinking_count(segment: &StreamSegment<'_>) -> usize {
    match segment {
        StreamSegment::Thinking(thinking) => usize::from(!thinking.thinking.trim().is_empty()),
        StreamSegment::Collapsed(run) => run
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    CollapsedStreamItem::Thinking(thinking)
                        if !thinking.thinking.trim().is_empty()
                )
            })
            .count(),
        StreamSegment::ThinkingGroup { segments } => {
            segments.iter().map(stream_segment_thinking_count).sum()
        }
        StreamSegment::Text(_) | StreamSegment::SingleTool(_) => 0,
    }
}

fn stream_segment_breaks_thinking_group(segment: &StreamSegment<'_>) -> bool {
    match segment {
        StreamSegment::Text(text) => !text.trim().is_empty(),
        StreamSegment::SingleTool(tool) => !TRANSCRIPT_HIDDEN_TOOLS
            .iter()
            .any(|hidden| *hidden == tool.tool_name),
        StreamSegment::Thinking(_)
        | StreamSegment::ThinkingGroup { .. }
        | StreamSegment::Collapsed(_) => false,
    }
}

fn group_stream_thinking_segments<'a>(segments: Vec<StreamSegment<'a>>) -> Vec<StreamSegment<'a>> {
    let mut grouped = Vec::new();
    let mut pending = Vec::new();
    let mut thinking_count = 0usize;

    for segment in segments {
        if matches!(segment, StreamSegment::Collapsed(_)) {
            flush_pending_stream_thinking_group(&mut grouped, &mut pending, &mut thinking_count);
            grouped.push(segment);
            continue;
        }

        if stream_segment_breaks_thinking_group(&segment) {
            flush_pending_stream_thinking_group(&mut grouped, &mut pending, &mut thinking_count);
            grouped.push(segment);
            continue;
        }

        let segment_thinking_count = stream_segment_thinking_count(&segment);
        if pending.is_empty() && segment_thinking_count == 0 {
            grouped.push(segment);
            continue;
        }

        thinking_count += segment_thinking_count;
        pending.push(segment);
    }

    flush_pending_stream_thinking_group(&mut grouped, &mut pending, &mut thinking_count);
    grouped
}

fn flush_pending_stream_thinking_group<'a>(
    grouped: &mut Vec<StreamSegment<'a>>,
    pending: &mut Vec<StreamSegment<'a>>,
    thinking_count: &mut usize,
) {
    if *thinking_count >= 2 {
        grouped.push(StreamSegment::ThinkingGroup {
            segments: std::mem::take(pending),
        });
    } else {
        grouped.append(pending);
    }
    *thinking_count = 0;
}

pub(super) fn collect_stream_segment_thinking<'a>(
    segment: &'a StreamSegment<'a>,
    thinking_blocks: &mut Vec<&'a StreamingThinking>,
) {
    match segment {
        StreamSegment::Thinking(thinking) => {
            if !thinking.thinking.trim().is_empty() {
                thinking_blocks.push(thinking);
            }
        }
        StreamSegment::Collapsed(run) => {
            for item in &run.items {
                if let CollapsedStreamItem::Thinking(thinking) = item {
                    if !thinking.thinking.trim().is_empty() {
                        thinking_blocks.push(thinking);
                    }
                }
            }
        }
        StreamSegment::ThinkingGroup { segments } => {
            for segment in segments {
                collect_stream_segment_thinking(segment, thinking_blocks);
            }
        }
        StreamSegment::Text(_) | StreamSegment::SingleTool(_) => {}
    }
}

pub(super) fn stream_segment_has_visible_output(segment: &StreamSegment<'_>) -> bool {
    match segment {
        StreamSegment::Text(text) => !text.trim().is_empty(),
        StreamSegment::Thinking(thinking) => !thinking.thinking.trim().is_empty(),
        StreamSegment::ThinkingGroup { segments } => {
            segments.iter().any(stream_segment_has_visible_output)
        }
        StreamSegment::SingleTool(_) | StreamSegment::Collapsed(_) => true,
    }
}

pub(super) fn stream_segment_is_read_search(segment: &StreamSegment<'_>) -> bool {
    match segment {
        StreamSegment::Collapsed(_) => true,
        StreamSegment::SingleTool(tool) => classify_streaming_tool(tool).is_collapsible(),
        StreamSegment::ThinkingGroup { segments } => {
            segments.iter().any(stream_segment_is_read_search)
        }
        StreamSegment::Text(_) | StreamSegment::Thinking(_) => false,
    }
}

pub(super) fn single_tool_needs_trailing_expand_hint(
    tool: &StreamingToolUse,
    extras: TranscriptRenderExtras<'_>,
    content_width: u16,
) -> bool {
    if TRANSCRIPT_HIDDEN_TOOLS
        .iter()
        .any(|&hidden| hidden == tool.tool_name)
    {
        return false;
    }

    if workflow_render_tool_name(tool).is_some() {
        return false;
    }

    if matches!(tool.tool_name.as_str(), "Agent" | "Sleep") {
        return false;
    }

    if tool.kind == ToolKind::Edit {
        if tool.status == ToolCallStatus::Failed {
            return false;
        }
        if matches!(
            tool.status,
            ToolCallStatus::Pending | ToolCallStatus::InProgress
        ) && extract_diff_content(tool).is_none()
        {
            return false;
        }
    }

    let body_lines = collect_tool_body_lines(tool, None, false, extras);
    let body_line_count = visual_preview_lines(&tool.tool_name, &body_lines, content_width).len();
    body_line_count > 0 && body_line_count <= TOOL_PREVIEW_MAX_LINES
}
