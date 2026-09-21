use super::*;

/// One memoized answer to "how tall is the live inline tail".
///
/// Three probes ask the same layout question every frame — the overflow
/// check, the viewport sizing, and the paint — and each used to deep-copy
/// the tail rows and the overlay, build a fresh measure cache, and run the
/// full transcript renderer, at the redraw cadence. During a long tool
/// turn that was most of the TUI's CPU and its allocation churn. The memo
/// keys on everything layout depends on: a spinner-only frame reuses the
/// entry wholesale, and the paint borrows the same state and warm cache
/// the measurement produced.
///
/// The per-frame `TranscriptMeasureCache` constraint still holds (see the
/// key): `TranscriptStore::from_rows` mints synthetic revisions, so the
/// cache must never outlive the row window it was built for — here it
/// lives exactly as long as its key matches.
pub(crate) struct InlineTailMeasure {
    key: InlineTailKey,
    live_state: rebon_tui::AppState,
    cache: rebon_tui::TranscriptMeasureCache,
    height: u16,
    leading_segment_margin: bool,
    permission_h: usize,
}

/// The memo's home in `AppState`. Cloning an `AppState` (view swaps) does
/// not clone the memo — a clone starts cold and rebuilds on its first
/// frame, which is cheap and the only correct option for a cache keyed on
/// the exact state it was built from.
#[derive(Default)]
pub(crate) struct InlineTailMeasureSlot(pub(crate) std::cell::RefCell<Option<InlineTailMeasure>>);

impl Clone for InlineTailMeasureSlot {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for InlineTailMeasureSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InlineTailMeasureSlot")
    }
}

#[derive(PartialEq, Debug)]
struct InlineTailKey {
    width: u16,
    terminal_height: u16,
    start: usize,
    rows_len: usize,
    transcript_revision: u64,
    overlay_revision: u64,
    flush_counter: u32,
    verbosity: rebon_tui::ToolOutputVerbosity,
    is_loading: bool,
    permission_h: usize,
    foregrounded_task_id: Option<String>,
    live_agent_tool_activity_revision: u64,
    auto_mode_allowed_fingerprint: u64,
    /// The elapsed clock, bucketed to ten seconds. Cards print elapsed
    /// text ("59s" → "1m 0s") and a wrap-boundary crossing can change a
    /// card's height; the bucket bounds how long a stale height can
    /// stand without re-measuring every frame for a spinner tick.
    elapsed_bucket: u64,
}

const INLINE_TAIL_ELAPSED_BUCKET_MS: u64 = 10_000;

/// Order-independent fingerprint of the auto-mode annotations: an entry's
/// note text depends on both the id and the source, and HashMap iteration
/// order must not leak into the key.
fn auto_mode_allowed_fingerprint(
    ids: &std::collections::HashMap<String, rebon_types::AutoModeAllowSource>,
) -> u64 {
    use std::hash::{Hash, Hasher};
    ids.iter().fold(0u64, |acc, (id, source)| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        std::mem::discriminant(source).hash(&mut hasher);
        acc ^ hasher.finish()
    })
}

fn inline_tail_render_extras<'a>(
    app: &'a AppState,
    terminal_height: u16,
    leading_segment_margin: bool,
) -> rebon_tui::TranscriptRenderExtras<'a> {
    rebon_tui::TranscriptRenderExtras {
        leading_segment_margin,
        render_thinking_only_rows: true,
        expand_thinking_rows: false,
        force_verbose_edit_tool_previews: true,
        static_agent_group_status: false,
        inline_live_workflow_card_max_rows: Some(inline_live_workflow_card_max_rows(
            terminal_height,
        )),
        ..transcript_render_extras(
            app.foregrounded_task_id.as_deref(),
            &app.live_agent_tool_activity,
            app.live_agent_tool_activity_revision,
            &app.auto_mode_allowed_tool_ids,
        )
    }
}

/// Reuse the memoized tail measurement, rebuilding only when the key
/// misses. Returns `None` — and drops the memo, so the last long turn's
/// tail copy is not retained between turns — when there is nothing live
/// to measure.
fn ensure_inline_tail_measure<'a>(
    slot: &'a mut Option<InlineTailMeasure>,
    app: &AppState,
    theme: &RenderTheme,
    width: u16,
    terminal_height: u16,
    committed_rows: usize,
    elapsed_ms: u64,
) -> Option<&'a mut InlineTailMeasure> {
    if width == 0 {
        *slot = None;
        return None;
    }
    let rows = app.rebon_tui.transcript.rows();
    let rows_len = rows.len();
    let start = committed_rows.min(rows_len);
    let permission_h = app
        .pending_permission_view
        .as_ref()
        .map(|v| measure_permission_inline(v, width) as usize)
        .unwrap_or(0);
    if start >= rows_len && app.rebon_tui.overlay.is_empty() && !app.is_loading && permission_h == 0
    {
        *slot = None;
        return None;
    }
    let key = InlineTailKey {
        width,
        terminal_height,
        start,
        rows_len,
        transcript_revision: app.rebon_tui.transcript.revision(),
        overlay_revision: app.rebon_tui.overlay.revision(),
        flush_counter: app.rebon_tui.flush_counter,
        verbosity: app.tool_output_verbosity,
        is_loading: app.is_loading,
        permission_h,
        foregrounded_task_id: app.foregrounded_task_id.clone(),
        live_agent_tool_activity_revision: app.live_agent_tool_activity_revision,
        auto_mode_allowed_fingerprint: auto_mode_allowed_fingerprint(
            &app.auto_mode_allowed_tool_ids,
        ),
        elapsed_bucket: elapsed_ms / INLINE_TAIL_ELAPSED_BUCKET_MS,
    };
    if slot.as_ref().is_some_and(|memo| memo.key == key) {
        return slot.as_mut();
    }
    app.inline_tail_measure_builds
        .set(app.inline_tail_measure_builds.get().wrapping_add(1));
    let leading_segment_margin =
        super::super::inline_commit_cursor::inline_slice_needs_leading_segment_margin(
            rows_len, start,
        );
    let live_state = rebon_tui::AppState {
        transcript: rebon_tui::TranscriptStore::from_rows(rows[start..].to_vec()),
        overlay: app.rebon_tui.overlay.clone(),
        flush_counter: app.rebon_tui.flush_counter,
    };
    // Measure with the bucket-anchored clock, so the stored height cannot
    // depend on which frame inside the bucket built it.
    let measure_theme = RenderTheme {
        frame_time_ms: key.elapsed_bucket * INLINE_TAIL_ELAPSED_BUCKET_MS,
        ..*theme
    };
    let mut cache = rebon_tui::TranscriptMeasureCache::new();
    let height = measure_inline_transcript_height(
        &live_state,
        width,
        &measure_theme,
        app.tool_output_verbosity,
        permission_h,
        &mut cache,
        app.is_loading,
        inline_tail_render_extras(app, terminal_height, leading_segment_margin),
    );
    *slot = Some(InlineTailMeasure {
        key,
        live_state,
        cache,
        height,
        leading_segment_margin,
        permission_h,
    });
    slot.as_mut()
}

/// The memoized measurement's outputs, copied out so callers do not hold
/// the `RefCell` borrow across code that takes `&mut AppState`.
struct InlineTailProbe {
    height: u16,
    leading_segment_margin: bool,
}

fn probe_inline_tail(
    app: &AppState,
    theme: &RenderTheme,
    width: u16,
    terminal_height: u16,
    committed_rows: usize,
    elapsed_ms: u64,
) -> InlineTailProbe {
    let mut slot = app.inline_tail_measure.0.borrow_mut();
    match ensure_inline_tail_measure(
        &mut slot,
        app,
        theme,
        width,
        terminal_height,
        committed_rows,
        elapsed_ms,
    ) {
        Some(memo) => InlineTailProbe {
            height: memo.height,
            leading_segment_margin: memo.leading_segment_margin,
        },
        None => {
            let rows_len = app.rebon_tui.transcript.len();
            let start = committed_rows.min(rows_len);
            InlineTailProbe {
                height: 0,
                leading_segment_margin:
                    super::super::inline_commit_cursor::inline_slice_needs_leading_segment_margin(
                        rows_len, start,
                    ),
            }
        }
    }
}

pub(in crate::tui::runner) fn render_inline_frame(
    frame: &mut Frame,
    app: &mut AppState,
    runtime_state: &rebon_tui::promptinput::PromptInputRuntimeState,
    theme: &RenderTheme,
    is_loading: bool,
    status: &StatusBarInfo<'_>,
    session: Option<&TuiEngineSession>,
    committed_rows: usize,
    terminal_height: u16,
    cursor_hint: &mut Option<(u16, u16)>,
) {
    // The event loop sizes the viewport via `Terminal::set_viewport_height()`
    // (ratatui#1964 backported in our vendored fork), so `frame.area()` is
    // already the correct inline viewport height.
    let frame_area = frame.area();
    render_inline_frame_in_area(
        frame,
        frame_area,
        app,
        runtime_state,
        theme,
        is_loading,
        status,
        session,
        committed_rows,
        terminal_height,
        cursor_hint,
    );
}

/// What one inline frame is measured to before anything is painted: the
/// height each host asked for, the layout those heights were fitted
/// into, and the task and queue models the paint below reads.
struct InlineFramePlan {
    full_viewport_height: u16,
    has_inline_separator: bool,
    animated_theme: RenderTheme,
    queue_layout: QueueDisplayLayout,
    has_queue_banner: bool,
    task_list: TaskListRenderState,
    desired_ultraplan_height: u16,
    desired_task_list_height: u16,
    desired_prompt_height: u16,
    desired_queue_height: u16,
    desired_picker_height: u16,
    show_agent_switcher: bool,
    layout: InlineFrameLayout,
}

/// Measure the inline frame. Nothing here paints, and what it returns is
/// everything both the transcript and the prompt block are drawn from.
fn plan_inline_frame(
    app: &mut AppState,
    area: Rect,
    theme: &RenderTheme,
    committed_rows: usize,
    terminal_height: u16,
    elapsed_ms: u64,
) -> InlineFramePlan {
    // Prompt sizing must use the real terminal viewport height, not
    // `area.height` (= owned_height). `prompt_height_for_width` caps the
    // prompt at `terminal_height / 3`, so passing the squeezed owned_height
    // here collapses the prompt to a single row (just the top border) — the
    // measurement pass in `desired_inline_viewport_height` uses the full
    // viewport, and the render pass must agree or owned_height is wrong.
    let full_viewport_height = terminal_height.max(area.height).max(1);

    let rows_len = app.rebon_tui.transcript.rows().len();
    let start = committed_rows.min(rows_len);
    let has_live_transcript_content = start < rows_len;
    let has_pending_loading_indicator = app.is_loading && !has_live_transcript_content;
    let has_live_inline_content = has_live_transcript_content
        || has_pending_loading_indicator
        || !app.rebon_tui.overlay.is_empty()
        || app.pending_permission_view.is_some();
    let probe = probe_inline_tail(
        app,
        theme,
        area.width,
        terminal_height,
        committed_rows,
        elapsed_ms,
    );
    let leading_segment_margin = probe.leading_segment_margin;
    let measured_transcript_height = probe.height;
    let animated_theme = RenderTheme {
        frame_time_ms: elapsed_ms,
        ..*theme
    };
    let has_inline_separator = start > 0
        && has_live_inline_content
        && !leading_segment_margin
        && (measured_transcript_height > 1 || area.height > 3);
    let has_permission_suffix = app.pending_permission_view.is_some();
    let queue_layout = queue_display_layout(app, area.width);
    let desired_queue_height = if has_permission_suffix {
        0
    } else {
        queue_banner_height(&queue_layout)
    };
    let has_queue_banner = desired_queue_height > 0;
    let desired_transcript_height =
        measured_transcript_height.saturating_add(u16::from(has_inline_separator));
    let has_inline_modal = has_inline_active_dialog(app);
    let desired_ultraplan_height = if has_permission_suffix || has_inline_modal {
        0
    } else {
        progress_widget_height(app, full_viewport_height / 3)
    };
    let task_list = prepare_task_list_for_render(app, full_viewport_height, area.width);
    let desired_task_list_height = if has_permission_suffix || has_inline_modal {
        0
    } else {
        task_list.height
    };
    let desired_prompt_height = if has_permission_suffix {
        0
    } else {
        inline_prompt_host_height(app, area.width, full_viewport_height)
    };
    let desired_dialog_height = inline_dialog_host_height(app, area.height);
    let desired_picker_height = if !has_permission_suffix && !has_inline_modal {
        slash_picker_overlay_height(app).max(at_mention_overlay_height(app))
    } else {
        0
    };
    let desired_base_host_height = desired_ultraplan_height
        .saturating_add(desired_task_list_height)
        .saturating_add(desired_prompt_height)
        .saturating_add(desired_queue_height)
        .saturating_add(desired_picker_height)
        .max(desired_dialog_height);
    let footer_transcript_min = if has_queue_banner && desired_transcript_height > 0 {
        2u16
    } else {
        1
    };
    let show_footer = !has_permission_suffix
        && ((desired_transcript_height == 0 && !has_queue_banner)
            || area.height > desired_base_host_height.saturating_add(footer_transcript_min));
    let show_agent_switcher = show_footer && !has_permission_suffix;
    let desired_agent_switcher_height = if show_agent_switcher {
        agent_switcher_height(app)
    } else {
        0
    };
    let desired_prompt_block_height = desired_ultraplan_height
        .saturating_add(desired_task_list_height)
        .saturating_add(desired_prompt_height)
        .saturating_add(desired_queue_height)
        .saturating_add(desired_picker_height)
        .max(desired_dialog_height);
    let desired_host_height =
        desired_prompt_block_height.saturating_add(desired_agent_switcher_height);
    let compact_host_height = compact_inline_host_height(
        desired_host_height,
        desired_transcript_height,
        area.height,
        show_footer,
    );
    let compact_transcript_height = compact_inline_transcript_height(
        desired_transcript_height,
        compact_host_height,
        area.height,
        show_footer,
    );
    let footer_height = u16::from(show_footer && area.height > 0);
    let status_line_height =
        custom_status_line_height(app, area.width).min(area.height.saturating_sub(footer_height));
    let effective_compact_host_height = compact_host_height.min(
        area.height
            .saturating_sub(footer_height)
            .saturating_sub(status_line_height),
    );
    // If the inline viewport is too short for prompt + footer + agent rows,
    // collapse agent rows before shrinking the prompt/dialog block.
    let compact_agent_switcher_height = desired_agent_switcher_height
        .min(effective_compact_host_height.saturating_sub(desired_prompt_block_height.max(1)));
    let compact_prompt_block_height =
        effective_compact_host_height.saturating_sub(compact_agent_switcher_height);
    // Natural-flow composer: always lay the occupied block out top-down, so the
    // message stream stays at the top, the input hugs it from below, and any
    // spare viewport (the reservoir) opens up beneath the footer. The input
    // therefore rides UP as the live tail shrinks rather than being pinned to
    // the viewport bottom. Bottom-anchoring (the old flow-to-stick latch) left a
    // blank band ABOVE the prompt whenever a tall streaming viewport outlived a
    // short idle tail ("blank band, then bottom-anchored"); descending
    // naturally with the stream no longer flickers now that the event loop
    // wraps each frame's resize + insert_before + draw in a DEC 2026
    // synchronized-output bracket.
    let layout_area = area;
    let layout = inline_layout_with_agent_switcher(
        layout_area,
        compact_prompt_block_height,
        compact_transcript_height,
        show_footer,
        compact_agent_switcher_height,
        status_line_height,
    );
    tracing::debug!(
        target: "inline_render",
        area_h = area.height,
        area_w = area.width,
        committed_rows = committed_rows,
        rows_len,
        live_rows = rows_len.saturating_sub(start),
        overlay_blocks = app.rebon_tui.overlay.blocks.len(),
        has_live_inline_content,
        has_inline_separator,
        measured_transcript_height,
        desired_transcript_height,
        compact_transcript_height,
        layout_transcript_h = layout.transcript.height,
        layout_prompt_h = layout.prompt.height,
        layout_footer_h = layout.footer.height,
        layout_switcher_h = layout.agent_switcher.height,
        "render_inline_frame_in_area: layout"
    );
    InlineFramePlan {
        full_viewport_height,
        has_inline_separator,
        animated_theme,
        queue_layout,
        has_queue_banner,
        task_list,
        desired_ultraplan_height,
        desired_task_list_height,
        desired_prompt_height,
        desired_queue_height,
        desired_picker_height,
        show_agent_switcher,
        layout,
    }
}

pub(in crate::tui::runner) fn render_inline_frame_in_area(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    runtime_state: &PromptInputRuntimeState,
    theme: &RenderTheme,
    is_loading: bool,
    status: &StatusBarInfo<'_>,
    session: Option<&TuiEngineSession>,
    committed_rows: usize,
    terminal_height: u16,
    cursor_hint: &mut Option<(u16, u16)>,
) {
    clear_rect(frame, area);

    if app.agent_view.is_some() {
        if let Some(dialog) = app.agent_view.as_mut() {
            dialog.render(frame, area, theme, cursor_hint);
        }
        return;
    }

    let InlineFramePlan {
        full_viewport_height,
        has_inline_separator,
        animated_theme,
        queue_layout,
        has_queue_banner,
        task_list,
        desired_ultraplan_height,
        desired_task_list_height,
        desired_prompt_height,
        desired_queue_height,
        desired_picker_height,
        show_agent_switcher,
        layout,
    } = plan_inline_frame(
        app,
        area,
        theme,
        committed_rows,
        terminal_height,
        status.elapsed_ms,
    );
    let task_views = task_list.views;
    let task_completion_timestamps = task_list.completion_timestamps;

    if layout.transcript.height > 0 {
        let transcript_area = layout.transcript;
        let mut render_area = transcript_area;
        let reserve_inline_separator = has_inline_separator && transcript_area.height > 1;
        if reserve_inline_separator {
            render_area.y = render_area.y.saturating_add(1);
            render_area.height = render_area.height.saturating_sub(1);
        }
        // A pending permission view (e.g. a long ExitPlanMode plan) renders
        // as a suffix below the transcript and CANNOT flow to native
        // scrollback, so it must be scrollable in place. Honor app.scroll_offset
        // (clamped by the transcript renderer via suffix_skip_lines) unless we
        // are following the tail, in which case usize::MAX keeps the action
        // options anchored at the bottom. Normal inline streaming has no suffix
        // and relies on terminal scrollback, so it always follows the tail.
        let permission_pending = app.pending_permission_view.is_some();
        let effective_scroll = if permission_pending && !app.follow_transcript_tail {
            app.scroll_offset
        } else {
            usize::MAX
        };
        // Re-borrow the memo for the paint. The key is unchanged since the
        // probe above, so this is the same tail state and warm cache the
        // measurement produced — not a rebuild, and no per-frame deep copy.
        // The result fields are copied out before the borrow ends, because
        // the bookkeeping below writes through `&mut app`.
        let paint = {
            let mut memo_slot = app.inline_tail_measure.0.borrow_mut();
            ensure_inline_tail_measure(
                &mut memo_slot,
                app,
                theme,
                area.width,
                terminal_height,
                committed_rows,
                status.elapsed_ms,
            )
            .map(|memo| {
                let extras =
                    inline_tail_render_extras(app, terminal_height, memo.leading_segment_margin);
                let InlineTailMeasure {
                    live_state,
                    cache,
                    permission_h: memo_permission_h,
                    ..
                } = memo;
                let result = render_transcript_cached_with_running_hints(
                    &*live_state,
                    render_area,
                    frame.buffer_mut(),
                    &animated_theme,
                    effective_scroll,
                    app.tool_output_verbosity,
                    *memo_permission_h,
                    None,
                    cache,
                    app.is_loading,
                    extras,
                );
                (
                    result.total_lines,
                    result.render_y_end,
                    result.suffix_skip_lines,
                )
            })
        };
        if let Some((total_lines, render_y_end, suffix_skip_lines)) = paint {
            if let Some(view) = &app.pending_permission_view {
                let render_y = render_y_end;
                let bottom = transcript_area.y.saturating_add(transcript_area.height);
                if render_y < bottom {
                    let permission_result = render_permission_inline_with_cursor(
                        view,
                        Rect {
                            x: transcript_area.x,
                            y: render_y,
                            width: transcript_area.width,
                            height: bottom - render_y,
                        },
                        frame.buffer_mut(),
                        suffix_skip_lines,
                    );
                    if let Some(cursor) = permission_result.cursor {
                        *cursor_hint = Some(cursor);
                    }
                }
            }
            // Keep the scroll bookkeeping fresh so the event loop's scroll handlers
            // (PageUp/PageDown/j/k/Ctrl+Home/End) and the next frame operate on
            // accurate totals and a real viewport rect. Only while a permission
            // suffix is present — otherwise inline transcript scroll is owned by
            // the terminal, not the app, and scroll_offset must stay untouched.
            if permission_pending {
                app.total_content_lines = total_lines;
                app.prev_frame_area = Some(render_area);
                if app.follow_transcript_tail {
                    let vh = render_area.height as usize;
                    app.scroll_offset = total_lines.saturating_sub(vh);
                }
            }
        }
    }
    if layout.prompt.height > 0 {
        if app.help_open {
            render_help_panel(frame, layout.prompt, app);
            *cursor_hint = None;
        } else if render_inline_active_dialog(
            frame,
            layout.prompt,
            app,
            session,
            theme,
            cursor_hint,
        ) {
            // Inline true-modal surfaces replace the prompt input instead
            // of using the centered/fullscreen overlay path reserved for
            // screen mode.
        } else {
            let host_height = layout.prompt.height;
            let prompt_min_height = desired_prompt_height;
            let reserved_prompt_suffix = desired_queue_height
                .saturating_add(desired_picker_height)
                .saturating_add(prompt_min_height);
            let ultraplan_height =
                desired_ultraplan_height.min(host_height.saturating_sub(reserved_prompt_suffix));
            let reserved_after_ultraplan = ultraplan_height.saturating_add(reserved_prompt_suffix);
            let task_list_height =
                desired_task_list_height.min(host_height.saturating_sub(reserved_after_ultraplan));
            let queue_height = if has_queue_banner {
                desired_queue_height.min(
                    host_height
                        .saturating_sub(ultraplan_height)
                        .saturating_sub(task_list_height)
                        .saturating_sub(prompt_min_height),
                )
            } else {
                0
            };
            let picker_height = desired_picker_height.min(
                host_height
                    .saturating_sub(ultraplan_height)
                    .saturating_sub(task_list_height)
                    .saturating_sub(queue_height)
                    .saturating_sub(prompt_min_height),
            );
            let prompt_height = host_height
                .saturating_sub(ultraplan_height)
                .saturating_sub(task_list_height)
                .saturating_sub(queue_height)
                .saturating_sub(picker_height);
            let ultraplan_area = Rect::new(
                layout.prompt.x,
                layout.prompt.y,
                layout.prompt.width,
                ultraplan_height,
            );
            let task_list_area = Rect::new(
                layout.prompt.x,
                layout.prompt.y.saturating_add(ultraplan_height),
                layout.prompt.width,
                task_list_height,
            );
            let queue_area = Rect::new(
                layout.prompt.x,
                layout
                    .prompt
                    .y
                    .saturating_add(ultraplan_height)
                    .saturating_add(task_list_height),
                layout.prompt.width,
                queue_height,
            );
            let prompt_area = Rect::new(
                layout.prompt.x,
                layout
                    .prompt
                    .y
                    .saturating_add(ultraplan_height)
                    .saturating_add(task_list_height)
                    .saturating_add(queue_height),
                layout.prompt.width,
                prompt_height,
            );
            let picker_parent_area = Rect::new(
                layout.prompt.x,
                prompt_area.y,
                layout.prompt.width,
                prompt_height.saturating_add(picker_height),
            );
            if ultraplan_area.height > 0 {
                render_progress_widget(frame, ultraplan_area, app, theme);
            }
            if task_list_area.height > 0 {
                render_task_list(
                    frame,
                    task_list_area,
                    &task_views,
                    &task_completion_timestamps,
                    theme,
                    app.task_list_collapsed,
                    full_viewport_height,
                );
            }
            if queue_height > 0 {
                render_queue_banner(frame, queue_area, &queue_layout);
            }
            if prompt_area.height > 0 {
                if inline_verbose_prompt_active(app) {
                    app.last_prompt_input_area = None;
                    *cursor_hint = None;
                    render_inline_verbose_prompt(frame, prompt_area);
                } else {
                    render_prompt_surface(
                        frame,
                        prompt_area,
                        app,
                        runtime_state,
                        theme,
                        is_loading,
                        status.elapsed_ms,
                        inline_prompt_mode_badge(app),
                        cursor_hint,
                    );
                    render_slash_picker_inline_overlay(frame, prompt_area, picker_parent_area, app);
                    render_at_mention_inline_overlay(frame, prompt_area, picker_parent_area, app);
                }
            }
        }
    }
    if layout.status_line.height > 0 {
        render_custom_status_line(frame, layout.status_line, app);
    }
    if layout.footer.height > 0 {
        let zones = inline_footer_zones(app, area);
        if !render_custom_status_line_footer(frame, layout.footer, app) {
            render_footer(frame, layout.footer, app, status, !is_loading, &zones);
        }
    }
    if show_agent_switcher && layout.agent_switcher.height > 0 {
        render_agent_switcher_rows(frame, layout.agent_switcher, app, !is_loading);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui::runner) struct InlineViewportHeightInput {
    pub(in crate::tui::runner) width: u16,
    pub(in crate::tui::runner) terminal_height: u16,
    pub(in crate::tui::runner) base_height: u16,
    pub(in crate::tui::runner) committed_rows: usize,
    pub(in crate::tui::runner) elapsed_ms: u64,
}

/// Blank rows kept beneath the footer while the inline composer is idle (or
/// loading a short tail). The natural-flow render top-aligns the content, so
/// this many viewport rows past the content become a "breathing" reservoir
/// under the prompt instead of flooring to the configured viewport height
/// (which left a tall blank band when the live tail was short). One row per
/// user preference — just enough that the input never butts against scrollback.
const IDLE_BOTTOM_RESERVOIR_ROWS: u16 = 1;

/// Rows reserved for inline chrome around the live transcript (prompt ≥ 3,
/// footer, inline separator, overlay leading margin, agent switcher rows)
/// when bounding a live workflow card to the terminal height.
const INLINE_LIVE_WORKFLOW_CARD_CHROME_RESERVE: u16 = 8;
/// Smallest bounded live workflow card: header + run summary + elision
/// marker + two freshest rows.
const INLINE_LIVE_WORKFLOW_CARD_MIN_ROWS: u16 = 5;

/// Row budget for an in-progress Workflow card in the inline live region.
/// The live overlay is bottom-anchored and an in-progress block cannot drain
/// to scrollback, so an unbounded card would push its own header above the
/// viewport top where it is permanently invisible. The card renderer elides
/// the middle of the body to stay within this budget; the full card still
/// drains to scrollback once the workflow reaches a terminal status.
pub(in crate::tui::runner) fn inline_live_workflow_card_max_rows(terminal_height: u16) -> u16 {
    terminal_height
        .saturating_sub(INLINE_LIVE_WORKFLOW_CARD_CHROME_RESERVE)
        .max(INLINE_LIVE_WORKFLOW_CARD_MIN_ROWS)
}

// MIGRATION(ratatui ≥0.31): this function becomes the `new_height` argument
// to `Terminal::set_viewport_height()` instead of a render-time owned_area calc.
pub(in crate::tui::runner) fn desired_inline_viewport_height(
    app: &AppState,
    theme: &RenderTheme,
    input: InlineViewportHeightInput,
) -> u16 {
    let terminal_height = input.terminal_height.max(1);
    let base_height = input.base_height.max(1).min(terminal_height);
    if input.width == 0 {
        return base_height;
    }
    if app.agent_view.is_some() || app.background_tasks_dialog.is_some() {
        return terminal_height;
    }

    let rows_len = app.rebon_tui.transcript.rows().len();
    let start = input.committed_rows.min(rows_len);
    let has_live_transcript_content = start < rows_len;
    let has_pending_loading_indicator = app.is_loading && !has_live_transcript_content;
    let has_live_inline_content = has_live_transcript_content
        || has_pending_loading_indicator
        || !app.rebon_tui.overlay.is_empty()
        || app.pending_permission_view.is_some();
    let probe = probe_inline_tail(
        app,
        theme,
        input.width,
        terminal_height,
        input.committed_rows,
        input.elapsed_ms,
    );
    let leading_segment_margin = probe.leading_segment_margin;
    let measured_transcript_height = probe.height;
    let has_inline_separator = start > 0
        && has_live_inline_content
        && !leading_segment_margin
        && (measured_transcript_height > 1 || terminal_height > 3);
    let desired_transcript_height =
        measured_transcript_height.saturating_add(u16::from(has_inline_separator));

    let has_permission_suffix = app.pending_permission_view.is_some();
    let queue_layout = queue_display_layout(app, input.width);
    let desired_queue_height = if has_permission_suffix {
        0
    } else {
        queue_banner_height(&queue_layout)
    };
    let has_queue_banner = desired_queue_height > 0;
    let has_inline_modal = has_inline_active_dialog(app);
    let task_views = collect_task_views();
    let task_completion_timestamps = task_completion_timestamps_for_render(app);
    let desired_task_list_height = if has_permission_suffix || has_inline_modal {
        0
    } else {
        task_list_height_for_views(
            &task_views,
            &task_completion_timestamps,
            app.task_list_collapsed,
            terminal_height,
            input.width,
        )
    };
    let desired_ultraplan_height = if has_permission_suffix || has_inline_modal {
        0
    } else {
        progress_widget_height(app, terminal_height / 3)
    };
    let desired_prompt_height = if has_permission_suffix {
        0
    } else {
        inline_prompt_host_height(app, input.width, terminal_height)
    };
    let desired_dialog_height = inline_dialog_host_height(app, terminal_height);
    let desired_picker_height = if !has_permission_suffix && !has_inline_modal {
        slash_picker_overlay_height(app).max(at_mention_overlay_height(app))
    } else {
        0
    };
    let desired_prompt_block_height = desired_ultraplan_height
        .saturating_add(desired_task_list_height)
        .saturating_add(desired_prompt_height)
        .saturating_add(desired_queue_height)
        .saturating_add(desired_picker_height)
        .max(desired_dialog_height);
    let footer_transcript_min = if has_queue_banner && desired_transcript_height > 0 {
        2u16
    } else {
        1
    };
    let include_footer = !has_permission_suffix
        && ((desired_transcript_height == 0 && !has_queue_banner) || desired_transcript_height > 0);
    let desired_agent_switcher_height = if include_footer {
        agent_switcher_height(app)
    } else {
        0
    };
    let desired_status_line_height = if include_footer {
        custom_status_line_height(app, input.width)
    } else {
        0
    };
    let transcript_height = if include_footer && desired_transcript_height > 0 {
        desired_transcript_height.max(footer_transcript_min)
    } else {
        desired_transcript_height
    };
    let desired_height = transcript_height
        .saturating_add(desired_prompt_block_height)
        .saturating_add(u16::from(include_footer))
        .saturating_add(desired_status_line_height)
        .saturating_add(desired_agent_switcher_height);

    // Bottom reservoir: the natural-flow render top-aligns the occupied block,
    // so any viewport height past the content becomes blank rows BENEATH the
    // footer. Keep exactly IDLE_BOTTOM_RESERVOIR_ROWS of them — one breathing
    // row under the prompt — instead of flooring to `base_height`, which left a
    // tall blank band when the live tail was short. The input still rides up as
    // the tail shrinks; per-frame resize no longer flickers (the event loop's
    // DEC 2026 synchronized-output bracket) and the loading high-water hold
    // keeps a streaming tail from shrink-jittering, so the old floor's
    // anti-resize role is no longer needed.
    //
    // Exact-fit surfaces opt out entirely (no reservoir): permission/modal/dialog
    // hosts size to their content, and the @/slash picker content-sizes so it
    // sits flush above the footer.
    if has_permission_suffix
        || has_inline_modal
        || desired_dialog_height > 0
        || desired_picker_height > 0
    {
        desired_height.max(1).min(terminal_height)
    } else {
        desired_height
            .saturating_add(IDLE_BOTTOM_RESERVOIR_ROWS)
            .min(terminal_height)
    }
}

/// Predict whether the inline live-tail content (rows after the commit
/// cursor + streaming overlay + permission/queue/picker suffixes) would
/// be cropped by the layout the next `render_inline_frame` would compute
/// for this `terminal_height`. Returns `true` when the total demand
/// exceeds the viewport, which is the signal the event loop uses to
/// force-drain the held tool cluster into transcript → scrollback before
/// drawing, so the dynamic prompt/queue/picker areas can grow without
/// silently swallowing earlier overlay rows from the top.
pub(in crate::tui::runner) fn inline_live_content_overflows_viewport(
    app: &AppState,
    theme: &RenderTheme,
    input: InlineViewportHeightInput,
) -> bool {
    let terminal_height = input.terminal_height.max(1);
    if input.width == 0 {
        return false;
    }
    if app.agent_view.is_some() || app.background_tasks_dialog.is_some() {
        return false;
    }

    let rows_len = app.rebon_tui.transcript.rows().len();
    let start = input.committed_rows.min(rows_len);
    let has_live_transcript_content = start < rows_len;
    let has_pending_loading_indicator = app.is_loading && !has_live_transcript_content;
    let has_live_inline_content = has_live_transcript_content
        || has_pending_loading_indicator
        || !app.rebon_tui.overlay.is_empty()
        || app.pending_permission_view.is_some();
    let probe = probe_inline_tail(
        app,
        theme,
        input.width,
        terminal_height,
        input.committed_rows,
        input.elapsed_ms,
    );
    let leading_segment_margin = probe.leading_segment_margin;
    let measured_transcript_height = probe.height;
    let has_inline_separator = start > 0
        && has_live_inline_content
        && !leading_segment_margin
        && (measured_transcript_height > 1 || terminal_height > 3);
    let desired_transcript_height =
        measured_transcript_height.saturating_add(u16::from(has_inline_separator));
    if desired_transcript_height == 0 {
        return false;
    }

    let has_permission_suffix = app.pending_permission_view.is_some();
    let desired_queue_height = if has_permission_suffix {
        0
    } else {
        queue_banner_height(&queue_display_layout(app, input.width))
    };
    let has_inline_modal = has_inline_active_dialog(app);
    let task_views = collect_task_views();
    let task_completion_timestamps = task_completion_timestamps_for_render(app);
    let desired_task_list_height = if has_permission_suffix || has_inline_modal {
        0
    } else {
        task_list_height_for_views(
            &task_views,
            &task_completion_timestamps,
            app.task_list_collapsed,
            terminal_height,
            input.width,
        )
    };
    let desired_ultraplan_height = if has_permission_suffix || has_inline_modal {
        0
    } else {
        progress_widget_height(app, terminal_height / 3)
    };
    let desired_prompt_height = if has_permission_suffix {
        0
    } else {
        inline_prompt_host_height(app, input.width, terminal_height)
    };
    let desired_dialog_height = inline_dialog_host_height(app, terminal_height);
    let desired_picker_height = if !has_permission_suffix && !has_inline_modal {
        slash_picker_overlay_height(app).max(at_mention_overlay_height(app))
    } else {
        0
    };
    let desired_prompt_block_height = desired_ultraplan_height
        .saturating_add(desired_task_list_height)
        .saturating_add(desired_prompt_height)
        .saturating_add(desired_queue_height)
        .saturating_add(desired_picker_height)
        .max(desired_dialog_height);
    // Permission suffixes replace the entire inline chrome host: prompt,
    // footer, custom status line, and agent switcher are all hidden by the
    // render path and by `desired_inline_viewport_height`. Overflow must use
    // the same geometry; reserving phantom chrome here makes an exact-fit
    // permission frame look one-or-more rows too tall and repeatedly triggers
    // the event loop's force-drain escape hatch.
    let include_chrome = !has_permission_suffix;
    let footer_height = u16::from(include_chrome);
    let desired_status_line_height = if include_chrome {
        custom_status_line_height(app, input.width)
    } else {
        0
    };
    let desired_agent_switcher_height = if include_chrome {
        agent_switcher_height(app)
    } else {
        0
    };
    let max_transcript_area = terminal_height
        .saturating_sub(desired_prompt_block_height)
        .saturating_sub(footer_height)
        .saturating_sub(desired_status_line_height)
        .saturating_sub(desired_agent_switcher_height);
    desired_transcript_height > max_transcript_area
}

pub(in crate::tui::runner) fn inline_prompt_host_height(
    app: &AppState,
    width: u16,
    available_height: u16,
) -> u16 {
    let base = prompt_height_for_width(app, width, available_height);
    if let Some(dialog) = app.resume_dialog.as_ref() {
        base.max(dialog.desired_height())
            .min(available_height.saturating_sub(1).max(1))
    } else if app.help_open {
        base.max(18).min(available_height.saturating_sub(1).max(1))
    } else if has_inline_active_dialog(app) {
        base.max(6).min(available_height.saturating_sub(1).max(1))
    } else if inline_verbose_prompt_active(app) {
        available_height.min(2)
    } else {
        base
    }
}

pub(in crate::tui::runner) fn compact_inline_host_height(
    desired_host_height: u16,
    desired_transcript_height: u16,
    available_height: u16,
    show_footer: bool,
) -> u16 {
    if desired_transcript_height == 0 || available_height <= 1 || show_footer {
        return desired_host_height;
    }
    let footer_height = u16::from(show_footer && available_height > 0);
    let max_transcript_height = available_height
        .saturating_sub(desired_host_height.max(1))
        .saturating_sub(footer_height);
    if max_transcript_height > 0 {
        desired_host_height
    } else {
        desired_host_height.min(available_height.saturating_sub(1).max(1))
    }
}

pub(in crate::tui::runner) fn compact_inline_transcript_height(
    desired_transcript_height: u16,
    desired_host_height: u16,
    available_height: u16,
    show_footer: bool,
) -> u16 {
    if desired_transcript_height == 0 || available_height == 0 {
        return desired_transcript_height;
    }
    let footer_height = u16::from(show_footer && available_height > 0);
    let max_transcript_height = available_height
        .saturating_sub(desired_host_height.max(1))
        .saturating_sub(footer_height);
    if max_transcript_height > 0 {
        desired_transcript_height
    } else {
        // In very small inline viewports, preserve at least one live
        // transcript row ahead of an oversized prompt/dialog host instead of
        // rendering only the prompt and leaving streamed content hidden.
        1
    }
}

pub(in crate::tui::runner) fn inline_dialog_host_height(
    app: &AppState,
    available_height: u16,
) -> u16 {
    if app.agent_view.is_some()
        || app.background_tasks_dialog.is_some()
        || app.dialogs.top_id() == Some(rebon_ui_seat::ids::dialog::SETTINGS)
    {
        available_height.max(1)
    } else if let Some(dialog) = app
        .onboarding_dialog
        .as_ref()
        .filter(|dialog| dialog.is_inline_hosted())
    {
        // The `/provider` and `/login` dialogs grow the host downward to
        // fit the visible pane instead of taking over the whole viewport
        // (which would scroll the conversation out of view).
        dialog
            .inline_host_desired_height()
            .min(available_height.max(1))
            .max(1)
    } else if let Some(desired) = app.dialogs.top_desired_height() {
        // Hosted pickers size to their content instead of taking over
        // the whole inline viewport.
        desired.min(available_height.max(1)).max(1)
    } else {
        0
    }
}

pub(in crate::tui::runner) fn has_inline_active_dialog(app: &AppState) -> bool {
    app.agent_view.is_some()
        || app.goal_confirm_dialog.is_some()
        || app.global_search_dialog.is_some()
        || app.resume_dialog.is_some()
        || app.background_tasks_dialog.is_some()
        || app.rewind_dialog.is_some()
        || app
            .onboarding_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_inline_hosted())
        || crate::tui::dialog_host::has_inline_view(&app.dialogs)
}

pub(in crate::tui::runner) fn render_inline_active_dialog(
    frame: &mut Frame,
    prompt_area: Rect,
    app: &mut AppState,
    session: Option<&TuiEngineSession>,
    theme: &RenderTheme,
    cursor_hint: &mut Option<(u16, u16)>,
) -> bool {
    refresh_settings_projection(app, session);
    let area = clamp_rect(prompt_area, frame.area());
    if area.width == 0 || area.height == 0 {
        return has_inline_active_dialog(app);
    }

    if let Some(dialog) = app.agent_view.as_mut() {
        dialog.render(frame, area, theme, cursor_hint);
    } else if let Some(dialog) = app.goal_confirm_dialog.as_ref() {
        dialog.render(frame, area, theme);
    } else if let Some(dialog) = app.global_search_dialog.as_ref() {
        dialog.render(frame, area);
    } else if let Some(dialog) = app.resume_dialog.as_ref() {
        dialog.render(frame, area);
    } else if app.background_tasks_dialog.is_some() {
        render_background_tasks_dialog(frame, area, app);
    } else if let Some(dialog) = app.rewind_dialog.as_ref() {
        dialog.render(frame, area);
    } else if let Some(dialog) = app
        .onboarding_dialog
        .as_ref()
        .filter(|dialog| dialog.is_inline_hosted())
    {
        crate::tui::onboarding_dialog::render(dialog, frame, area);
    } else if crate::tui::dialog_host::render_inline(&mut app.dialogs, frame, area) {
        // Painted by the host.
    } else {
        return false;
    }
    true
}

pub(in crate::tui::runner) fn clamp_rect(rect: Rect, bounds: Rect) -> Rect {
    let x1 = rect.x.max(bounds.x);
    let y1 = rect.y.max(bounds.y);
    let x2 = rect.right().min(bounds.right());
    let y2 = rect.bottom().min(bounds.bottom());
    Rect::new(x1, y1, x2.saturating_sub(x1), y2.saturating_sub(y1))
}

pub(in crate::tui::runner) fn inline_picker_area(
    prompt_area: Rect,
    parent_area: Rect,
    desired_height: u16,
) -> Option<Rect> {
    inline_picker_area_with_preference(prompt_area, parent_area, desired_height, false)
}

pub(in crate::tui::runner) fn inline_picker_area_below_first(
    prompt_area: Rect,
    parent_area: Rect,
    desired_height: u16,
) -> Option<Rect> {
    inline_picker_area_with_preference(prompt_area, parent_area, desired_height, true)
}

fn inline_picker_area_with_preference(
    prompt_area: Rect,
    parent_area: Rect,
    desired_height: u16,
    prefer_below: bool,
) -> Option<Rect> {
    if desired_height < 2 || parent_area.width < 3 || parent_area.height < 2 {
        return None;
    }
    let x = prompt_area.x.saturating_add(1).max(parent_area.x);
    let max_width = parent_area.right().saturating_sub(x);
    let width = prompt_area.width.min(60).min(max_width);
    if width < 3 {
        return None;
    }

    let available_above = prompt_area.y.saturating_sub(parent_area.y);
    let parent_bottom = parent_area.y.saturating_add(parent_area.height);
    let available_below = parent_bottom.saturating_sub(prompt_area.bottom());
    let height = desired_height
        .min(parent_area.height)
        .min(available_above.max(available_below));
    if height < 2 {
        return None;
    }

    let y = if prefer_below {
        if available_below >= height || available_below >= available_above {
            prompt_area
                .bottom()
                .min(parent_bottom.saturating_sub(height))
        } else {
            prompt_area.y.saturating_sub(height).max(parent_area.y)
        }
    } else if available_above >= height || available_above >= available_below {
        prompt_area.y.saturating_sub(height).max(parent_area.y)
    } else {
        prompt_area
            .bottom()
            .min(parent_bottom.saturating_sub(height))
    };
    Some(Rect::new(x, y, width, height))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InlineFrameLayout {
    pub(super) transcript: Rect,
    pub(super) prompt: Rect,
    pub(super) status_line: Rect,
    pub(super) footer: Rect,
    pub(super) agent_switcher: Rect,
}

#[cfg(test)]
pub(in crate::tui::runner) fn inline_layout(
    area: Rect,
    desired_prompt_height: u16,
    desired_transcript_height: u16,
    show_footer: bool,
) -> InlineFrameLayout {
    inline_layout_with_agent_switcher(
        area,
        desired_prompt_height,
        desired_transcript_height,
        show_footer,
        0,
        0,
    )
}

pub(in crate::tui::runner) fn inline_layout_with_agent_switcher(
    area: Rect,
    desired_prompt_height: u16,
    desired_transcript_height: u16,
    show_footer: bool,
    desired_agent_switcher_height: u16,
    status_line_height: u16,
) -> InlineFrameLayout {
    let requested_status_line_height = status_line_height;
    let footer_height = u16::from(show_footer && area.height > 0);
    let min_prompt_height = u16::from(desired_prompt_height > 0);
    let min_transcript_height = u16::from(desired_transcript_height > 0);
    let max_status_line_height = area
        .height
        .saturating_sub(footer_height)
        .saturating_sub(min_prompt_height)
        .saturating_sub(min_transcript_height);
    let status_line_height = requested_status_line_height
        .min(area.height.saturating_sub(footer_height))
        .min(max_status_line_height);
    // Keep the footer/status row directly below the prompt. Agent switcher
    // rows, when present, live under the footer (matching screen mode), and
    // are the first thing to collapse in very small inline viewports.
    let max_agent_switcher_height = area
        .height
        .saturating_sub(footer_height)
        .saturating_sub(status_line_height)
        .saturating_sub(1);
    let agent_switcher_height = desired_agent_switcher_height.min(max_agent_switcher_height);
    let reserved_transcript_height = if requested_status_line_height > 0 {
        min_transcript_height
    } else {
        0
    };
    let max_prompt_height = area
        .height
        .saturating_sub(footer_height)
        .saturating_sub(status_line_height)
        .saturating_sub(agent_switcher_height)
        .saturating_sub(reserved_transcript_height);
    let prompt_height = if desired_prompt_height == 0 {
        0
    } else {
        desired_prompt_height.max(1).min(max_prompt_height)
    };
    let max_transcript_height = area
        .height
        .saturating_sub(prompt_height)
        .saturating_sub(footer_height)
        .saturating_sub(status_line_height)
        .saturating_sub(agent_switcher_height);
    let transcript_height = desired_transcript_height.min(max_transcript_height);
    let transcript = Rect::new(area.x, area.y, area.width, transcript_height);
    let prompt = Rect::new(
        area.x,
        area.y.saturating_add(transcript_height),
        area.width,
        prompt_height,
    );
    let status_line = Rect::new(
        area.x,
        area.y
            .saturating_add(transcript_height)
            .saturating_add(prompt_height),
        area.width,
        status_line_height,
    );
    let footer = Rect::new(
        area.x,
        status_line.y.saturating_add(status_line.height),
        area.width,
        footer_height,
    );
    let agent_switcher = Rect::new(
        area.x,
        footer.y.saturating_add(footer.height),
        area.width,
        agent_switcher_height,
    );
    InlineFrameLayout {
        transcript,
        prompt,
        status_line,
        footer,
        agent_switcher,
    }
}

pub(in crate::tui::runner) fn measure_inline_transcript_height(
    state: &rebon_tui::AppState,
    width: u16,
    theme: &RenderTheme,
    verbosity: rebon_tui::ToolOutputVerbosity,
    suffix_height: usize,
    cache: &mut rebon_tui::TranscriptMeasureCache,
    show_running_transcript_hints: bool,
    extras: rebon_tui::TranscriptRenderExtras<'_>,
) -> u16 {
    if width == 0 {
        return 0;
    }
    let area = Rect::new(0, 0, width, 0);
    let mut scratch = Buffer::empty(area);
    let result = render_transcript_cached_with_running_hints(
        state,
        area,
        &mut scratch,
        theme,
        0,
        verbosity,
        suffix_height,
        None,
        cache,
        show_running_transcript_hints,
        extras,
    );
    result.total_lines.min(u16::MAX as usize) as u16
}

pub(in crate::tui::runner) fn inline_footer_zones(app: &AppState, area: Rect) -> LayoutZones {
    let messages_lite: Vec<unseen_divider::MessageLite> = app
        .rebon_tui
        .transcript
        .rows()
        .iter()
        .map(super::super::to_message_lite)
        .collect();
    let unseen =
        unseen_divider::compute_unseen_divider(&messages_lite, app.unseen_divider.divider_index());
    let unseen_count = unseen.as_ref().map(|u| u.count).unwrap_or(0);
    let snap = super::super::build_scroll_snapshot(app, area.height.saturating_sub(4) as usize);
    let pill_vis = app.unseen_divider.pill_visible(snap);
    let input = LayoutInput {
        fullscreen_enabled: false,
        terminal_rows: area.height,
        terminal_columns: area.width,
        sticky: StickyPrompt::None,
        hide_sticky: true,
        has_scrollable: true,
        has_overlay: app.pending_permission_view.is_some(),
        has_bottom: true,
        has_bottom_float: false,
        has_modal: false,
        hide_pill: false,
        pill_visible: pill_vis,
        new_message_count: unseen_count,
        has_suggestions_overlay: false,
        has_dialog_overlay: false,
    };
    fullscreen_layout::layout_zones(&input)
}

pub(in crate::tui::runner) fn clear_rect(frame: &mut Frame, area: Rect) {
    let buf = frame.buffer_mut();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_dialog_replaces_inline_prompt_with_compact_host() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = AppState::default();
        app.dialogs
            .push(rebon_dialog::effort_dialog::EffortDialogState::open(
                "inline-model",
                None,
            ));
        assert!(has_inline_active_dialog(&app));
        // Five levels plus border, blank and footer.
        assert_eq!(inline_dialog_host_height(&app, 20), 9);

        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| {
                let mut cursor_hint = Some((0, 0));
                assert!(render_inline_active_dialog(
                    frame,
                    frame.area(),
                    &mut app,
                    None,
                    &RenderTheme::plain(),
                    &mut cursor_hint,
                ));
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Select Reasoning Level for inline-model"));
        assert!(rendered.contains("1. Low"));
    }

    #[test]
    fn wrapped_status_line_counts_toward_live_overflow() {
        fn minimum_height_without_overflow(
            app: &AppState,
            input: InlineViewportHeightInput,
        ) -> u16 {
            (1..=64)
                .find(|terminal_height| {
                    !inline_live_content_overflows_viewport(
                        app,
                        &RenderTheme::plain(),
                        InlineViewportHeightInput {
                            terminal_height: *terminal_height,
                            ..input
                        },
                    )
                })
                .expect("live content should fit within a 64-row viewport")
        }

        let mut app = AppState::default();
        app.rebon_tui.overlay.append_streaming_text("one\ntwo");
        let input = InlineViewportHeightInput {
            width: 10,
            terminal_height: 10,
            base_height: 4,
            committed_rows: 0,
            elapsed_ms: 0,
        };
        let height_without_status = minimum_height_without_overflow(&app, input);

        app.custom_status_line.output = vec![String::from("012345678901234567890123456789")];
        let height_with_status = minimum_height_without_overflow(&app, input);

        assert!(
            height_with_status > height_without_status,
            "wrapped status rows should increase the required viewport height: without={height_without_status}, with={height_with_status}"
        );
    }

    #[test]
    fn inline_layout_places_status_line_immediately_above_footer() {
        for status_height in 1..=5 {
            let layout = inline_layout_with_agent_switcher(
                Rect::new(0, 0, 80, 10),
                2,
                2,
                true,
                0,
                status_height,
            );
            assert_eq!(layout.status_line.height, status_height);
            assert_eq!(
                layout.footer.y,
                layout.status_line.y + layout.status_line.height
            );
            assert_eq!(layout.prompt.y + layout.prompt.height, layout.status_line.y);
        }
    }

    #[test]
    fn inline_layout_shrinks_status_line_before_erasing_prompt_and_transcript() {
        let layout = inline_layout_with_agent_switcher(Rect::new(0, 0, 80, 4), 2, 2, true, 0, 5);
        assert_eq!(layout.footer.height, 1);
        assert_eq!(layout.prompt.height, 1);
        assert_eq!(layout.transcript.height, 1);
        assert_eq!(layout.status_line.height, 1);
        assert_eq!(layout.footer.y, 3);
    }

    #[test]
    fn inline_layout_allows_status_line_to_hide_when_viewport_is_tiny() {
        let layout = inline_layout_with_agent_switcher(Rect::new(0, 0, 80, 3), 2, 2, true, 0, 5);
        assert_eq!(layout.footer.height, 1);
        assert_eq!(layout.prompt.height, 1);
        assert_eq!(layout.transcript.height, 1);
        assert_eq!(layout.status_line.height, 0);
    }

    fn probe_height(app: &crate::tui::app::AppState, elapsed_ms: u64) -> u16 {
        super::probe_inline_tail(app, &RenderTheme::plain(), 80, 24, 0, elapsed_ms).height
    }

    /// Spinner frames must not rebuild the memo: same content, elapsed
    /// within the bucket — one build for any number of probes. A content
    /// change rebuilds.
    #[test]
    fn inline_tail_memo_reuses_across_spinner_frames() {
        let mut app = crate::tui::app::AppState::default();
        app.rebon_tui
            .overlay
            .append_streaming_text("streaming line one");
        app.is_loading = true;

        assert!(probe_height(&app, 1_000) > 0);
        probe_height(&app, 1_050);
        probe_height(&app, 9_900);
        assert_eq!(app.inline_tail_measure_builds.get(), 1);

        app.rebon_tui.overlay.append_streaming_text(" and more");
        probe_height(&app, 9_950);
        assert_eq!(app.inline_tail_measure_builds.get(), 2);
    }

    /// The force-drain interleave: a flush between the overflow probe and
    /// the paint bumps `flush_counter`, and the next probe must miss the
    /// memo instead of painting the stale row window.
    #[test]
    fn inline_tail_memo_misses_after_a_flush() {
        let mut app = crate::tui::app::AppState::default();
        app.rebon_tui
            .overlay
            .append_streaming_text("first line\nsecond line");
        app.is_loading = true;

        probe_height(&app, 1_000);
        assert_eq!(app.inline_tail_measure_builds.get(), 1);

        app.rebon_tui.flush_counter = app.rebon_tui.flush_counter.wrapping_add(1);
        probe_height(&app, 1_016);
        assert_eq!(app.inline_tail_measure_builds.get(), 2);
    }

    /// Between turns the memo is dropped, not retained with the last long
    /// turn's tail copy inside it.
    #[test]
    fn inline_tail_memo_drops_when_the_tail_empties() {
        let mut app = crate::tui::app::AppState::default();
        app.rebon_tui.overlay.append_streaming_text("tail");
        app.is_loading = true;
        probe_height(&app, 1_000);
        assert!(app.inline_tail_measure.0.borrow().is_some());

        app.rebon_tui.overlay.clear();
        app.is_loading = false;
        probe_height(&app, 2_000);
        assert!(app.inline_tail_measure.0.borrow().is_none());
    }
}
