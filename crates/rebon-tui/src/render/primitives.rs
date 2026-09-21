use super::*;

/// Copy a horizontal band from `src` (rows `src_y..src_y+height`) into
/// `dst` at `(dst_x, dst_y)`. Used for partial-message clipping.
pub(super) fn copy_buffer_region(
    src: &Buffer,
    src_y: u16,
    dst: &mut Buffer,
    dst_y: u16,
    dst_x: u16,
    width: u16,
    height: u16,
) {
    for dy in 0..height {
        for dx in 0..width {
            if let Some(cell) = src.cell((dx, src_y + dy)) {
                if let Some(dst_cell) = dst.cell_mut((dst_x + dx, dst_y + dy)) {
                    *dst_cell = cell.clone();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Low-level helpers
// ---------------------------------------------------------------------------

pub(super) fn render_wrapped(
    text: &str,
    gutter: &str,
    gutter_style: Style,
    body_style: Style,
    area: Rect,
    buf: &mut Buffer,
) -> u16 {
    if area.height == 0 || area.width == 0 {
        return 0;
    }

    let content_width = area.width.saturating_sub(GUTTER).max(1);

    // Build content lines without inline prefix — the gutter column
    // handles the prefix separately so visual wrapping stays aligned.
    let lines: Vec<Line<'static>> = text
        .split('\n')
        .map(|line| Line::from(Span::styled(line.to_string(), body_style)))
        .collect();

    let requested = wrap_height(text, content_width);
    let actual = requested.max(1).min(area.height);

    // Mirror the defensive clear in `render_gutter_lines`: when the
    // previous frame painted a wider/taller body into this rect and the
    // current frame's content shrinks, ratatui's wrap-paragraph leaves
    // unfilled cells alone. Resetting them here keeps the invariant
    // local to the paint function so the fix can't regress.
    clear_buffer_area(
        buf,
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: actual,
        },
    );

    // Paint gutter symbol on the first row.
    if GUTTER <= area.width {
        let gutter_line = Line::from(Span::styled(format!("{gutter} "), gutter_style));
        Paragraph::new(gutter_line).render(
            Rect {
                x: area.x,
                y: area.y,
                width: GUTTER,
                height: 1,
            },
            buf,
        );
    }

    // Paint content text in the right column — wrapping is confined to
    // content_width so continuation lines stay indented past the gutter.
    Paragraph::new(lines).wrap(Wrap { trim: false }).render(
        Rect {
            x: area.x.saturating_add(GUTTER),
            y: area.y,
            width: content_width,
            height: actual,
        },
        buf,
    );
    actual
}

/// Like [`render_wrapped`] but accepts pre-built [`Line`]s so callers
/// can use multiple [`Span`]s with different styles on a single line.
///
/// `text_for_height` is the plain-text equivalent used only for
/// `wrap_height` calculation — it must match the semantic content of
/// `lines` (line count, approximate char width) for correct sizing.
pub(super) fn render_gutter_lines(
    lines: Vec<Line<'static>>,
    text_for_height: &str,
    gutter: &str,
    gutter_style: Style,
    area: Rect,
    buf: &mut Buffer,
) -> u16 {
    if area.height == 0 || area.width == 0 {
        return 0;
    }
    let content_width = area.width.saturating_sub(GUTTER).max(1);
    let requested = wrap_height_visible(text_for_height, content_width);
    let actual = requested.max(1).min(area.height);

    // Reset the cells we're about to paint into. The transcript-area
    // entry already cleared `area`, but a Read tool whose body shrank
    // between frames (Verbose card → Compact summary, or output trimmed)
    // can still leave stale glyphs when the previous frame's
    // `render_gutter_lines` painted into rows the current call no longer
    // covers — `Paragraph::wrap` only writes the cells the current
    // wrapped lines occupy and skips the rest. Clearing here is cheap
    // and pins the invariant locally so the fix can't regress when an
    // upstream caller forgets the area-level clear.
    clear_buffer_area(
        buf,
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: actual,
        },
    );

    if GUTTER <= area.width {
        let gutter_line = Line::from(Span::styled(format!("{gutter} "), gutter_style));
        Paragraph::new(gutter_line).render(
            Rect {
                x: area.x,
                y: area.y,
                width: GUTTER,
                height: 1,
            },
            buf,
        );
    }

    Paragraph::new(lines).wrap(Wrap { trim: false }).render(
        Rect {
            x: area.x.saturating_add(GUTTER),
            y: area.y,
            width: content_width,
            height: actual,
        },
        buf,
    );
    actual
}
