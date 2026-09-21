use super::*;

pub(in crate::tui::runner) fn render_transcript_area(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    theme: &RenderTheme,
    cursor_hint: &mut Option<(u16, u16)>,
) {
    if app.rebon_tui.transcript.is_empty()
        && app.rebon_tui.overlay.is_empty()
        && app.pending_permission_view.is_none()
    {
        render_empty_transcript_background(frame, area);
        return;
    }

    // Measure the inline permission overlay height so render_transcript
    // includes it in total_lines for correct scroll math. The permission
    // overlay sits inside the scrollable area, after scrollable content.
    let permission_h = app
        .pending_permission_view
        .as_ref()
        .map(|v| measure_permission_inline(v, area.width) as usize)
        .unwrap_or(0);

    // When following tail, pass usize::MAX so render_transcript
    // clamps scroll_offset using its freshly measured total_lines.
    // This eliminates the 1-frame lag where new content (e.g. a tool
    // call) would be off-screen because the pre-render snap used a
    // stale total_content_lines from the previous frame.
    let effective_scroll = if app.follow_transcript_tail {
        usize::MAX
    } else {
        app.scroll_offset
    };

    let result = render_transcript_cached_with_running_hints(
        &app.rebon_tui,
        area,
        frame.buffer_mut(),
        theme,
        effective_scroll,
        app.tool_output_verbosity,
        permission_h,
        None,
        &mut app.transcript_measure_cache,
        app.is_loading,
        rebon_tui::TranscriptRenderExtras {
            leading_segment_margin: false,
            force_verbose_edit_tool_previews: true,
            ..transcript_render_extras(
                app.foregrounded_task_id.as_deref(),
                &app.live_agent_tool_activity,
                app.live_agent_tool_activity_revision,
                &app.auto_mode_allowed_tool_ids,
            )
        },
    );

    // Update total content lines and scroll offset so subsequent
    // scroll math (keyboard handlers, next frame) uses fresh data.
    app.total_content_lines = result.total_lines;
    app.transcript_sticky_anchor = result.sticky_anchor;
    app.transcript_sticky_anchor_label = result.sticky_anchor.and_then(|anchor| {
        app.rebon_tui
            .transcript
            .rows()
            .get(anchor.row_index)
            .and_then(rebon_tui::Message::sticky_anchor_preview_text)
    });
    let vh = area.height as usize;
    if app.follow_transcript_tail {
        app.scroll_offset = result.total_lines.saturating_sub(vh);
    }

    // ── Selection scroll compensation ────────────────────────────
    // Detect scroll delta (from keyboard scroll, mouse wheel, or
    // follow-tail). Capture rows leaving the viewport from the
    // PREVIOUS frame's snapshot, then shift selection coordinates.
    let scroll_delta = app.scroll_offset as i32 - app.prev_scroll_offset as i32;
    if scroll_delta != 0
        && app.selection_owner == SelectionOwner::Transcript
        && app.selection.has_selection()
    {
        let min_row = area.y;
        let max_row = area.y + area.height.saturating_sub(1);

        // Capture rows about to scroll off from the prev-frame snapshot.
        if let Some(prev_area) = app.prev_frame_area {
            if scroll_delta > 0 {
                // Scrolled down → top rows of old viewport scroll off above.
                let first = prev_area.y;
                let last = prev_area.y
                    + (scroll_delta as u16)
                        .min(prev_area.height)
                        .saturating_sub(1);
                app.selection.capture_from_snapshot(
                    &app.prev_frame_lines,
                    prev_area,
                    first,
                    last,
                    rebon_tui::selection::ScrollSide::Above,
                );
            } else {
                // Scrolled up → bottom rows of old viewport scroll off below.
                let delta = (-scroll_delta) as u16;
                let last = prev_area.y + prev_area.height.saturating_sub(1);
                let first = last.saturating_sub(delta.saturating_sub(1));
                app.selection.capture_from_snapshot(
                    &app.prev_frame_lines,
                    prev_area,
                    first,
                    last,
                    rebon_tui::selection::ScrollSide::Below,
                );
            }
        }

        // Shift selection to track the text.
        let d_row = -scroll_delta;
        if app.selection.is_dragging() {
            app.selection.shift_anchor(d_row, min_row, max_row);
        } else {
            app.selection.shift_for_follow(d_row, min_row, max_row);
        }
    }
    app.prev_scroll_offset = app.scroll_offset;

    // ── Inline permission overlay ───────────────────────────────
    if let Some(view) = &app.pending_permission_view {
        let render_y = result.render_y_end;
        let bottom = area.y.saturating_add(area.height);
        if render_y < bottom {
            let remaining = Rect {
                x: area.x,
                y: render_y,
                width: area.width,
                height: bottom - render_y,
            };
            let permission_result = render_permission_inline_with_cursor(
                view,
                remaining,
                frame.buffer_mut(),
                result.suffix_skip_lines,
            );
            if let Some(cursor) = permission_result.cursor {
                *cursor_hint = Some(cursor);
            }
        }
    }

    // ── Selection overlay ───────────────────────────────────────
    if app.selection_owner == SelectionOwner::Transcript {
        rebon_tui::apply_selection_overlay(&app.selection, frame.buffer_mut(), area);
    }

    // ── Snapshot current frame for next-frame capture ────────────
    // Save text lines so capture_from_snapshot can read them if
    // a scroll happens before the next render.
    app.prev_frame_lines = rebon_tui::selection::snapshot_area_text(frame.buffer_mut(), area);
    app.prev_frame_area = Some(area);

    // ── Deferred clipboard copy ─────────────────────────────────
    if app.pending_copy && app.selection_owner == SelectionOwner::Transcript {
        app.pending_copy = false;
        let text = app.selection.get_selected_text(frame.buffer_mut(), area);
        if !text.is_empty() {
            rebon_tui::selection::copy_to_clipboard_osc52(&text);
        }
    }
}
