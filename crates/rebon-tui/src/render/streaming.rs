use super::*;

pub(super) const THINKING_GROUP_SUPPRESSION_ID: &str = "no-thinking-group";

/// Paint the streaming overlay below the committed rows. Iterates
/// content blocks in chronological order so text segments and tool
/// calls are interleaved in the order they streamed.
///
/// Consecutive collapsible tool uses (Read/Grep/Glob/LS/MCP/…) are
/// folded into a single summary block by the read/search collapse pass.
pub fn render_streaming_overlay(
    overlay: &StreamingOverlay,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
) -> u16 {
    let mut cache = StreamingOverlayRenderCache::new();
    render_streaming_overlay_cached(
        overlay,
        area,
        buf,
        theme,
        verbosity,
        &mut cache,
        TranscriptRenderExtras::empty(),
    )
}

pub(super) fn render_streaming_overlay_cached(
    overlay: &StreamingOverlay,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    cache: &mut StreamingOverlayRenderCache,
    extras: TranscriptRenderExtras<'_>,
) -> u16 {
    render_streaming_overlay_with_options(
        overlay,
        area,
        buf,
        theme,
        verbosity,
        cache,
        StreamingOverlayRenderOptions::paint(),
        extras,
    )
}

pub(super) fn assistant_thinking_block_is_hidden(
    last_thinking_block_id: Option<&str>,
    assistant_uuid: &str,
    block_idx: usize,
) -> bool {
    match last_thinking_block_id {
        Some("no-thinking") | Some(THINKING_GROUP_SUPPRESSION_ID) => true,
        Some(latest) => latest != format!("{assistant_uuid}:{block_idx}"),
        None => false,
    }
}

pub(super) fn render_committed_assistant_tool_step(
    msg: &Message,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    last_thinking_block_id: Option<&str>,
    extras: TranscriptRenderExtras<'_>,
) -> Option<u16> {
    let Message::Assistant(assistant) = msg else {
        return None;
    };

    let mut has_visible_tool = false;
    for block in &assistant.message.content {
        match block {
            AssistantContentBlock::ToolUse(tool_use) => {
                if committed_tool_to_streaming(tool_use).is_some() {
                    has_visible_tool = true;
                }
            }
            AssistantContentBlock::Thinking(_) | AssistantContentBlock::RedactedThinking(_) => {}
            AssistantContentBlock::Text(text) if text.text.trim().is_empty() => {}
            AssistantContentBlock::Text(_)
            | AssistantContentBlock::GeneratedImage(_)
            | AssistantContentBlock::Other => return None,
        }
    }
    if !has_visible_tool {
        return None;
    }

    let mut consumed = 0u16;
    let mut rendered_any = false;
    for (block_idx, block) in assistant.message.content.iter().enumerate() {
        if consumed >= area.height {
            break;
        }
        let sub = Rect {
            x: area.x,
            y: area.y.saturating_add(consumed),
            width: area.width,
            height: area.height.saturating_sub(consumed),
        };
        let margin_here = if rendered_any { true } else { add_margin };
        let used = match block {
            AssistantContentBlock::ToolUse(tool_use) => {
                if let Some(tool_stream) =
                    committed_tool_to_streaming_with_terminal_status(tool_use, true)
                {
                    let mut tool_consumed = 0u16;
                    if margin_here {
                        tool_consumed = tool_consumed.saturating_add(1.min(sub.height));
                    }
                    if tool_consumed >= sub.height {
                        tool_consumed
                    } else {
                        let tool_sub = Rect {
                            x: sub.x,
                            y: sub.y.saturating_add(tool_consumed),
                            width: sub.width,
                            height: sub.height.saturating_sub(tool_consumed),
                        };
                        tool_consumed.saturating_add(render_streaming_tool_use_with_content(
                            &tool_stream,
                            None,
                            tool_sub,
                            buf,
                            theme,
                            verbosity,
                            extras,
                            last_thinking_block_id == Some(THINKING_GROUP_SUPPRESSION_ID),
                        ))
                    }
                } else {
                    0
                }
            }
            AssistantContentBlock::Thinking(thinking) => {
                if thinking.thinking.trim().is_empty()
                    || assistant_thinking_block_is_hidden(
                        last_thinking_block_id,
                        &assistant.uuid,
                        block_idx,
                    )
                {
                    0
                } else {
                    let thinking = StreamingThinking {
                        thinking: thinking.thinking.clone(),
                        is_streaming: false,
                        streaming_ended_at: None,
                    };
                    let margin = u16::from(margin_here).min(sub.height);
                    if margin >= sub.height {
                        margin
                    } else {
                        let thinking_sub = Rect {
                            x: sub.x,
                            y: sub.y.saturating_add(margin),
                            width: sub.width,
                            height: sub.height.saturating_sub(margin),
                        };
                        margin.saturating_add(render_streaming_thinking(
                            &thinking,
                            thinking_sub,
                            buf,
                            theme,
                            verbosity,
                            StreamingOverlayRenderMode::Paint,
                            false,
                        ))
                    }
                }
            }
            AssistantContentBlock::RedactedThinking(_) => {
                if assistant_thinking_block_is_hidden(
                    last_thinking_block_id,
                    &assistant.uuid,
                    block_idx,
                ) {
                    0
                } else {
                    let block_msg =
                        streaming_assistant_message(assistant.uuid.clone(), vec![block.clone()]);
                    render_message_inner_with_context(
                        &block_msg,
                        sub,
                        buf,
                        theme,
                        verbosity,
                        margin_here,
                        last_thinking_block_id,
                        true,
                        extras,
                    )
                }
            }
            AssistantContentBlock::Text(_) => 0,
            AssistantContentBlock::GeneratedImage(_) | AssistantContentBlock::Other => 0,
        };
        if used > 0 {
            rendered_any = true;
        }
        consumed = consumed.saturating_add(used);
    }

    Some(consumed)
}

pub(super) fn streaming_assistant_message(
    uuid: String,
    content: Vec<AssistantContentBlock>,
) -> Message {
    Message::Assistant(AssistantMessage {
        uuid,
        timestamp: String::new(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content,
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

pub(super) fn streaming_text_message(index: usize, text: &str) -> Message {
    streaming_text_message_with_continuation(index, text, false)
}

/// Like [`streaming_text_message`], with the continuation bit for the
/// live remainder of an overflow-split text block (overlay block 0 when
/// `StreamingOverlay::first_text_is_continuation` is set) so it streams
/// without a gutter dot, matching how it will commit.
pub(super) fn streaming_text_message_with_continuation(
    index: usize,
    text: &str,
    is_continuation: bool,
) -> Message {
    let Message::Assistant(mut assistant) = streaming_assistant_message(
        format!("stream-text-{index}"),
        vec![AssistantContentBlock::Text(AssistantTextBlock {
            text: text.to_string(),
        })],
    ) else {
        unreachable!("streaming_assistant_message always builds an assistant row");
    };
    assistant.is_stream_continuation = is_continuation.then_some(true);
    Message::Assistant(assistant)
}

pub(super) fn has_thinking_block(msg: &Message) -> bool {
    matches!(
        msg,
        Message::Assistant(AssistantMessage {
            message: AssistantMessageInner { content, .. },
            ..
        }) if content.iter().any(|block| matches!(block, AssistantContentBlock::Thinking(_)))
    )
}

pub(super) fn render_streaming_thinking(
    thinking: &StreamingThinking,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    mode: StreamingOverlayRenderMode,
    hide_in_compact: bool,
) -> u16 {
    if hide_in_compact || thinking.thinking.trim().is_empty() {
        return 0;
    }

    let compact = matches!(verbosity, ToolOutputVerbosity::Compact);
    let source = if compact {
        thinking
            .thinking
            .trim()
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
    } else {
        thinking.thinking.trim()
    };

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
    let mut rendered = std::mem::take(&mut annotated.text);

    if compact {
        let hint = format!(
            "({})",
            format_shortcut_hint("ctrl+o", "expand", false, false).plain_text
        );
        if let Some(first) = rendered.lines.first_mut() {
            first.spans.extend([
                Span::styled(" ", msg_theme.dim),
                Span::styled(hint, msg_theme.dim),
            ]);
        } else {
            rendered
                .lines
                .push(Line::from(Span::styled(hint, msg_theme.dim)));
        }
    }

    let plain_text: String = rendered
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

    if matches!(mode, StreamingOverlayRenderMode::Measure) {
        let h = wrap_height_visible(&plain_text, content_width);
        return h.max(1).min(area.height);
    }

    let used = render_gutter_lines(
        rendered.lines.clone(),
        &plain_text,
        "·",
        theme.thinking,
        area,
        buf,
    );
    if used > 0 && !annotated.formulas.is_empty() {
        let layers = [rebon_message_tui::HyperlinkPaintLayer {
            area: Rect::new(area.x.saturating_add(GUTTER), area.y, content_width, used),
            text: rendered,
            hyperlinks: annotated.hyperlinks,
            formulas: annotated.formulas,
        }];
        apply_math_image_layers(&layers, theme, buf);
    }
    used
}

pub(super) fn render_streaming_message(
    msg: &Message,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    mode: StreamingOverlayRenderMode,
    last_thinking_block_id: Option<&str>,
) -> u16 {
    if area.height == 0 || area.width == 0 {
        return 0;
    }
    let Some(input) = to_render_input_with_tool_state(
        msg,
        area.width,
        add_margin,
        verbosity,
        theme.frame_time_ms,
        Vec::new(),
        Vec::new(),
        last_thinking_block_id,
        false,
        TranscriptRenderExtras::empty(),
    ) else {
        return 1.min(area.height);
    };
    let mut rm_theme = MessageRenderTheme::default();
    if theme.math_display.math_enabled() {
        let palette = rebon_design_system::theme::get_theme(theme.name);
        rm_theme.assistant_text = Style::new().fg(parse_theme_color(palette.text));
    }
    let widget = RenderedMessageWidget::new_with_markdown_options(
        &input,
        &rm_theme,
        area.width,
        None,
        None,
        markdown_render_options(theme.math_display),
    );
    let h = widget.height(area.width).min(area.height);
    if matches!(mode, StreamingOverlayRenderMode::Paint) && h > 0 {
        let layers = widget.render_to_buffer_with_hyperlinks(
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: h,
            },
            buf,
        );
        if theme.supports_hyperlinks {
            apply_markdown_hyperlink_layers(&layers, buf);
        }
        apply_math_image_layers(&layers, theme, buf);
    }
    h
}

pub(super) fn collapsed_stream_item_has_visible_output(item: &CollapsedStreamItem<'_>) -> bool {
    match item {
        CollapsedStreamItem::Tool(_) => true,
        CollapsedStreamItem::Thinking(thinking) => {
            !last_non_empty_line(&thinking.thinking).is_empty()
        }
        CollapsedStreamItem::Text(text) => !text.trim().is_empty(),
    }
}

fn render_streaming_thinking_group(
    overlay: &StreamingOverlay,
    segments: &[StreamSegment<'_>],
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    mode: StreamingOverlayRenderMode,
    extras: TranscriptRenderExtras<'_>,
    show_collapsed_hint_lines: bool,
) -> u16 {
    let mut thinking_blocks = Vec::new();
    for segment in segments {
        collect_stream_segment_thinking(segment, &mut thinking_blocks);
    }
    if thinking_blocks.len() < 2 || area.height == 0 || area.width == 0 {
        return 0;
    }

    let compact = verbosity == ToolOutputVerbosity::Compact;
    let measure_only = matches!(mode, StreamingOverlayRenderMode::Measure);
    let mut consumed = if measure_only {
        measure_thinking_group_header_height(thinking_blocks.len(), compact, area.width)
    } else {
        render_thinking_group_header(thinking_blocks.len(), compact, area, buf, theme)
    };
    for (idx, thinking) in thinking_blocks.iter().enumerate() {
        if !measure_only && consumed >= area.height {
            return consumed;
        }
        let source = if compact {
            thinking
                .thinking
                .trim()
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("")
        } else {
            thinking.thinking.trim()
        };
        if source.is_empty() {
            continue;
        }
        let sub = Rect {
            x: area.x,
            y: if measure_only {
                area.y
            } else {
                area.y.saturating_add(consumed)
            },
            width: area.width,
            height: if measure_only {
                area.height
            } else {
                area.height.saturating_sub(consumed)
            },
        };
        let used = if measure_only {
            measure_grouped_thinking_block_height(source, area.width, theme)
        } else {
            render_grouped_thinking_block(
                source,
                if idx + 1 == thinking_blocks.len() {
                    "└"
                } else {
                    "├"
                },
                sub,
                buf,
                theme,
            )
        };
        consumed = consumed.saturating_add(used);
    }

    let latest_read_search_segment = show_collapsed_hint_lines
        .then(|| segments.iter().rposition(stream_segment_is_read_search))
        .flatten();
    let mut rendered_child = consumed > 0;
    for (segment_idx, segment) in segments.iter().enumerate() {
        if !stream_segment_has_non_thinking_output(segment) {
            continue;
        }
        if !measure_only && consumed >= area.height {
            break;
        }
        if rendered_child {
            consumed = consumed.saturating_add(1);
        }
        let sub = Rect {
            x: area.x,
            y: if measure_only {
                area.y
            } else {
                area.y.saturating_add(consumed)
            },
            width: area.width,
            height: if measure_only {
                area.height
            } else {
                area.height.saturating_sub(consumed)
            },
        };
        let used = render_stream_segment_without_thinking(
            overlay,
            segment,
            sub,
            buf,
            theme,
            verbosity,
            mode,
            extras,
            latest_read_search_segment == Some(segment_idx),
        );
        if used > 0 {
            rendered_child = true;
            consumed = consumed.saturating_add(used);
        }
    }

    consumed
}

fn stream_segment_has_non_thinking_output(segment: &StreamSegment<'_>) -> bool {
    match segment {
        StreamSegment::Text(text) => !text.trim().is_empty(),
        StreamSegment::Thinking(_) => false,
        StreamSegment::ThinkingGroup { segments } => {
            segments.iter().any(stream_segment_has_non_thinking_output)
        }
        StreamSegment::SingleTool(tool) => !TRANSCRIPT_HIDDEN_TOOLS
            .iter()
            .any(|hidden| *hidden == tool.tool_name),
        StreamSegment::Collapsed(_) => true,
    }
}

fn render_stream_segment_without_thinking(
    overlay: &StreamingOverlay,
    segment: &StreamSegment<'_>,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    mode: StreamingOverlayRenderMode,
    extras: TranscriptRenderExtras<'_>,
    show_collapsed_hint_lines: bool,
) -> u16 {
    match segment {
        StreamSegment::Text(text) => {
            if text.trim().is_empty() {
                0
            } else {
                render_streaming_message(
                    &streaming_text_message(0, text),
                    area,
                    buf,
                    theme,
                    verbosity,
                    false,
                    mode,
                    None,
                )
            }
        }
        StreamSegment::Thinking(_) => 0,
        StreamSegment::ThinkingGroup { segments } => render_streaming_thinking_group(
            overlay,
            segments,
            area,
            buf,
            theme,
            verbosity,
            mode,
            extras,
            show_collapsed_hint_lines,
        ),
        StreamSegment::SingleTool(tool) => render_streaming_tool_use_with_content(
            tool,
            matches!(verbosity, ToolOutputVerbosity::Verbose)
                .then(|| overlay.full_live_shell_content(&tool.call_id))
                .flatten(),
            area,
            buf,
            theme,
            verbosity,
            extras,
            true,
        ),
        StreamSegment::Collapsed(run) if verbosity == ToolOutputVerbosity::Compact => {
            render_collapsed_streaming_group(
                &run.tools,
                area,
                buf,
                theme,
                verbosity,
                show_collapsed_hint_lines,
                false,
            )
        }
        StreamSegment::Collapsed(run) => render_collapsed_stream_run_without_thinking(
            overlay, run, area, buf, theme, verbosity, mode, extras,
        ),
    }
}

fn render_collapsed_stream_run_without_thinking(
    overlay: &StreamingOverlay,
    run: &CollapsedStreamRun<'_>,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    mode: StreamingOverlayRenderMode,
    extras: TranscriptRenderExtras<'_>,
) -> u16 {
    let measure_only = matches!(mode, StreamingOverlayRenderMode::Measure);
    let mut consumed = 0u16;
    let mut rendered_item = false;
    for item in &run.items {
        if matches!(item, CollapsedStreamItem::Thinking(_))
            || matches!(item, CollapsedStreamItem::Text(text) if text.trim().is_empty())
        {
            continue;
        }
        if rendered_item {
            consumed = consumed.saturating_add(1);
        }
        let item_area = Rect {
            x: area.x,
            y: if measure_only {
                area.y
            } else {
                area.y.saturating_add(consumed)
            },
            width: area.width,
            height: if measure_only {
                area.height
            } else {
                area.height.saturating_sub(consumed)
            },
        };
        let used = match item {
            CollapsedStreamItem::Thinking(_) => unreachable!(),
            CollapsedStreamItem::Text(text) => render_streaming_message(
                &streaming_text_message(0, text),
                item_area,
                buf,
                theme,
                verbosity,
                false,
                mode,
                None,
            ),
            CollapsedStreamItem::Tool(tool) => render_streaming_tool_use_with_content(
                tool,
                matches!(verbosity, ToolOutputVerbosity::Verbose)
                    .then(|| overlay.full_live_shell_content(&tool.call_id))
                    .flatten(),
                item_area,
                buf,
                theme,
                verbosity,
                extras,
                true,
            ),
        };
        if used > 0 {
            rendered_item = true;
            consumed = consumed.saturating_add(used);
        }
    }
    consumed
}

fn render_expanded_collapsed_stream_run(
    overlay: &StreamingOverlay,
    run: &CollapsedStreamRun<'_>,
    sub: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    mode: StreamingOverlayRenderMode,
    extras: TranscriptRenderExtras<'_>,
) -> u16 {
    let measure_only = matches!(mode, StreamingOverlayRenderMode::Measure);
    let mut group_consumed: u16 = 0;
    for (item_index, item) in run.items.iter().enumerate() {
        if !measure_only && group_consumed >= sub.height {
            break;
        }
        let item_sub = if measure_only {
            Rect {
                x: sub.x,
                y: sub.y,
                width: sub.width,
                height: sub.height,
            }
        } else {
            Rect {
                x: sub.x,
                y: sub.y.saturating_add(group_consumed),
                width: sub.width,
                height: sub.height.saturating_sub(group_consumed),
            }
        };
        let used = match item {
            CollapsedStreamItem::Tool(tool) => render_streaming_tool_use_with_content(
                tool,
                matches!(verbosity, ToolOutputVerbosity::Verbose)
                    .then(|| overlay.full_live_shell_content(&tool.call_id))
                    .flatten(),
                item_sub,
                buf,
                theme,
                verbosity,
                extras,
                false,
            ),
            CollapsedStreamItem::Thinking(thinking) => {
                render_streaming_thinking(thinking, item_sub, buf, theme, verbosity, mode, false)
            }
            CollapsedStreamItem::Text(text) => {
                if text.trim().is_empty() {
                    0
                } else {
                    let msg = streaming_text_message(item_index, text);
                    render_streaming_message(
                        &msg, item_sub, buf, theme, verbosity, false, mode, None,
                    )
                }
            }
        };
        group_consumed = group_consumed.saturating_add(used);
        let next_visible = run
            .items
            .iter()
            .skip(item_index + 1)
            .any(collapsed_stream_item_has_visible_output);
        if used > 0 && next_visible && (measure_only || group_consumed < sub.height) {
            group_consumed = group_consumed.saturating_add(1);
        }
    }
    group_consumed
}

pub(super) fn render_streaming_overlay_with_options(
    overlay: &StreamingOverlay,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    cache: &mut StreamingOverlayRenderCache,
    render_options: StreamingOverlayRenderOptions,
    extras: TranscriptRenderExtras<'_>,
) -> u16 {
    if area.height == 0 || area.width == 0 {
        return 0;
    }
    let fallback_theme =
        (!render_options.native_math_graphics).then(|| theme.without_math_graphics());
    let theme = fallback_theme.as_ref().unwrap_or(theme);
    // In Measure mode the caller cares only about the natural total
    // height of the overlay — they will not actually display the
    // scratch buffer content. We therefore give every segment a fresh
    // sub-rect anchored at `area.y` with the full `area.height`, so
    // the cumulative height returned reflects the true on-screen size
    // even when the overlay would be many viewports tall. Without
    // this, `total_lines` (and the scrollback `max_offset` derived
    // from it) gets capped at the scratch buffer's height, leaving
    // late-arriving tool calls and assistant text unreachable by
    // scrolling.
    let measure_only = matches!(render_options.mode, StreamingOverlayRenderMode::Measure);
    // Leading margin is an explicit caller policy: full transcript rendering
    // enables it only when committed rows precede the live overlay, while
    // inline sliced/standalone overlays start immediately at the first row.
    //
    // Paint-mode guard: if applying the margin would starve the overlay
    // entirely (e.g. area.height==1 with leading_margin=true), drop it.
    // Rendering content jammed against the row above is strictly better
    // than rendering nothing and the BREAK warning that produced. Measure
    // mode keeps the margin so the reported natural height stays
    // consistent with a roomier paint pass.
    let mut consumed = u16::from(render_options.leading_margin);
    if !measure_only && consumed >= area.height {
        consumed = 0;
    }

    let _ = cache;
    let segments = build_stream_segments(overlay);
    let latest_read_search_segment = segments.iter().rposition(stream_segment_is_read_search);
    let has_thinking_group = segments
        .iter()
        .any(|segment| matches!(segment, StreamSegment::ThinkingGroup { .. }));
    let collapsed_inline_expand_hint = verbosity == ToolOutputVerbosity::Compact
        && latest_read_search_segment.is_some_and(|idx| {
            matches!(
                segments.get(idx),
                Some(StreamSegment::Collapsed(_) | StreamSegment::ThinkingGroup { .. })
            )
        });
    let rendered_expand_hint = collapsed_inline_expand_hint
        || (verbosity == ToolOutputVerbosity::Compact && has_thinking_group);
    let segment_count = segments.len();
    let log_this_frame = matches!(render_options.mode, StreamingOverlayRenderMode::Paint);
    let content_width = area.width.saturating_sub(GUTTER).max(1);
    let has_single_tool = segments.iter().any(|s| match s {
        StreamSegment::SingleTool(tool) => {
            single_tool_needs_trailing_expand_hint(tool, extras, content_width)
        }
        _ => false,
    });
    if log_this_frame {
        tracing::info!(
            target: "stream_dbg",
            area_w = area.width,
            area_h = area.height,
            segment_count,
            overlay_blocks = overlay.blocks.len(),
            "render_streaming_overlay: begin"
        );
    }
    for (i, segment) in segments.iter().enumerate() {
        let sub = if measure_only {
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: area.height,
            }
        } else {
            Rect {
                x: area.x,
                y: area.y.saturating_add(consumed),
                width: area.width,
                height: area.height.saturating_sub(consumed),
            }
        };
        if !measure_only && sub.height == 0 {
            if log_this_frame {
                // After the leading-margin starvation guard above, hitting
                // sub.height==0 mid-loop means an earlier segment filled
                // the paint area — the viewport is simply not tall enough
                // to show every overlay segment at once. That's expected
                // for tall transcripts; the user scrolls or the area
                // grows on the next frame. Logged at debug so it can be
                // turned on when investigating layout bugs without
                // spamming the WARN channel.
                tracing::debug!(
                    target: "stream_dbg",
                    segment_idx = i,
                    segment_count,
                    consumed,
                    area_h = area.height,
                    "render_streaming_overlay: viewport full, deferring remaining segments"
                );
            }
            break;
        }
        let used = match segment {
            StreamSegment::Text(text) => {
                if text.trim().is_empty() {
                    0
                } else {
                    let msg = streaming_text_message_with_continuation(
                        i,
                        text,
                        i == 0 && overlay.first_text_is_continuation,
                    );
                    render_streaming_message(
                        &msg,
                        sub,
                        buf,
                        theme,
                        verbosity,
                        false,
                        render_options.mode,
                        None,
                    )
                }
            }
            StreamSegment::Thinking(thinking) => render_streaming_thinking(
                thinking,
                sub,
                buf,
                theme,
                verbosity,
                render_options.mode,
                false,
            ),
            StreamSegment::ThinkingGroup { segments } => render_streaming_thinking_group(
                overlay,
                segments,
                sub,
                buf,
                theme,
                verbosity,
                render_options.mode,
                extras,
                latest_read_search_segment == Some(i),
            ),
            StreamSegment::SingleTool(tool) => render_streaming_tool_use_with_content(
                tool,
                matches!(verbosity, ToolOutputVerbosity::Verbose)
                    .then(|| overlay.full_live_shell_content(&tool.call_id))
                    .flatten(),
                sub,
                buf,
                theme,
                verbosity,
                extras,
                false,
            ),
            StreamSegment::Collapsed(run) => {
                if verbosity == ToolOutputVerbosity::Compact {
                    render_collapsed_streaming_group(
                        &run.tools,
                        sub,
                        buf,
                        theme,
                        verbosity,
                        latest_read_search_segment == Some(i),
                        true,
                    )
                } else {
                    render_expanded_collapsed_stream_run(
                        overlay,
                        run,
                        sub,
                        buf,
                        theme,
                        verbosity,
                        render_options.mode,
                        extras,
                    )
                }
            }
        };
        if log_this_frame {
            let kind = match segment {
                StreamSegment::Text(_) => "Text",
                StreamSegment::Thinking(_) => "Thinking",
                StreamSegment::ThinkingGroup { .. } => "ThinkingGroup",
                StreamSegment::SingleTool(_) => "SingleTool",
                StreamSegment::Collapsed(_) => "Collapsed",
            };
            tracing::info!(
                target: "stream_dbg",
                segment_idx = i,
                kind,
                used,
                consumed_before = consumed,
                sub_h = sub.height,
                "render_streaming_overlay: segment"
            );
        }
        consumed = consumed.saturating_add(used);
        // Add a 1-line gap between segments for visual breathing room,
        // matching the committed-message gap in render_message().
        if used > 0 && i + 1 < segment_count {
            consumed = consumed.saturating_add(1);
        }
    }

    // Show the Ctrl-O hint when there are visible single (non-collapsed)
    // tool calls in the overlay. Collapsed runs embed their own `(ctrl+o to
    // expand)` inline on the summary line, so skip the trailing hint
    // when every expandable tool segment was collapsed.
    if has_single_tool && !rendered_expand_hint && verbosity == ToolOutputVerbosity::Compact {
        let sub = if measure_only {
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: area.height,
            }
        } else {
            Rect {
                x: area.x,
                y: area.y.saturating_add(consumed),
                width: area.width,
                height: area.height.saturating_sub(consumed),
            }
        };
        if sub.height > 0 {
            let hint = format_shortcut_hint("ctrl+o", "expand", false, false).plain_text;
            consumed = consumed.saturating_add(render_wrapped(
                &hint,
                " ",
                theme.thinking,
                theme.thinking,
                sub,
                buf,
            ));
        }
    }

    consumed
}

// ---------------------------------------------------------------------------
// Committed-transcript collapse grouping
// ---------------------------------------------------------------------------
//
// The read/search collapse pass applied to *committed* rows (not just the live
// streaming overlay). Consecutive pure-tool-use assistant messages and their
// pure-tool-result user companions fold into a single past-tense summary line
// ("Searched for 2 patterns, read 1 file, listed 1 directory") so long
// chains of repetitive read/search activity don't flood the scrollback.
