use super::*;

pub(super) fn measure_message_height(
    rows: &[Message],
    row_revisions: &[u64],
    idx: usize,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    add_margin: bool,
    cache: &mut TranscriptMeasureCache,
    last_thinking_block_id: Option<&str>,
    extras: TranscriptRenderExtras<'_>,
) -> u16 {
    let msg = &rows[idx];
    let live_activity_signature = message_live_agent_activity_signature(msg, extras);
    // Anything with a stable uuid + a real per-row revision is
    // cacheable. Messages without a uuid (the open-world `Unknown`
    // fallback) or rows where the parallel revision data is missing
    // fall through to a direct measure — those are rare enough that
    // the missed cache is a non-event.
    let cache_key = match (msg.uuid(), row_revisions.get(idx).copied()) {
        (Some(uuid), Some(rev)) if rev != 0 => Some((uuid, rev)),
        _ => None,
    };

    if let Some((uuid, row_revision)) = cache_key {
        if let Some(height) = cache.row_heights.get(
            uuid,
            area.width,
            row_revision,
            add_margin,
            last_thinking_block_id,
            live_activity_signature,
        ) {
            return height;
        }
    }

    let measure_theme = theme.without_math_graphics();
    let measured = render_message_inner_with_context(
        msg,
        area,
        buf,
        &measure_theme,
        verbosity,
        add_margin,
        last_thinking_block_id,
        true,
        extras,
    );

    if let Some((uuid, row_revision)) = cache_key {
        cache.row_heights.insert(
            uuid.to_string(),
            area.width,
            row_revision,
            add_margin,
            last_thinking_block_id,
            live_activity_signature,
            measured,
        );
    }

    measured
}
