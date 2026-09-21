use super::*;

/// Result of [`render_transcript`], providing the total virtual
/// content height so callers can manage scroll bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptStickyAnchor {
    pub row_index: usize,
    pub scroll_offset: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct TranscriptRenderResult {
    /// Total height of all committed messages + streaming overlay
    /// + suffix, in terminal lines. Used by the caller to clamp
    /// scroll offset and implement follow-tail.
    pub total_lines: usize,
    /// The Y coordinate in the buffer where transcript rendering
    /// stopped. Callers that pass `suffix_height > 0` should render
    /// their suffix content starting at this Y, up to
    /// `area.y + area.height`. The suffix may need partial clipping
    /// when the scroll offset falls inside it.
    pub render_y_end: u16,
    /// Number of suffix lines that were skipped (clipped above the
    /// viewport) because the scroll offset falls inside the suffix
    /// region. The caller should skip this many lines when rendering
    /// its suffix content.
    pub suffix_skip_lines: usize,
    pub sticky_anchor: Option<TranscriptStickyAnchor>,
}

/// Paint the transcript with **line-level scrolling**.
///
/// `scroll_offset` is the number of terminal lines to skip from
/// the top of the virtual content. This gives smooth per-line
/// scrolling instead of the earlier per-message jumping.
///
/// `suffix_height` is the number of extra terminal lines the caller
/// will render after the transcript (e.g. an inline permission
/// dialog). These lines are included in `total_lines` so scroll
/// math is correct, but `render_transcript` does not paint them —
/// the caller renders them at `render_y_end` using `suffix_skip_lines`
/// for partial-clipping when the scroll offset falls inside the
/// suffix region. This keeps the overlay (the permission request)
/// inside the scroll region, after the scrollable message content.
///
/// Returns [`TranscriptRenderResult`] with `total_lines` so callers
/// can compute `max_scroll_offset = total_lines - viewport_height`.
pub fn render_transcript(
    state: &AppState,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    scroll_offset: usize,
    verbosity: ToolOutputVerbosity,
    suffix_height: usize,
    divider_before_index: Option<usize>,
) -> TranscriptRenderResult {
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        state,
        area,
        buf,
        theme,
        scroll_offset,
        verbosity,
        suffix_height,
        divider_before_index,
        &mut cache,
        false,
        TranscriptRenderExtras::empty(),
    )
}

pub fn render_transcript_cached(
    state: &AppState,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    scroll_offset: usize,
    verbosity: ToolOutputVerbosity,
    suffix_height: usize,
    divider_before_index: Option<usize>,
    cache: &mut TranscriptMeasureCache,
) -> TranscriptRenderResult {
    render_transcript_cached_with_running_hints(
        state,
        area,
        buf,
        theme,
        scroll_offset,
        verbosity,
        suffix_height,
        divider_before_index,
        cache,
        false,
        TranscriptRenderExtras::empty(),
    )
}

pub fn render_transcript_cached_with_running_hints(
    state: &AppState,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    scroll_offset: usize,
    verbosity: ToolOutputVerbosity,
    suffix_height: usize,
    divider_before_index: Option<usize>,
    cache: &mut TranscriptMeasureCache,
    show_running_transcript_hints: bool,
    extras: TranscriptRenderExtras<'_>,
) -> TranscriptRenderResult {
    render_transcript_cached_with_running_hints_at(
        state,
        area,
        buf,
        theme,
        scroll_offset,
        verbosity,
        suffix_height,
        divider_before_index,
        cache,
        show_running_transcript_hints,
        extras,
        wall_clock_ms(),
    )
}

pub(super) fn render_transcript_cached_with_running_hints_at(
    state: &AppState,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    scroll_offset: usize,
    verbosity: ToolOutputVerbosity,
    suffix_height: usize,
    divider_before_index: Option<usize>,
    cache: &mut TranscriptMeasureCache,
    show_running_transcript_hints: bool,
    extras: TranscriptRenderExtras<'_>,
    render_time_ms: u64,
) -> TranscriptRenderResult {
    clear_buffer_area(buf, area);

    let rows = state.transcript.rows();
    let row_revisions = state.transcript.row_revisions();
    let viewport_height = area.height as usize;
    let math_layout_enabled = theme.math_display.math_enabled();
    cache.prepare(verbosity, math_layout_enabled, extras);

    let latest_visible_thinking_block_id = if let Some(layout) = cache.layout.as_ref() {
        if state.overlay.is_empty()
            && layout.transcript_revision == state.transcript.revision()
            && layout.render_key.matches_without_latest_thinking(
                area.width,
                verbosity,
                divider_before_index,
                show_running_transcript_hints,
                math_layout_enabled,
                extras,
            )
            && layout_matches_rows(layout, rows, row_revisions, extras)
        {
            layout.render_key.latest_visible_thinking_block_id.clone()
        } else {
            latest_visible_thinking_block_id_for_render(state, verbosity, extras)
        }
    } else {
        latest_visible_thinking_block_id_for_render(state, verbosity, extras)
    };

    let render_key = TranscriptLayoutRenderKey::new(
        area.width,
        verbosity,
        divider_before_index,
        show_running_transcript_hints,
        math_layout_enabled,
        latest_visible_thinking_block_id,
        extras,
    );
    let layout = transcript_layout_for_render(
        state.transcript.revision(),
        rows,
        row_revisions,
        render_key,
        theme,
        cache,
        extras,
    );

    let scratch_rect = Rect {
        x: 0,
        y: 0,
        width: area.width,
        height: TRANSCRIPT_MEASURE_HEIGHT,
    };
    let mut scratch = Buffer::empty(scratch_rect);
    let overlay_h = if !state.overlay.is_empty() {
        render_streaming_overlay_with_options(
            &state.overlay,
            scratch_rect,
            &mut scratch,
            theme,
            verbosity,
            &mut cache.streaming_overlay,
            StreamingOverlayRenderOptions::measure(
                !rows.is_empty() || extras.leading_segment_margin,
            ),
            extras,
        )
    } else {
        cache.streaming_overlay.clear();
        0
    };

    let total_msg_lines = layout.total_msg_lines;
    let content_lines = total_msg_lines + overlay_h as usize;
    let total_lines = content_lines + suffix_height;

    if total_lines == 0 || viewport_height == 0 {
        return TranscriptRenderResult {
            total_lines,
            render_y_end: area.y,
            suffix_skip_lines: 0,
            sticky_anchor: None,
        };
    }

    // Clamp scroll so the last line sits at the viewport bottom, not
    // the top. This matches the scroll_down_lines max_offset calc
    // and lets callers pass usize::MAX to mean "snap to bottom using
    // freshly measured total_lines" (used by follow-tail).
    let scroll_offset = scroll_offset.min(total_lines.saturating_sub(viewport_height));
    let first_visible = layout.first_visible_segment(scroll_offset);
    let first_visible_idx = first_visible.map(|start| start.segment_idx);

    let sticky_anchor = if scroll_offset > 0 {
        first_visible_idx.and_then(|idx| layout.sticky_anchors.get(idx).copied().flatten())
    } else {
        None
    };

    let render_y = {
        let mut paint = TranscriptPaintStage {
            state,
            rows,
            row_revisions,
            layout: &layout,
            area,
            buf,
            theme,
            verbosity,
            cache,
            extras,
            render_time_ms,
            render_y: area.y,
            bottom: area.y.saturating_add(area.height),
        };
        paint_visible_transcript_segments(&mut paint, first_visible);
        paint_streaming_transcript_overlay(&mut paint, scroll_offset, total_msg_lines, overlay_h);
        paint.render_y
    };

    // ── Suffix skip calculation ─────────────────────────────────
    // If the scroll offset falls inside the suffix region, compute
    // how many suffix lines are above the viewport so the caller
    // can clip its own rendering.
    let suffix_skip_lines = if suffix_height > 0 && scroll_offset > content_lines {
        scroll_offset - content_lines
    } else {
        0
    };

    TranscriptRenderResult {
        total_lines,
        render_y_end: render_y,
        suffix_skip_lines,
        sticky_anchor,
    }
}

struct TranscriptPaintStage<'render, 'buffer> {
    state: &'render AppState,
    rows: &'render [Message],
    row_revisions: &'render [u64],
    layout: &'render TranscriptLayout,
    area: Rect,
    buf: &'buffer mut Buffer,
    theme: &'render RenderTheme,
    verbosity: ToolOutputVerbosity,
    cache: &'buffer mut TranscriptMeasureCache,
    extras: TranscriptRenderExtras<'render>,
    render_time_ms: u64,
    render_y: u16,
    bottom: u16,
}

fn paint_visible_transcript_segments(
    paint: &mut TranscriptPaintStage<'_, '_>,
    first_visible: Option<TranscriptVisibleStart>,
) {
    // Pre-build the divider theme from the design system so it's
    // ready when we hit the divider index.
    let ds_divider = rebon_design_system::theme::get_theme(paint.theme.name);
    let divider_style = Style::default()
        .fg(parse_theme_color(ds_divider.professionalBlue))
        .add_modifier(Modifier::BOLD);

    if let Some(start) = first_visible {
        for i in start.segment_idx..paint.layout.segments.len() {
            if paint.render_y >= paint.bottom {
                break;
            }

            // ── Unseen divider line ─────────────────────────────
            if paint.layout.divider_before_segment == Some(i)
                && paint.render_y < paint.bottom
                && (i != start.segment_idx || start.start_idx_shows_divider)
            {
                let w = paint.area.width as usize;
                let label = " new ";
                let pad = w.saturating_sub(label.len()) / 2;
                let left = "\u{2500}".repeat(pad.max(1));
                let right_len = w.saturating_sub(pad + label.len());
                let right = "\u{2500}".repeat(right_len.max(1));
                let divider_text = format!("{left}{label}{right}");
                let line = Line::from(Span::styled(divider_text, divider_style));
                let para = Paragraph::new(line);
                let divider_rect = Rect {
                    x: paint.area.x,
                    y: paint.render_y,
                    width: paint.area.width,
                    height: 1,
                };
                para.render(divider_rect, paint.buf);
                paint.render_y = paint.render_y.saturating_add(1);
                if paint.render_y >= paint.bottom {
                    break;
                }
            }

            let segment = &paint.layout.segments[i];

            if i == start.segment_idx && start.skip_lines_in_first > 0 {
                let seg_h = paint.layout.seg_heights[i];
                let clip_rect = Rect {
                    x: 0,
                    y: 0,
                    width: paint.area.width,
                    height: seg_h,
                };
                let add_margin = i > 0 || paint.extras.leading_segment_margin;
                let show_collapsed_hint_lines =
                    running_hint_segment_for_layout(paint.layout) == Some(i);
                let clip_theme = paint.theme.without_math_graphics();
                let clip_key = ClippedSegmentCacheKey::new(
                    paint.rows,
                    paint.row_revisions,
                    segment,
                    paint.area.width,
                    seg_h,
                    paint.verbosity,
                    &clip_theme,
                    add_margin,
                    show_collapsed_hint_lines,
                    paint
                        .layout
                        .render_key
                        .latest_visible_thinking_block_id
                        .as_deref(),
                    paint.extras,
                    paint.render_time_ms,
                );
                let clip_buf = if let Some(cached) = paint.cache.clipped_segments.get(&clip_key) {
                    #[cfg(test)]
                    {
                        paint.cache.clipped_segment_hits += 1;
                    }
                    cached
                } else {
                    let mut clip_buf = Buffer::empty(clip_rect);
                    render_segment(
                        paint.rows,
                        segment,
                        clip_rect,
                        &mut clip_buf,
                        &clip_theme,
                        paint.verbosity,
                        add_margin,
                        show_collapsed_hint_lines,
                        paint
                            .layout
                            .render_key
                            .latest_visible_thinking_block_id
                            .as_deref(),
                        paint.extras,
                        paint.render_time_ms,
                    );
                    paint.cache.clipped_segments.insert(clip_key, clip_buf)
                };

                let visible = (seg_h as usize)
                    .saturating_sub(start.skip_lines_in_first)
                    .min((paint.bottom - paint.render_y) as usize)
                    as u16;
                copy_buffer_region(
                    clip_buf.as_ref(),
                    start.skip_lines_in_first as u16,
                    paint.buf,
                    paint.render_y,
                    paint.area.x,
                    paint.area.width,
                    visible,
                );
                paint.render_y = paint.render_y.saturating_add(visible);
            } else {
                let remaining = paint.bottom - paint.render_y;
                let sub = Rect {
                    x: paint.area.x,
                    y: paint.render_y,
                    width: paint.area.width,
                    height: remaining,
                };
                let used = render_segment(
                    paint.rows,
                    segment,
                    sub,
                    paint.buf,
                    paint.theme,
                    paint.verbosity,
                    i > 0 || paint.extras.leading_segment_margin,
                    running_hint_segment_for_layout(paint.layout) == Some(i),
                    paint
                        .layout
                        .render_key
                        .latest_visible_thinking_block_id
                        .as_deref(),
                    paint.extras,
                    paint.render_time_ms,
                );
                paint.render_y = paint.render_y.saturating_add(used);
            }
        }
    }
}

fn paint_streaming_transcript_overlay(
    paint: &mut TranscriptPaintStage<'_, '_>,
    scroll_offset: usize,
    total_msg_lines: usize,
    overlay_h: u16,
) {
    // ── Streaming overlay ────────────────────────────────────────
    if paint.render_y < paint.bottom && !paint.state.overlay.is_empty() {
        // Check if the overlay itself needs partial clipping
        // (scroll_offset falls inside the overlay region).
        let overlay_virtual_top = total_msg_lines;
        if scroll_offset > overlay_virtual_top {
            // Overlay is partially above viewport.
            let overlay_skip = scroll_offset - overlay_virtual_top;
            let overlay_clip_rect = Rect {
                x: 0,
                y: 0,
                width: paint.area.width,
                height: overlay_h,
            };
            let mut clip_buf = Buffer::empty(overlay_clip_rect);
            render_streaming_overlay_with_options(
                &paint.state.overlay,
                overlay_clip_rect,
                &mut clip_buf,
                paint.theme,
                paint.verbosity,
                &mut paint.cache.streaming_overlay.clone(),
                StreamingOverlayRenderOptions::scratch_paint(
                    !paint.rows.is_empty() || paint.extras.leading_segment_margin,
                ),
                paint.extras,
            );

            let visible = (overlay_h as usize)
                .saturating_sub(overlay_skip)
                .min((paint.bottom - paint.render_y) as usize) as u16;
            copy_buffer_region(
                &clip_buf,
                overlay_skip as u16,
                paint.buf,
                paint.render_y,
                paint.area.x,
                paint.area.width,
                visible,
            );
            paint.render_y = paint.render_y.saturating_add(visible);
        } else {
            let sub = Rect {
                x: paint.area.x,
                y: paint.render_y,
                width: paint.area.width,
                height: paint.bottom - paint.render_y,
            };
            let overlay_leading_margin =
                !paint.rows.is_empty() || paint.extras.leading_segment_margin;
            let used = render_streaming_overlay_with_options(
                &paint.state.overlay,
                sub,
                paint.buf,
                paint.theme,
                paint.verbosity,
                &mut paint.cache.streaming_overlay,
                if overlay_leading_margin {
                    StreamingOverlayRenderOptions::paint_after_committed()
                } else {
                    StreamingOverlayRenderOptions::paint()
                },
                paint.extras,
            );
            paint.render_y = paint.render_y.saturating_add(used);
        }
    }
}

fn latest_visible_thinking_block_id_for_render(
    state: &AppState,
    verbosity: ToolOutputVerbosity,
    extras: TranscriptRenderExtras<'_>,
) -> Option<String> {
    if matches!(verbosity, ToolOutputVerbosity::Compact)
        && !extras.expand_thinking_rows
        && !extras.render_thinking_only_rows
    {
        find_latest_visible_thinking_block_id(state.transcript.rows(), &state.overlay)
    } else {
        None
    }
}

fn transcript_layout_for_render(
    transcript_revision: u64,
    rows: &[Message],
    row_revisions: &[u64],
    render_key: TranscriptLayoutRenderKey,
    theme: &RenderTheme,
    cache: &mut TranscriptMeasureCache,
    extras: TranscriptRenderExtras<'_>,
) -> std::sync::Arc<TranscriptLayout> {
    let previous = cache.layout.as_ref().cloned();
    if let Some(layout) = previous.as_ref() {
        if layout.transcript_revision == transcript_revision
            && layout.render_key == render_key
            && layout_matches_rows(layout, rows, row_revisions, extras)
        {
            #[cfg(test)]
            {
                cache.layout_cache_hits += 1;
            }
            return std::sync::Arc::clone(layout);
        }
    }

    let row_signatures = TranscriptRowSignature::collect(rows, row_revisions, extras);
    if let Some(layout) = previous.as_ref() {
        if layout.render_key == render_key {
            if let Some(updated) = try_live_activity_update_layout(
                layout,
                transcript_revision,
                rows,
                row_revisions,
                &row_signatures,
                theme,
                cache,
                extras,
            ) {
                #[cfg(test)]
                {
                    cache.layout_activity_updates += 1;
                }
                let updated = std::sync::Arc::new(updated);
                cache.layout = Some(std::sync::Arc::clone(&updated));
                return updated;
            }

            if let Some(updated) = try_tail_revision_update_layout(
                layout,
                transcript_revision,
                rows,
                row_revisions,
                &row_signatures,
                theme,
                cache,
                extras,
            ) {
                #[cfg(test)]
                {
                    cache.layout_tail_updates += 1;
                }
                let updated = std::sync::Arc::new(updated);
                cache.layout = Some(std::sync::Arc::clone(&updated));
                return updated;
            }

            if let Some(updated) = try_append_layout(
                layout,
                transcript_revision,
                rows,
                row_revisions,
                &row_signatures,
                theme,
                cache,
                extras,
            ) {
                #[cfg(test)]
                {
                    cache.layout_incremental_appends += 1;
                }
                let updated = std::sync::Arc::new(updated);
                cache.layout = Some(std::sync::Arc::clone(&updated));
                return updated;
            }
        }
    }

    #[cfg(test)]
    {
        cache.layout_full_builds += 1;
    }
    let built = build_full_layout(
        transcript_revision,
        rows,
        row_revisions,
        row_signatures,
        render_key,
        theme,
        cache,
        extras,
    );
    let built = std::sync::Arc::new(built);
    cache.layout = Some(std::sync::Arc::clone(&built));
    built
}

/// Whether the cached layout was built for exactly these rows. The fast path
/// may only trust `(transcript_revision, render_key)` equality when the cache
/// is fed by a single long-lived store, whose revision is monotonic. Stores
/// rebuilt per call via `TranscriptStore::from_rows` stamp a constant
/// synthetic revision, so two calls with IDENTICAL revisions can present
/// DIFFERENT rows — reusing the layout then applies the previous rows'
/// heights to the new rows (the inline commit path sized its scrollback
/// insert from exactly that stale height, clipping committed content). This
/// re-check is allocation-free: it compares uuid/revision in place instead of
/// collecting fresh `TranscriptRowSignature`s.
fn layout_matches_rows(
    layout: &TranscriptLayout,
    rows: &[Message],
    row_revisions: &[u64],
    extras: TranscriptRenderExtras<'_>,
) -> bool {
    layout.row_count == rows.len()
        && layout.row_signatures.len() == rows.len()
        && layout
            .row_signatures
            .iter()
            .zip(rows.iter())
            .enumerate()
            .all(|(idx, (signature, row))| {
                signature.uuid.as_deref() == row.uuid()
                    && signature.revision == row_revisions.get(idx).copied().unwrap_or(0)
                    && signature.live_activity_signature
                        == message_live_agent_activity_signature(row, extras)
            })
}

fn build_full_layout(
    transcript_revision: u64,
    rows: &[Message],
    row_revisions: &[u64],
    row_signatures: Vec<TranscriptRowSignature>,
    render_key: TranscriptLayoutRenderKey,
    theme: &RenderTheme,
    cache: &mut TranscriptMeasureCache,
    extras: TranscriptRenderExtras<'_>,
) -> TranscriptLayout {
    let segments = build_transcript_segments_with_extras(rows, extras);
    let running_hint_segment = latest_running_transcript_hint_segment(
        rows,
        &segments,
        render_key.show_running_transcript_hints,
    );
    let seg_heights = measure_layout_segments(
        rows,
        row_revisions,
        &segments,
        theme,
        cache,
        extras,
        &render_key,
        running_hint_segment,
    );
    TranscriptLayout::new(
        render_key,
        transcript_revision,
        row_signatures,
        segments,
        seg_heights,
        running_hint_segment,
    )
}

fn try_live_activity_update_layout(
    previous: &TranscriptLayout,
    transcript_revision: u64,
    rows: &[Message],
    row_revisions: &[u64],
    row_signatures: &[TranscriptRowSignature],
    theme: &RenderTheme,
    cache: &mut TranscriptMeasureCache,
    extras: TranscriptRenderExtras<'_>,
) -> Option<TranscriptLayout> {
    if previous.transcript_revision != transcript_revision
        || previous.row_count != rows.len()
        || previous.row_signatures.len() != row_signatures.len()
    {
        return None;
    }

    let mut changed_rows = Vec::new();
    for (idx, (old, new)) in previous
        .row_signatures
        .iter()
        .zip(row_signatures.iter())
        .enumerate()
    {
        if old.uuid != new.uuid
            || old.revision != new.revision
            || old.sticky_anchor != new.sticky_anchor
        {
            return None;
        }
        if old.live_activity_signature != new.live_activity_signature {
            changed_rows.push(idx);
        }
    }
    if changed_rows.is_empty() {
        return None;
    }

    let changed_segments = previous
        .segments
        .iter()
        .enumerate()
        .filter_map(|(segment_idx, segment)| {
            changed_rows
                .iter()
                .any(|row_idx| segment.contains(*row_idx))
                .then_some(segment_idx)
        })
        .collect::<Vec<_>>();
    if changed_segments.is_empty() {
        return None;
    }

    let mut layout = previous.clone();
    for segment_idx in changed_segments {
        let height = measure_one_layout_segment(
            rows,
            row_revisions,
            &layout.segments[segment_idx],
            theme,
            cache,
            extras,
            &layout.render_key,
            segment_idx,
            layout.running_hint_segment == Some(segment_idx),
        );
        layout.seg_heights[segment_idx] = height;
    }
    layout.row_signatures = row_signatures.to_vec();
    layout.recompute_metrics();
    Some(layout)
}

fn try_append_layout(
    previous: &TranscriptLayout,
    transcript_revision: u64,
    rows: &[Message],
    row_revisions: &[u64],
    row_signatures: &[TranscriptRowSignature],
    theme: &RenderTheme,
    cache: &mut TranscriptMeasureCache,
    extras: TranscriptRenderExtras<'_>,
) -> Option<TranscriptLayout> {
    if previous.render_key.show_running_transcript_hints
        || previous.row_count >= rows.len()
        || row_signatures.len() != rows.len()
        || !row_signatures
            .get(..previous.row_count)?
            .eq(previous.row_signatures.as_slice())
        || !append_boundary_is_stable(rows, previous.row_count)
    {
        return None;
    }

    let mut appended_segments =
        build_transcript_segments_with_extras(&rows[previous.row_count..], extras)
            .into_iter()
            .map(|segment| offset_segment(segment, previous.row_count))
            .collect::<Vec<_>>();

    let mut layout = previous.clone();
    let base_segment_count = layout.segments.len();
    let mut appended_heights = Vec::with_capacity(appended_segments.len());
    for (offset, segment) in appended_segments.iter().enumerate() {
        let segment_idx = base_segment_count + offset;
        appended_heights.push(measure_one_layout_segment(
            rows,
            row_revisions,
            segment,
            theme,
            cache,
            extras,
            &layout.render_key,
            segment_idx,
            false,
        ));
    }

    layout.transcript_revision = transcript_revision;
    layout.row_count = rows.len();
    layout.row_signatures = row_signatures.to_vec();
    layout.segments.append(&mut appended_segments);
    layout.seg_heights.extend(appended_heights);
    layout.recompute_metrics();
    Some(layout)
}

fn try_tail_revision_update_layout(
    previous: &TranscriptLayout,
    transcript_revision: u64,
    rows: &[Message],
    row_revisions: &[u64],
    row_signatures: &[TranscriptRowSignature],
    theme: &RenderTheme,
    cache: &mut TranscriptMeasureCache,
    extras: TranscriptRenderExtras<'_>,
) -> Option<TranscriptLayout> {
    let row_count = previous.row_count;
    if row_count == 0
        || row_count != rows.len()
        || row_signatures.len() != row_count
        || previous.row_signatures.len() != row_count
        || !row_signatures
            .get(..row_count.saturating_sub(1))?
            .eq(&previous.row_signatures[..row_count.saturating_sub(1)])
        || row_signatures[row_count - 1].uuid != previous.row_signatures[row_count - 1].uuid
        || row_signatures[row_count - 1].revision == previous.row_signatures[row_count - 1].revision
        || !append_boundary_is_stable(rows, row_count - 1)
        || !single_tail_segment_is_stable(rows, row_count - 1, extras)
    {
        return None;
    }

    let segment_idx = previous.segments.len().checked_sub(1)?;
    if !matches!(previous.segments.get(segment_idx), Some(TranscriptSegment::Single(idx)) if *idx == row_count - 1)
    {
        return None;
    }

    let mut layout = previous.clone();
    let new_height = measure_one_layout_segment(
        rows,
        row_revisions,
        &layout.segments[segment_idx],
        theme,
        cache,
        extras,
        &layout.render_key,
        segment_idx,
        false,
    );
    layout.transcript_revision = transcript_revision;
    layout.row_signatures = row_signatures.to_vec();
    layout.seg_heights[segment_idx] = new_height;
    layout.recompute_metrics();
    Some(layout)
}

fn measure_layout_segments(
    rows: &[Message],
    row_revisions: &[u64],
    segments: &[TranscriptSegment],
    theme: &RenderTheme,
    cache: &mut TranscriptMeasureCache,
    extras: TranscriptRenderExtras<'_>,
    render_key: &TranscriptLayoutRenderKey,
    running_hint_segment: Option<usize>,
) -> Vec<u16> {
    segments
        .iter()
        .enumerate()
        .map(|(idx, segment)| {
            measure_one_layout_segment(
                rows,
                row_revisions,
                segment,
                theme,
                cache,
                extras,
                render_key,
                idx,
                running_hint_segment == Some(idx),
            )
        })
        .collect()
}

fn measure_one_layout_segment(
    rows: &[Message],
    row_revisions: &[u64],
    segment: &TranscriptSegment,
    theme: &RenderTheme,
    cache: &mut TranscriptMeasureCache,
    extras: TranscriptRenderExtras<'_>,
    render_key: &TranscriptLayoutRenderKey,
    segment_idx: usize,
    show_collapsed_hint_lines: bool,
) -> u16 {
    let scratch_rect = Rect {
        x: 0,
        y: 0,
        width: render_key.width,
        height: TRANSCRIPT_MEASURE_HEIGHT,
    };
    let mut scratch = Buffer::empty(scratch_rect);
    measure_segment_height(
        rows,
        row_revisions,
        segment,
        scratch_rect,
        &mut scratch,
        theme,
        render_key.verbosity,
        segment_idx > 0 || render_key.leading_segment_margin,
        cache,
        show_collapsed_hint_lines,
        render_key.latest_visible_thinking_block_id.as_deref(),
        extras,
    )
}

fn append_boundary_is_stable(rows: &[Message], old_count: usize) -> bool {
    if old_count == 0 {
        return true;
    }
    let Some(last_idx) = (0..old_count).rev().find(|idx| !is_meta_user(&rows[*idx])) else {
        return true;
    };
    let row = &rows[last_idx];
    if is_thinking_only_assistant(row) {
        return false;
    }
    if tool_only_assistant(row).is_some() || tool_result_only_user(row).is_some() {
        return false;
    }
    true
}

fn single_tail_segment_is_stable(
    rows: &[Message],
    tail_idx: usize,
    extras: TranscriptRenderExtras<'_>,
) -> bool {
    let segments = build_transcript_segments_with_extras(&rows[tail_idx..], extras);
    matches!(segments.as_slice(), [TranscriptSegment::Single(0)])
}

fn offset_segment(segment: TranscriptSegment, offset: usize) -> TranscriptSegment {
    match segment {
        TranscriptSegment::Single(idx) => TranscriptSegment::Single(idx + offset),
        TranscriptSegment::Collapsed { indices } => TranscriptSegment::Collapsed {
            indices: indices.into_iter().map(|idx| idx + offset).collect(),
        },
        TranscriptSegment::AgentGroup { indices } => TranscriptSegment::AgentGroup {
            indices: indices.into_iter().map(|idx| idx + offset).collect(),
        },
        TranscriptSegment::ThinkingGroup { segments } => TranscriptSegment::ThinkingGroup {
            segments: segments
                .into_iter()
                .map(|segment| offset_segment(segment, offset))
                .collect(),
        },
    }
}

fn running_hint_segment_for_layout(layout: &TranscriptLayout) -> Option<usize> {
    layout.running_hint_segment
}
