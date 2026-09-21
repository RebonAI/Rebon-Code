//! Ratatui renderer for the prompt-input surface.
//!
//! Draws the visible prompt input — the text area, the
//! highlight spans, the placeholder fallback, and the cursor position.
//! This module deliberately keeps only the rendering concerns; all
//! state logic, highlight composition, suggestion routing, and
//! cursor snapping already live in [`crate::promptinput`] and are
//! consumed here as plain data via [`PromptInputRuntimeState`].
//!
//! ## Rendering coverage
//!
//! * Displayed value → spans, split on explicit `\n` for the
//!   multi-line case mirroring `is_input_wrapped`.
//! * Placeholder rendering as a dim single-line affordance when the
//!   input is empty and a placeholder is set.
//! * Highlight → ratatui style via [`highlight_style`], honoring
//!   `inverse`, `dim_color`, and a small set of theme color keys
//!   (`"warning"`, `"suggestion"`, history
//!   match). Priority is resolved by sorting ascending and writing
//!   later → higher-priority styles overwrite lower on overlap.
//! * Cursor position computation in terminal columns via
//!   `unicode-width` so CJK / emoji / combining marks line up with
//!   the glyph grid the same way the transcript renderer does.
//! * `snapped_cursor_offset` override for the "cursor inside image
//!   chip" case — when set, rendering uses the snapped offset so the
//!   caller can keep the raw cursor in `AppState` unchanged.
//! * `text_input_view.show_cursor` gating — when false (history
//!   search / footer selected / cursor on image chip) the returned
//!   [`PromptInputRenderResult::cursor`] is `None` so the caller
//!   does not position the hardware cursor.
//!
//! ## Deliberately omitted behavior
//!
//! * Shimmer animation for rainbow highlights. `shimmer_color` is
//!   ignored; the static `color` is used. Animation lands when the
//!   caller has a tick source.
//! * Border / frame chrome — the caller is expected to pass an
//!   inner `area` that already excludes any outer block borders.
//!
//! These deferrals keep the render function focused on the state →
//! buffer translation, same as the transcript renderer in
//! [`crate::render`].

use crate::input::{
    filter_and_remap_highlights, render_placeholder, HighlightViewportInput, PlaceholderInput,
    PlaceholderStyle, TextHighlight,
};
use crate::promptinput::{clamp_cursor_offset, PromptInputRuntimeState, PromptSurfaceHighlight};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use rebon_design_system::theme;
use rebon_width::WidthStr;
use unicode_linebreak::linebreaks;
use unicode_segmentation::UnicodeSegmentation;

use crate::render::{parse_theme_color, RenderTheme};

/// Result of painting a prompt-input surface into a ratatui buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptInputRenderResult {
    /// Number of vertical rows the prompt input consumed.
    pub lines_used: u16,
    /// Absolute `(x, y)` screen position of the text cursor, or
    /// `None` when `text_input_view.show_cursor` is false. Callers
    /// can pass this straight to `ratatui::Frame::set_cursor_position`.
    pub cursor: Option<(u16, u16)>,
}

/// Unicode-aware visual layout for one logical prompt-input line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptInputLineLayout {
    /// Wrapped rows, with discarded wrap whitespace matching the prompt renderer.
    pub rows: Vec<String>,
    /// Number of rows required for both content and the optional cursor.
    pub lines_used: u16,
    /// Cursor `(column, row)` relative to the layout origin.
    pub cursor: Option<(u16, u16)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrapToken {
    Grapheme {
        start: usize,
        end: usize,
        width: u16,
        break_after: bool,
    },
    Cursor,
}

impl WrapToken {
    fn width(self) -> u16 {
        match self {
            Self::Grapheme { width, .. } => width,
            Self::Cursor => 0,
        }
    }
}

#[derive(Debug)]
struct WrappedLineLayout {
    rows: Vec<Vec<WrapToken>>,
    cursor_row: u16,
    cursor_col: u16,
}

impl PromptInputRenderResult {
    /// Empty result — used when `area` has zero width or height.
    pub const EMPTY: Self = Self {
        lines_used: 0,
        cursor: None,
    };
}

/// Lay out one logical input line with the same wrapping and cursor rules as
/// [`render_prompt_input`]. Cursor offsets are UTF-8 byte offsets.
pub fn layout_prompt_input_line(
    value: &str,
    cursor_byte_offset: Option<usize>,
    width: u16,
) -> PromptInputLineLayout {
    if width == 0 {
        return PromptInputLineLayout {
            rows: Vec::new(),
            lines_used: 0,
            cursor: None,
        };
    }

    let cursor_byte_offset = cursor_byte_offset.map(|offset| clamp_cursor_offset(value, offset));
    let layout = wrap_line(value, cursor_byte_offset, usize::from(width));
    let cursor = cursor_byte_offset.map(|_| (layout.cursor_col, layout.cursor_row));
    let lines_used = cursor
        .map(|(_, row)| row.saturating_add(1))
        .unwrap_or(0)
        .max(layout.rows.len() as u16);
    let mut rows: Vec<String> = layout
        .rows
        .iter()
        .map(|row| wrapped_row_text(value, row))
        .collect();
    rows.resize(usize::from(lines_used), String::new());

    PromptInputLineLayout {
        rows,
        lines_used,
        cursor,
    }
}

/// Render the prompt-input surface into `area` using the derived
/// [`PromptInputRuntimeState`] plus the raw cursor offset from the
/// caller's `AppState`.
///
/// The raw `cursor_offset` is passed in separately because
/// [`PromptInputRuntimeState`] does not carry it — the state struct
/// is derived from an input struct that owns it, and we do not want
/// to re-shape the derivation output just for the renderer. The
/// renderer honors `state.snapped_cursor_offset` when present so
/// the "cursor inside image chip" snap is applied consistently.
pub fn render_prompt_input(
    state: &PromptInputRuntimeState,
    cursor_offset: usize,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
) -> PromptInputRenderResult {
    if area.height == 0 || area.width == 0 {
        return PromptInputRenderResult::EMPTY;
    }

    // Reset every cell we own before painting. The prompt input shrinks
    // and grows between frames (multi-line wrap, placeholder ↔ value,
    // history search overlays) and the inner renderers below only write
    // into the exact cells their current line / span occupies. Without
    // this reset, ratatui's diff back-end leaves the prior frame's
    // glyphs in cells the new frame no longer touches and they show up
    // as residue in the input gutter. Matches the invariant enforced in
    // `rebon-tui::render::clear_buffer_area` for the transcript surface.
    crate::render::buffer_util::clear_buffer_area(buf, area);

    let effective_cursor = state.snapped_cursor_offset.unwrap_or(cursor_offset);
    let displayed = state.text_input_view.displayed_value.as_str();

    // Empty-input + placeholder branch. Uses the `input` module's full
    // placeholder decision tree (dim, inverse-first-char, voice cursor,
    // hidden modes) instead of a simple dim string.
    if displayed.is_empty() {
        let decision = render_placeholder(&PlaceholderInput {
            placeholder: state.text_input_view.placeholder.as_deref(),
            value: displayed,
            show_cursor: state.text_input_view.show_cursor,
            focus: true, // TUI prompt is always focused when visible
            terminal_focus: true,
            hide_placeholder_text: false,
        });

        let lines_used = match &decision.rendered_placeholder {
            PlaceholderStyle::None | PlaceholderStyle::HiddenEmpty => 0,
            PlaceholderStyle::HiddenCursorInverseSpace => {
                let line = Line::from(Span::styled(
                    " ".to_string(),
                    Style::new().add_modifier(Modifier::REVERSED),
                ));
                render_single_line(line, area, buf)
            }
            PlaceholderStyle::Dim { text } => {
                let ds_ph = theme::get_theme(theme.name);
                let line = Line::from(Span::styled(
                    text.clone(),
                    Style::new().fg(parse_theme_color(ds_ph.inactive)),
                ));
                render_single_line(line, area, buf)
            }
            PlaceholderStyle::FirstCharInverseRestDim { first, rest } => {
                let ds_ph = theme::get_theme(theme.name);
                let line = Line::from(vec![
                    Span::styled(
                        first.to_string(),
                        Style::new().add_modifier(Modifier::REVERSED),
                    ),
                    Span::styled(
                        rest.clone(),
                        Style::new().fg(parse_theme_color(ds_ph.inactive)),
                    ),
                ]);
                render_single_line(line, area, buf)
            }
        };
        // Show cursor at the start position when input is empty,
        // regardless of placeholder state — the cursor should always
        // be visible in the prompt when show_cursor is true.
        let cursor = if state.text_input_view.show_cursor {
            Some((area.x, area.y))
        } else {
            None
        };
        return PromptInputRenderResult { lines_used, cursor };
    }

    // Non-empty: split on explicit \n so multi-line input renders
    // one logical line per row. Long single lines use Unicode line-break
    // opportunities so CJK text can fill the remaining row width.

    // ── Cursor-aware highlight filtering (`input`) ──────────
    // Drop highlights that overlap the cursor position (unless
    // dim_color is set).
    // Viewport remap is a no-op (viewport_char_offset == 0) since
    // the TUI doesn't scroll horizontally within the prompt.
    let viewport_highlights: Vec<TextHighlight> = state
        .highlights
        .iter()
        .map(|h| TextHighlight {
            start: h.start,
            end: h.end,
            dim_color: h.dim_color,
        })
        .collect();
    let filtered_viewport = filter_and_remap_highlights(&HighlightViewportInput {
        highlights: &viewport_highlights,
        show_cursor: state.text_input_view.show_cursor,
        cursor_offset: effective_cursor,
        viewport_char_offset: 0,
        viewport_char_end: usize::MAX,
    });
    // Rebuild PromptSurfaceHighlight with filtered ranges. We keep
    // the original highlight metadata (color, priority, inverse) by
    // matching on (start, end) — since the filter only drops entries
    // or clips them, we map each surviving TextHighlight back.
    let filtered_highlights: Vec<PromptSurfaceHighlight> = filtered_viewport
        .iter()
        .filter_map(|fh| {
            state
                .highlights
                .iter()
                .find(|oh| oh.start == fh.start && oh.end == fh.end && oh.dim_color == fh.dim_color)
                .cloned()
        })
        .collect();

    let logical_lines: Vec<&str> = displayed.split('\n').collect();
    let mut rendered_lines: Vec<Line<'static>> = Vec::with_capacity(logical_lines.len());

    // Single pass: detect the cursor line, compute visual wrapping, and
    // build the styled rows from the same layout used for cursor placement.
    let aw = area.width.max(1) as usize;
    let mut line_start_offset = 0usize;
    let mut visual_total: u16 = 0;
    let mut cursor_visual_row: u16 = 0;
    let mut cursor_visual_col: u16 = 0;
    let mut cursor_found = false;

    for (idx, logical) in logical_lines.iter().enumerate() {
        let line_end_offset = line_start_offset + logical.len();

        let cursor_in_line = if !cursor_found
            && effective_cursor >= line_start_offset
            && effective_cursor <= line_end_offset
        {
            cursor_found = true;
            Some(effective_cursor - line_start_offset)
        } else {
            None
        };

        let layout = wrap_line(logical, cursor_in_line, aw);
        let vrows = layout.rows.len() as u16;

        if cursor_in_line.is_some() {
            cursor_visual_row = visual_total + layout.cursor_row;
            cursor_visual_col = layout.cursor_col;
        } else if !cursor_found && idx == logical_lines.len() - 1 {
            // Fallback: cursor past all lines → end of last line.
            let end_layout = wrap_line(logical, Some(logical.len()), aw);
            cursor_visual_row = visual_total + end_layout.cursor_row;
            cursor_visual_col = end_layout.cursor_col;
        }

        visual_total += vrows;

        rendered_lines.extend(layout.rows.iter().map(|row| {
            Line::from(build_wrapped_row_spans(
                logical,
                line_start_offset,
                row,
                &filtered_highlights,
                theme,
            ))
        }));

        // `+ 1` to account for the `\n` we split on.
        line_start_offset = line_end_offset + 1;
    }

    // Include the cursor row in the effective total so that when the
    // cursor sits at the exact wrap boundary of the last content row
    // (col == area_width) the extra empty row is allocated and the
    // cursor isn't clamped onto the content row above.
    let effective_total = if state.text_input_view.show_cursor {
        visual_total.max(cursor_visual_row + 1)
    } else {
        visual_total
    };

    // Scroll to keep the cursor row visible within the area.
    let scroll_y = if cursor_visual_row >= area.height {
        cursor_visual_row - area.height + 1
    } else {
        0
    };

    let lines_used = effective_total.saturating_sub(scroll_y).min(area.height);
    let draw_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: lines_used,
    };
    Paragraph::new(rendered_lines)
        .scroll((scroll_y, 0))
        .render(draw_area, buf);

    let cursor = if state.text_input_view.show_cursor {
        let vrow_screen = cursor_visual_row.saturating_sub(scroll_y);
        let vrow_clamped = vrow_screen.min(lines_used.saturating_sub(1));
        let vcol_clamped = cursor_visual_col.min(area.width.saturating_sub(1));
        Some((area.x + vcol_clamped, area.y + vrow_clamped))
    } else {
        None
    };

    PromptInputRenderResult { lines_used, cursor }
}

/// Wrap one logical input line using Unicode line-break opportunities.
/// Returns `(visual_rows, cursor_visual_row_offset, cursor_visual_col)`.
#[cfg(test)]
fn wrap_metrics(logical: &str, cursor_byte_in_line: Option<usize>, aw: usize) -> (u16, u16, u16) {
    let layout = wrap_line(logical, cursor_byte_in_line, aw);
    (
        layout.rows.len() as u16,
        layout.cursor_row,
        layout.cursor_col,
    )
}

fn wrap_line(logical: &str, cursor_byte_in_line: Option<usize>, aw: usize) -> WrappedLineLayout {
    let max_line_width = aw.max(1) as u16;
    let break_offsets: Vec<usize> = linebreaks(logical).map(|(offset, _)| offset).collect();
    let mut rows: Vec<Vec<WrapToken>> = Vec::new();
    let mut current: Vec<WrapToken> = Vec::new();
    let mut current_width = 0u16;
    let mut last_break = None;
    let mut cursor_inserted = false;

    for (start, symbol) in UnicodeSegmentation::grapheme_indices(logical, true) {
        if !cursor_inserted && cursor_byte_in_line == Some(start) {
            current.push(WrapToken::Cursor);
            cursor_inserted = true;
        }

        let width = WidthStr::width(symbol) as u16;
        if width > max_line_width {
            continue;
        }

        let end = start + symbol.len();
        let break_after = break_offsets.binary_search(&end).is_ok();
        let is_whitespace = symbol.chars().all(char::is_whitespace);

        if width > 0 && current_width + width > max_line_width && is_whitespace {
            rows.push(std::mem::take(&mut current));
            current_width = 0;
            last_break = None;
            continue;
        }

        while width > 0 && current_width + width > max_line_width {
            if let Some(split_at) = last_break.filter(|split_at| *split_at > 0) {
                let carry = current.split_off(split_at);
                rows.push(std::mem::take(&mut current));
                current = carry;
            } else if current
                .iter()
                .any(|token| matches!(token, WrapToken::Grapheme { .. }))
            {
                rows.push(std::mem::take(&mut current));
            } else {
                break;
            }

            current_width = row_width(&current);
            last_break = last_break_index(&current);
        }

        current.push(WrapToken::Grapheme {
            start,
            end,
            width,
            break_after,
        });
        current_width += width;
        if break_after {
            last_break = Some(current.len());
        }
    }

    if !cursor_inserted && cursor_byte_in_line.is_some() {
        current.push(WrapToken::Cursor);
    }

    let trailing = if current
        .iter()
        .any(|token| matches!(token, WrapToken::Grapheme { .. }))
    {
        rows.push(current);
        Vec::new()
    } else {
        current
    };

    let (cursor_row, cursor_col) = cursor_position(&rows, &trailing, max_line_width);
    if rows.is_empty() {
        rows.push(Vec::new());
    }

    WrappedLineLayout {
        rows,
        cursor_row,
        cursor_col,
    }
}

fn row_width(tokens: &[WrapToken]) -> u16 {
    tokens.iter().copied().map(WrapToken::width).sum()
}

fn last_break_index(tokens: &[WrapToken]) -> Option<usize> {
    tokens.iter().enumerate().rev().find_map(|(index, token)| {
        matches!(
            token,
            WrapToken::Grapheme {
                break_after: true,
                ..
            }
        )
        .then_some(index + 1)
    })
}

fn cursor_position(
    rows: &[Vec<WrapToken>],
    trailing: &[WrapToken],
    max_line_width: u16,
) -> (u16, u16) {
    for (row_index, row) in rows.iter().enumerate() {
        if let Some(col) = cursor_col(row) {
            return normalize_cursor(row_index as u16, col, max_line_width);
        }
    }

    if let Some(col) = cursor_col(trailing) {
        return normalize_cursor(rows.len() as u16, col, max_line_width);
    }

    (0, 0)
}

fn cursor_col(tokens: &[WrapToken]) -> Option<u16> {
    let mut col = 0u16;
    for token in tokens {
        match token {
            WrapToken::Cursor => return Some(col),
            WrapToken::Grapheme { width, .. } => col += *width,
        }
    }
    None
}

fn normalize_cursor(row: u16, col: u16, max_line_width: u16) -> (u16, u16) {
    if col >= max_line_width {
        (row + 1, 0)
    } else {
        (row, col)
    }
}

/// Paint a single pre-built line into the first row of `area` and
/// return the number of rows consumed (0 or 1).
fn render_single_line(line: Line<'static>, area: Rect, buf: &mut Buffer) -> u16 {
    if area.height == 0 || area.width == 0 {
        return 0;
    }
    let sub = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    Paragraph::new(line).render(sub, buf);
    1
}

fn wrapped_row_ranges(row: &[WrapToken]) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();

    for token in row {
        let WrapToken::Grapheme { start, end, .. } = token else {
            continue;
        };
        match ranges.last_mut() {
            Some((_, range_end)) if *range_end == *start => *range_end = *end,
            _ => ranges.push((*start, *end)),
        }
    }

    ranges
}

fn wrapped_row_text(logical: &str, row: &[WrapToken]) -> String {
    wrapped_row_ranges(row)
        .into_iter()
        .map(|(start, end)| &logical[start..end])
        .collect()
}

fn build_wrapped_row_spans(
    logical: &str,
    line_start_offset: usize,
    row: &[WrapToken],
    highlights: &[PromptSurfaceHighlight],
    theme: &RenderTheme,
) -> Vec<Span<'static>> {
    wrapped_row_ranges(row)
        .into_iter()
        .flat_map(|(start, end)| {
            build_line_spans(
                &logical[start..end],
                line_start_offset + start,
                highlights,
                theme,
            )
        })
        .collect()
}

/// Build the spans for one logical line of the displayed value,
/// applying every highlight whose range overlaps the logical
/// `[line_start_offset, line_start_offset + logical.len())` range.
fn build_line_spans(
    logical: &str,
    line_start_offset: usize,
    highlights: &[PromptSurfaceHighlight],
    theme: &RenderTheme,
) -> Vec<Span<'static>> {
    if logical.is_empty() {
        return Vec::new();
    }

    // Step 1: compute a per-byte style override by walking
    // highlights in priority order. Lower priorities are applied
    // first, so higher priorities overwrite them on overlap.
    let mut sorted: Vec<&PromptSurfaceHighlight> = highlights.iter().collect();
    sorted.sort_by_key(|h| h.priority);

    let logical_end = line_start_offset + logical.len();
    let mut byte_style: Vec<Option<Style>> = vec![None; logical.len()];

    for hl in sorted {
        // Clip highlight to the current logical line.
        let clip_start = hl.start.max(line_start_offset);
        let clip_end = hl.end.min(logical_end);
        if clip_start >= clip_end {
            continue;
        }
        let style = highlight_style(hl, theme);
        for pos in (clip_start - line_start_offset)..(clip_end - line_start_offset) {
            byte_style[pos] = Some(style);
        }
    }

    // Step 2: walk chars and coalesce adjacent same-style runs.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut current: Option<Style> = None;
    let mut started = false;

    for (byte_idx, ch) in logical.char_indices() {
        let style = byte_style[byte_idx];
        if !started {
            current = style;
            buf.push(ch);
            started = true;
            continue;
        }
        if style == current {
            buf.push(ch);
        } else {
            spans.push(make_span(std::mem::take(&mut buf), current));
            current = style;
            buf.push(ch);
        }
    }
    if !buf.is_empty() {
        spans.push(make_span(buf, current));
    }
    spans
}

fn make_span(text: String, style: Option<Style>) -> Span<'static> {
    match style {
        Some(s) => Span::styled(text, s),
        None => Span::raw(text),
    }
}

/// Map a single [`PromptSurfaceHighlight`] to a ratatui style using
/// the currently configured [`RenderTheme`].
///
/// This supports the theme color keys actually emitted by the current
/// `build_prompt_highlights` routine in `crate::promptinput`:
///
/// * `"warning"` → `theme.system_warning`
/// * `"suggestion"` → active theme `suggestion`
/// * palette strings (`"#rrggbb"`, `"rgb(r,g,b)"`, `"ansi:name"`, etc.) → parsed theme color
///
/// Rainbow shimmer animation is not applied here — this path has no tick
/// source.
fn highlight_style(hl: &PromptSurfaceHighlight, theme: &RenderTheme) -> Style {
    let palette = theme::get_theme(theme.name);
    let mut style = match hl.color.as_deref() {
        Some("warning") => theme.system_warning,
        Some("suggestion") => Style::new()
            .fg(parse_theme_color(palette.suggestion))
            .add_modifier(Modifier::BOLD),
        Some(color) => Style::new().fg(parse_theme_color(color)),
        None => Style::new(),
    };
    if hl.dim_color {
        style = style.add_modifier(Modifier::DIM);
    }
    if hl.inverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::promptinput::{PromptSurfaceHighlight, TextInputViewState};

    fn new_buf(w: u16, h: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, w, h))
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        let mut s = String::new();
        for x in 0..buf.area().width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.trim_end().to_string()
    }

    fn state_with(
        displayed: &str,
        placeholder: Option<&str>,
        show_cursor: bool,
        snapped: Option<usize>,
        highlights: Vec<PromptSurfaceHighlight>,
    ) -> PromptInputRuntimeState {
        PromptInputRuntimeState {
            displayed_value: displayed.to_string(),
            show_prompt_suggestion: false,
            should_mark_prompt_suggestion_shown: false,
            should_reset_prompt_suggestion_for_timing: false,
            image_ref_positions: vec![],
            cursor_at_image_chip: false,
            snapped_cursor_offset: snapped,
            highlights,
            text_input_view: TextInputViewState {
                displayed_value: displayed.to_string(),
                placeholder: placeholder.map(str::to_string),
                disable_cursor_movement_for_up_down_keys: false,
                disable_escape_double_press: false,
                focus: true,
                show_cursor,
                undo_enabled: false,
                is_input_wrapped: displayed.contains('\n'),
            },
        }
    }

    #[test]
    fn plain_text_renders_and_positions_cursor() {
        let state = state_with("hello", None, true, None, vec![]);
        let mut buf = new_buf(20, 1);
        let result = render_prompt_input(
            &state,
            5,
            Rect::new(0, 0, 20, 1),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(row_text(&buf, 0), "hello");
        assert_eq!(result.lines_used, 1);
        assert_eq!(result.cursor, Some((5, 0)));
    }

    #[test]
    fn sentence_wraps_on_word_boundary_and_keeps_cursor_aligned() {
        let state = state_with("hello world", None, true, None, vec![]);
        let mut buf = new_buf(10, 3);
        let result = render_prompt_input(
            &state,
            "hello world".len(),
            Rect::new(0, 0, 10, 3),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(row_text(&buf, 0), "hello");
        assert_eq!(row_text(&buf, 1), "world");
        assert_eq!(result.cursor, Some((5, 1)));
    }

    #[test]
    fn mixed_cjk_and_english_word_wrap_keeps_cursor_aligned() {
        let text = "当前这句话会因为 promptinput";
        let state = state_with(text, None, true, None, vec![]);
        let mut buf = new_buf(20, 3);
        let result = render_prompt_input(
            &state,
            text.len(),
            Rect::new(0, 0, 20, 3),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(row_text(&buf, 0).replace(' ', ""), "当前这句话会因为");
        assert_eq!(row_text(&buf, 1), "promptinput");
        assert_eq!(result.cursor, Some((11, 1)));
    }

    #[test]
    fn mixed_cjk_wrap_uses_remaining_width_after_ascii_word() {
        let text = "其实正常的理解应该是对魔法数字的理解，部分人认为是攻击伤害*段数中的段数修改为 x 这个是合理的，技能牌则应该是格挡值、或者类似的 x";
        let state = state_with(text, None, true, None, vec![]);
        let mut buf = new_buf(120, 3);
        let result = render_prompt_input(
            &state,
            text.len(),
            Rect::new(0, 0, 120, 3),
            &mut buf,
            &RenderTheme::plain(),
        );

        let first_row = row_text(&buf, 0).replace(' ', "");
        assert!(first_row.contains("x这个是合理的"), "{first_row:?}");
        assert_eq!(result.lines_used, 2);
    }

    #[test]
    fn empty_input_with_placeholder_renders_placeholder_and_cursor_at_zero() {
        let state = state_with("", Some("type here"), true, None, vec![]);
        let mut buf = new_buf(20, 1);
        let result = render_prompt_input(
            &state,
            0,
            Rect::new(0, 0, 20, 1),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(row_text(&buf, 0), "type here");
        assert_eq!(result.cursor, Some((0, 0)));
        assert_eq!(result.lines_used, 1);
    }

    #[test]
    fn empty_input_without_placeholder_still_reports_cursor() {
        let state = state_with("", None, true, None, vec![]);
        let mut buf = new_buf(20, 1);
        let result = render_prompt_input(
            &state,
            0,
            Rect::new(0, 0, 20, 1),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(row_text(&buf, 0), "");
        assert_eq!(result.lines_used, 0);
        assert_eq!(result.cursor, Some((0, 0)));
    }

    #[test]
    fn show_cursor_false_returns_none() {
        let state = state_with("hello", None, false, None, vec![]);
        let mut buf = new_buf(20, 1);
        let result = render_prompt_input(
            &state,
            3,
            Rect::new(0, 0, 20, 1),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(row_text(&buf, 0), "hello");
        assert_eq!(result.cursor, None);
    }

    #[test]
    fn multiline_input_places_cursor_on_correct_row_and_col() {
        // "ab\ncd" — cursor after 'd' (offset 5)
        let state = state_with("ab\ncd", None, true, None, vec![]);
        let mut buf = new_buf(10, 2);
        let result = render_prompt_input(
            &state,
            5,
            Rect::new(0, 0, 10, 2),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(row_text(&buf, 0), "ab");
        assert_eq!(row_text(&buf, 1), "cd");
        assert_eq!(result.lines_used, 2);
        assert_eq!(result.cursor, Some((2, 1)));
    }

    #[test]
    fn cjk_cursor_column_uses_display_width_not_byte_offset() {
        // "你好x" — 你 and 好 each occupy 2 columns, x occupies 1.
        // Byte cursor offset 6 sits after "你好", before 'x'.
        let state = state_with("你好x", None, true, None, vec![]);
        let mut buf = new_buf(20, 1);
        let result = render_prompt_input(
            &state,
            6,
            Rect::new(0, 0, 20, 1),
            &mut buf,
            &RenderTheme::plain(),
        );
        // Column 4 because 你=2 + 好=2 = 4 terminal cells.
        assert_eq!(result.cursor, Some((4, 0)));
    }

    #[test]
    fn snapped_cursor_offset_overrides_raw_cursor() {
        // Byte offset 3 is inside "abcdef", snapped to 0 → cursor at col 0.
        let state = state_with("abcdef", None, true, Some(0), vec![]);
        let mut buf = new_buf(20, 1);
        let result = render_prompt_input(
            &state,
            3,
            Rect::new(0, 0, 20, 1),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(result.cursor, Some((0, 0)));
    }

    #[test]
    fn highlights_are_clipped_to_logical_line_bounds() {
        // "ab\ncd" with a highlight covering [0..5) should paint
        // "ab" on row 0 and "cd" on row 1 without crashing on the
        // `\n` boundary.
        let highlights = vec![PromptSurfaceHighlight {
            start: 0,
            end: 5,
            color: Some(String::from("warning")),
            shimmer_color: None,
            dim_color: false,
            inverse: false,
            priority: 5,
        }];
        let state = state_with("ab\ncd", None, true, None, highlights);
        let mut buf = new_buf(10, 2);
        let result = render_prompt_input(
            &state,
            5,
            Rect::new(0, 0, 10, 2),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(row_text(&buf, 0), "ab");
        assert_eq!(row_text(&buf, 1), "cd");
        assert_eq!(result.lines_used, 2);
    }

    #[test]
    fn higher_priority_highlight_overwrites_lower_on_overlap() {
        // Two highlights on "hello":
        //   priority 5  — warning    — covers full range
        //   priority 20 — suggestion — covers middle "ll"
        // The middle should end up in the suggestion style, which
        // means the spans produced for the row split into three
        // segments: "he", "ll", "o" — hence at least 3 spans.
        let highlights = vec![
            PromptSurfaceHighlight {
                start: 0,
                end: 5,
                color: Some(String::from("warning")),
                shimmer_color: None,
                dim_color: false,
                inverse: false,
                priority: 5,
            },
            PromptSurfaceHighlight {
                start: 2,
                end: 4,
                color: Some(String::from("suggestion")),
                shimmer_color: None,
                dim_color: false,
                inverse: false,
                priority: 20,
            },
        ];
        let spans = build_line_spans("hello", 0, &highlights, &RenderTheme::default_styled());
        // Three runs of distinct styles: he (warning) | ll (suggestion) | o (warning).
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "he");
        assert_eq!(spans[1].content, "ll");
        assert_eq!(spans[2].content, "o");
        assert_ne!(spans[0].style, spans[1].style);
        assert_eq!(spans[0].style, spans[2].style);
    }

    #[test]
    fn rainbow_palette_highlights_parse_to_colored_styles() {
        let highlights = vec![PromptSurfaceHighlight {
            start: 0,
            end: 10,
            color: Some(String::from("ansi:redBright")),
            shimmer_color: Some(String::from("ansi:yellowBright")),
            dim_color: false,
            inverse: false,
            priority: 10,
        }];

        let spans = build_line_spans("/ultrawork", 0, &highlights, &RenderTheme::plain());

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "/ultrawork");
        assert_eq!(spans[0].style.fg, Some(ratatui::style::Color::LightRed));
    }

    #[test]
    fn suggestion_highlights_use_visible_theme_color() {
        let highlight = PromptSurfaceHighlight {
            start: 0,
            end: 5,
            color: Some(String::from("suggestion")),
            shimmer_color: None,
            dim_color: false,
            inverse: false,
            priority: 5,
        };

        let style = highlight_style(&highlight, &RenderTheme::default_styled());

        assert_eq!(
            style.fg,
            Some(parse_theme_color(
                theme::get_theme(rebon_design_system::theme::ThemeName::Dark).suggestion
            ))
        );
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn zero_sized_area_returns_empty_result() {
        let state = state_with("hello", None, true, None, vec![]);
        let mut buf = new_buf(20, 1);
        let result = render_prompt_input(
            &state,
            0,
            Rect::new(0, 0, 0, 0),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(result, PromptInputRenderResult::EMPTY);
    }

    #[test]
    fn line_layout_wraps_words_with_prompt_rules() {
        let layout = layout_prompt_input_line("hello world", Some("hello world".len()), 10);

        assert_eq!(layout.rows, vec!["hello ", "world"]);
        assert_eq!(layout.lines_used, 2);
        assert_eq!(layout.cursor, Some((5, 1)));
    }

    #[test]
    fn line_layout_uses_terminal_width_for_cjk_cursor() {
        let layout = layout_prompt_input_line("你a", Some("你a".len()), 4);

        assert_eq!(layout.rows, vec!["你a"]);
        assert_eq!(layout.cursor, Some((3, 0)));
    }

    #[test]
    fn line_layout_clamps_cursor_to_utf8_boundary() {
        let layout = layout_prompt_input_line("你a", Some(1), 4);

        assert_eq!(layout.cursor, Some((0, 0)));
    }

    #[test]
    fn line_layout_allocates_cursor_row_at_exact_boundary() {
        let layout = layout_prompt_input_line("abcde", Some(5), 5);

        assert_eq!(layout.rows, vec!["abcde", ""]);
        assert_eq!(layout.lines_used, 2);
        assert_eq!(layout.cursor, Some((0, 1)));
    }

    #[test]
    fn line_layout_zero_width_is_empty() {
        let layout = layout_prompt_input_line("abc", Some(1), 0);

        assert!(layout.rows.is_empty());
        assert_eq!(layout.lines_used, 0);
        assert_eq!(layout.cursor, None);
    }

    // ── wrap_metrics unit tests ────────────────────────────────────

    #[test]
    fn wrap_metrics_ascii_no_wrap() {
        // "hello" in 10-col area — fits on one row.
        let (vrows, _, _) = wrap_metrics("hello", None, 10);
        assert_eq!(vrows, 1);
    }

    #[test]
    fn wrap_metrics_ascii_wrap() {
        // "abcdefghij" (10 chars) in 5-col — 2 visual rows.
        let (vrows, _, _) = wrap_metrics("abcdefghij", None, 5);
        assert_eq!(vrows, 2);
    }

    #[test]
    fn wrap_metrics_wraps_whole_word_instead_of_splitting_it() {
        let (vrows, c_vrow, c_vcol) = wrap_metrics("hello world", Some("hello world".len()), 10);
        assert_eq!(vrows, 2);
        assert_eq!(c_vrow, 1);
        assert_eq!(c_vcol, 5);
    }

    #[test]
    fn wrap_metrics_mixed_cjk_and_english_word_stays_aligned() {
        let text = "当前这句话会因为 promptinput";
        let (vrows, c_vrow, c_vcol) = wrap_metrics(text, Some(text.len()), 20);
        assert_eq!(vrows, 2);
        assert_eq!(c_vrow, 1);
        assert_eq!(c_vcol, 11);
    }

    #[test]
    fn wrap_metrics_cjk_gap_wrapping() {
        // "ab你c" in 3-col area.
        // Row 0: a(1)+b(1)=2, 你(2) won't fit (2+2=4>3) → wrap.
        // Row 1: 你(2)+c(1)=3.
        // Total: 2 visual rows.
        let (vrows, _, _) = wrap_metrics("ab你c", None, 3);
        assert_eq!(vrows, 2);

        // Cursor after 你 (byte 5 = start of 'c'):
        // Should be row 1, col 2 (after 你 which takes 2 cols on row 1).
        let (_, c_vrow, c_vcol) = wrap_metrics("ab你c", Some(5), 3);
        assert_eq!(c_vrow, 1);
        assert_eq!(c_vcol, 2);
    }

    #[test]
    fn wrap_metrics_cjk_only_narrow_terminal() {
        // "你你你" (3 CJK, each 2-wide) in 3-col area.
        // Row 0: 你(2), next 你(2) won't fit (2+2=4>3) → wrap.
        // Row 1: 你(2), next 你(2) won't fit → wrap.
        // Row 2: 你(2).
        // Total: 3 visual rows.
        let (vrows, _, _) = wrap_metrics("你你你", None, 3);
        assert_eq!(vrows, 3);
    }

    #[test]
    fn wrap_metrics_cursor_at_exact_boundary_wraps() {
        // "abcde" in 5-col: cursor at end (byte 5) → col 5 == aw → wrap.
        let (_, c_vrow, c_vcol) = wrap_metrics("abcde", Some(5), 5);
        assert_eq!(c_vrow, 1);
        assert_eq!(c_vcol, 0);
    }

    #[test]
    fn wrap_metrics_empty_line() {
        let (vrows, c_vrow, c_vcol) = wrap_metrics("", Some(0), 10);
        assert_eq!(vrows, 1);
        assert_eq!(c_vrow, 0);
        assert_eq!(c_vcol, 0);
    }

    #[test]
    fn cjk_wrap_cursor_position_in_render() {
        // "ab你c" in 3-col area. Cursor at byte 5 (start of 'c').
        // With the old division: col=4, 4%3=1. Wrong.
        // With wrap_metrics: row 1, col 2. Correct.
        let state = state_with("ab你c", None, true, None, vec![]);
        let mut buf = new_buf(3, 3);
        let result = render_prompt_input(
            &state,
            5,
            Rect::new(0, 0, 3, 3),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(result.cursor, Some((2, 1)));
    }

    #[test]
    fn cursor_at_wrap_boundary_gets_extra_row() {
        // "abcde" in 5-col: cursor at byte 5 (end of line). The line
        // fills the full width, so the cursor wraps to row 1 col 0.
        // lines_used must be 2 (1 content + 1 cursor) so the cursor
        // isn't clamped back onto the content row.
        let state = state_with("abcde", None, true, None, vec![]);
        let mut buf = new_buf(5, 3);
        let result = render_prompt_input(
            &state,
            5,
            Rect::new(0, 0, 5, 3),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(result.lines_used, 2);
        assert_eq!(result.cursor, Some((0, 1)));
    }

    #[test]
    fn cursor_at_wrap_boundary_cjk() {
        // "你好" (4 display cols) in 4-col area: fills the line.
        // Cursor at byte 6 (end) → wraps to row 1, col 0.
        let state = state_with("你好", None, true, None, vec![]);
        let mut buf = new_buf(4, 3);
        let result = render_prompt_input(
            &state,
            6,
            Rect::new(0, 0, 4, 3),
            &mut buf,
            &RenderTheme::plain(),
        );
        assert_eq!(result.lines_used, 2);
        assert_eq!(result.cursor, Some((0, 1)));
    }
}
