//! In-app text selection for fullscreen mode.
//!
//! Selection state is held in [`SelectionState`] as an anchor plus a
//! focus point.
//!
//! Tracks a linear selection in screen-buffer coordinates (0-indexed
//! col/row). Selection is line-based: cells from (start_col, start_row)
//! through (end_col, end_row) inclusive, wrapping across line boundaries.
//!
//! The selection is stored as ANCHOR (where the drag started) + FOCUS
//! (where the cursor is now). The rendered highlight normalizes to
//! start <= end.

use ratatui::buffer::Buffer;
use ratatui::prelude::Rect;
use ratatui::style::{Modifier, Style};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub col: u16,
    pub row: u16,
}

impl Point {
    pub fn new(col: u16, row: u16) -> Self {
        Self { col, row }
    }
}

fn cmp_points(a: &Point, b: &Point) -> std::cmp::Ordering {
    a.row.cmp(&b.row).then(a.col.cmp(&b.col))
}

/// Normalized start/end of the selection (start <= end in reading order).
#[derive(Debug, Clone, Copy)]
pub struct SelectionBounds {
    pub start: Point,
    pub end: Point,
}

/// Text from a single row captured before it scrolled out of the viewport.
#[derive(Debug, Clone)]
struct CapturedRow {
    text: String,
    /// True if this row is a soft-wrap continuation (no logical newline
    /// before it). Currently always false since ratatui doesn't expose
    /// a soft-wrap bitmap — we treat every row as a logical line. Can
    /// be refined later to honour soft-wrap from render.
    soft_wrap: bool,
}

#[derive(Debug, Clone)]
pub struct SelectionState {
    /// Where the mouse-down occurred. None when no selection.
    anchor: Option<Point>,
    /// Current drag/focus position. None until the first drag motion
    /// (a click-release with no drag leaves focus None to no highlight).
    focus: Option<Point>,
    /// True between mouse-down and mouse-up.
    is_dragging: bool,
    /// Text from rows that scrolled out ABOVE the viewport during
    /// drag-to-scroll or follow-tail. Prepended to on-screen text
    /// by `get_selected_text`.
    scrolled_off_above: Vec<CapturedRow>,
    /// Symmetric: rows scrolled out BELOW when dragging up.
    scrolled_off_below: Vec<CapturedRow>,
    /// Pre-clamp anchor row. Set when shift clamps anchor so a
    /// reverse scroll can restore the true position. Cleared on
    /// start/clear.
    virtual_anchor_row: Option<i32>,
    /// Same for focus.
    virtual_focus_row: Option<i32>,
}

impl Default for SelectionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SelectionState {
    pub fn new() -> Self {
        Self {
            anchor: None,
            focus: None,
            is_dragging: false,
            scrolled_off_above: Vec::new(),
            scrolled_off_below: Vec::new(),
            virtual_anchor_row: None,
            virtual_focus_row: None,
        }
    }

    pub fn has_selection(&self) -> bool {
        self.anchor.is_some() && self.focus.is_some()
    }

    pub fn is_dragging(&self) -> bool {
        self.is_dragging
    }

    // ── Lifecycle ────────────────────────────────────────────────

    pub fn start(&mut self, col: u16, row: u16) {
        self.anchor = Some(Point::new(col, row));
        // Focus is not set until first drag motion.
        self.focus = None;
        self.is_dragging = true;
        self.scrolled_off_above.clear();
        self.scrolled_off_below.clear();
        self.virtual_anchor_row = None;
        self.virtual_focus_row = None;
    }

    pub fn update(&mut self, col: u16, row: u16) {
        if !self.is_dragging {
            return;
        }
        // First motion at the same cell as anchor is a no-op (sub-pixel
        // tremor). Once focus is set, we track normally.
        if self.focus.is_none() {
            if let Some(a) = self.anchor {
                if a.col == col && a.row == row {
                    return;
                }
            }
        }
        self.focus = Some(Point::new(col, row));
    }

    pub fn finish(&mut self) {
        self.is_dragging = false;
        // Keep anchor/focus so highlight stays visible and text can
        // be copied. Clear via `clear()`.
    }

    pub fn clear(&mut self) {
        self.anchor = None;
        self.focus = None;
        self.is_dragging = false;
        self.scrolled_off_above.clear();
        self.scrolled_off_below.clear();
        self.virtual_anchor_row = None;
        self.virtual_focus_row = None;
    }

    // ── Bounds ───────────────────────────────────────────────────

    /// Normalized bounds (start <= end). Returns None if no selection.
    pub fn bounds(&self) -> Option<SelectionBounds> {
        let anchor = self.anchor?;
        let focus = self.focus?;
        if cmp_points(&anchor, &focus) == std::cmp::Ordering::Greater {
            Some(SelectionBounds {
                start: focus,
                end: anchor,
            })
        } else {
            Some(SelectionBounds {
                start: anchor,
                end: focus,
            })
        }
    }

    /// True when the selection has at least one row that should be
    /// painted inside `area`. Returns false when both endpoints have
    /// scrolled out of the viewport on the same side — `bounds()` will
    /// be clamped to a boundary row, but no actual selected content
    /// lives there, so painting would leave a 1-row highlight residue.
    pub fn is_overlay_visible(&self, area: Rect) -> bool {
        if !self.has_selection() {
            return false;
        }
        if let (Some(va), Some(vf)) = (self.virtual_anchor_row, self.virtual_focus_row) {
            let min = area.y as i32;
            let max = (area.y + area.height.saturating_sub(1)) as i32;
            if (va < min && vf < min) || (va > max && vf > max) {
                return false;
            }
        }
        true
    }

    // ── Scroll compensation ─────────────────────────────────────

    /// Shift anchor by `d_row` during drag-to-scroll. Focus tracks the
    /// mouse so only anchor moves.
    pub fn shift_anchor(&mut self, d_row: i32, min_row: u16, max_row: u16) {
        let anchor = match self.anchor {
            Some(a) => a,
            None => return,
        };
        let raw = self.virtual_anchor_row.unwrap_or(anchor.row as i32) + d_row;
        let clamped = raw.max(min_row as i32).min(max_row as i32) as u16;
        self.anchor = Some(Point::new(anchor.col, clamped));
        self.virtual_anchor_row = if raw < min_row as i32 || raw > max_row as i32 {
            Some(raw)
        } else {
            None
        };
    }

    /// Shift both anchor and focus by `d_row` for follow-tail/keyboard
    /// scroll. Returns true if the selection was cleared (both ends
    /// scrolled entirely off).
    pub fn shift_for_follow(&mut self, d_row: i32, min_row: u16, max_row: u16) -> bool {
        let anchor = match self.anchor {
            Some(a) => a,
            None => return false,
        };
        let raw_anchor = self.virtual_anchor_row.unwrap_or(anchor.row as i32) + d_row;
        let raw_focus = self
            .focus
            .map(|f| self.virtual_focus_row.unwrap_or(f.row as i32) + d_row);

        // Both ends above the viewport — don't clear. The selection
        // text has been captured into `scrolled_off_above` already.
        // Keeping the selection alive means Ctrl+C will still trigger
        // a copy instead of killing the running process.
        // Fall through to the clamping logic below.

        let clamp = |raw: i32| -> u16 { raw.max(min_row as i32).min(max_row as i32) as u16 };

        self.anchor = Some(Point::new(anchor.col, clamp(raw_anchor)));
        if let Some(f) = self.focus {
            let rf = raw_focus.unwrap();
            self.focus = Some(Point::new(f.col, clamp(rf)));
            self.virtual_focus_row = if rf < min_row as i32 || rf > max_row as i32 {
                Some(rf)
            } else {
                None
            };
        }
        self.virtual_anchor_row = if raw_anchor < min_row as i32 || raw_anchor > max_row as i32 {
            Some(raw_anchor)
        } else {
            None
        };
        false
    }

    // ── Scrolled-off text capture ───────────────────────────────

    /// Capture rows about to scroll off using a text snapshot from the
    /// PREVIOUS frame. `prev_lines` is indexed by `(screen_row - prev_area.y)`.
    /// Must be called BEFORE `shift_anchor` / `shift_for_follow`.
    pub fn capture_from_snapshot(
        &mut self,
        prev_lines: &[String],
        prev_area: Rect,
        first_row: u16,
        last_row: u16,
        side: ScrollSide,
    ) {
        let b = match self.bounds() {
            Some(b) => b,
            None => return,
        };
        if first_row > last_row {
            return;
        }
        let lo = first_row.max(b.start.row);
        let hi = last_row.min(b.end.row);
        if lo > hi {
            return;
        }

        for row in lo..=hi {
            let idx = (row.saturating_sub(prev_area.y)) as usize;
            let text = prev_lines.get(idx).cloned().unwrap_or_default();
            match side {
                ScrollSide::Above => {
                    self.scrolled_off_above.push(CapturedRow {
                        text,
                        soft_wrap: false,
                    });
                }
                ScrollSide::Below => {
                    self.scrolled_off_below.insert(
                        0,
                        CapturedRow {
                            text,
                            soft_wrap: false,
                        },
                    );
                }
            }
        }

        // Reset anchor col to full-width after capturing.
        if side == ScrollSide::Above {
            if let Some(ref mut a) = self.anchor {
                if a.row == b.start.row && lo == b.start.row {
                    a.col = 0;
                }
            }
        } else if let Some(ref mut a) = self.anchor {
            if a.row == b.end.row && hi == b.end.row {
                a.col = prev_area.width.saturating_sub(1);
            }
        }
    }

    // ── Text extraction ─────────────────────────────────────────

    /// Build the full selected text for clipboard copy.
    pub fn get_selected_text(&self, buf: &Buffer, area: Rect) -> String {
        let b = match self.bounds() {
            Some(b) => b,
            None => return String::new(),
        };

        let mut lines: Vec<String> = Vec::new();

        // Rows scrolled off above.
        for row in &self.scrolled_off_above {
            join_rows(&mut lines, &row.text, row.soft_wrap);
        }

        // On-screen rows.
        for row in b.start.row..=b.end.row {
            if row < area.y || row >= area.y + area.height {
                continue;
            }
            let text = extract_row_text(buf, area, row);
            join_rows(&mut lines, &text, false);
        }

        // Rows scrolled off below.
        for row in &self.scrolled_off_below {
            join_rows(&mut lines, &row.text, row.soft_wrap);
        }

        lines.join("\n")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSide {
    Above,
    Below,
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Apply selection highlight overlay directly to the frame buffer.
/// Modifies cell styles in-place so ratatui's diff engine picks up
/// selection as ordinary cell changes — no separate selection layer.
///
/// Uses inverse video (swap fg/bg) for a clean, universal look that
/// works with any color scheme.
pub fn apply_selection_overlay(selection: &SelectionState, buf: &mut Buffer, area: Rect) {
    if !selection.is_overlay_visible(area) {
        return;
    }
    let b = match selection.bounds() {
        Some(b) => b,
        None => return,
    };

    let sel_style = Style::default().add_modifier(Modifier::REVERSED);

    for row in b.start.row..=b.end.row {
        if row < area.y || row >= area.y + area.height {
            continue;
        }
        let col_start = if row == b.start.row {
            b.start.col.max(area.x)
        } else {
            area.x
        };
        let col_end = if row == b.end.row {
            b.end.col.min(area.x + area.width - 1)
        } else {
            area.x + area.width - 1
        };
        for col in col_start..=col_end {
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.set_style(sel_style);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract visible text from a single row in the buffer.
///
/// Reads all cells in the row within `area`, skips wide-char
/// continuation cells (ratatui fills the second cell of a CJK /
/// emoji character with `" "`), and trims trailing whitespace.
fn extract_row_text(buf: &Buffer, area: Rect, row: u16) -> String {
    use rebon_width::WidthStr;

    let mut text = String::new();
    let col_start = area.x;
    let col_end = area.x + area.width.saturating_sub(1);
    let mut skip_continuation: u16 = 0;

    for col in col_start..=col_end {
        // After a wide character (display width > 1), ratatui fills
        // the continuation cells with " ". Skip them.
        if skip_continuation > 0 {
            skip_continuation -= 1;
            continue;
        }
        if let Some(cell) = buf.cell((col, row)) {
            let sym = cell.symbol();
            if sym.is_empty() || sym == "\0" {
                continue;
            }
            text.push_str(sym);
            // If this grapheme is wider than 1 column, skip the
            // continuation cells that follow it.
            let w = WidthStr::width(sym);
            if w > 1 {
                skip_continuation = (w as u16) - 1;
            }
        }
    }
    text.trim_end().to_string()
}

/// Join rows respecting soft-wrap: soft-wrapped rows concatenate onto
/// the previous line, non-wrapped rows start a new line.
fn join_rows(lines: &mut Vec<String>, text: &str, soft_wrap: bool) {
    if soft_wrap && !lines.is_empty() {
        lines.last_mut().unwrap().push_str(text);
    } else {
        lines.push(text.to_string());
    }
}

/// Snapshot visible text lines from the buffer for the given area.
/// Returns one `String` per screen row, indexed by `(row - area.y)`.
/// Used to cache the previous frame's text so `capture_from_snapshot`
/// can read rows that have since scrolled off.
pub fn snapshot_area_text(buf: &Buffer, area: Rect) -> Vec<String> {
    (0..area.height)
        .map(|dy| extract_row_text(buf, area, area.y + dy))
        .collect()
}

// ---------------------------------------------------------------------------
// OSC 52 clipboard
// ---------------------------------------------------------------------------

use std::io::Write;

/// Write selected text to the system clipboard via OSC 52. Works over
/// SSH, tmux, and most modern terminal emulators.
pub fn copy_to_clipboard_osc52(text: &str) {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    // OSC 52 ; c ; <base64> ST
    let seq = format!("\x1b]52;c;{}\x07", encoded);
    let _ = std::io::stdout().write_all(seq.as_bytes());
    let _ = std::io::stdout().flush();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Widget};

    /// Create a Buffer and paint `lines` into it starting at (0, 0).
    /// Each string becomes one row; the buffer width is `width`.
    fn buf_with_lines(width: u16, lines: &[&str]) -> (Buffer, Rect) {
        let h = lines.len() as u16;
        let area = Rect::new(0, 0, width, h);
        let mut buf = Buffer::empty(area);
        for (y, text) in lines.iter().enumerate() {
            let para = Paragraph::new(Line::from(*text));
            para.render(Rect::new(0, y as u16, width, 1), &mut buf);
        }
        (buf, area)
    }

    // ── Lifecycle ────────────────────────────────────────────────

    #[test]
    fn new_has_no_selection() {
        let s = SelectionState::new();
        assert!(!s.has_selection());
        assert!(!s.is_dragging());
        assert!(s.bounds().is_none());
    }

    #[test]
    fn start_sets_anchor_and_dragging() {
        let mut s = SelectionState::new();
        s.start(5, 3);
        assert!(s.is_dragging());
        // No focus yet → no selection.
        assert!(!s.has_selection());
        assert!(s.bounds().is_none());
    }

    #[test]
    fn update_at_anchor_cell_is_noop() {
        let mut s = SelectionState::new();
        s.start(5, 3);
        s.update(5, 3); // same cell — sub-pixel tremor
        assert!(!s.has_selection());
    }

    #[test]
    fn update_to_different_cell_creates_selection() {
        let mut s = SelectionState::new();
        s.start(5, 3);
        s.update(10, 3);
        assert!(s.has_selection());
        let b = s.bounds().unwrap();
        assert_eq!(b.start, Point::new(5, 3));
        assert_eq!(b.end, Point::new(10, 3));
    }

    #[test]
    fn update_noop_when_not_dragging() {
        let mut s = SelectionState::new();
        // Not started — update should be ignored.
        s.update(10, 3);
        assert!(!s.has_selection());
    }

    #[test]
    fn finish_keeps_selection_visible() {
        let mut s = SelectionState::new();
        s.start(0, 0);
        s.update(5, 0);
        s.finish();
        assert!(!s.is_dragging());
        assert!(s.has_selection());
        assert!(s.bounds().is_some());
    }

    #[test]
    fn clear_removes_everything() {
        let mut s = SelectionState::new();
        s.start(0, 0);
        s.update(5, 2);
        s.finish();
        s.clear();
        assert!(!s.has_selection());
        assert!(!s.is_dragging());
        assert!(s.bounds().is_none());
    }

    // ── Bounds normalization ────────────────────────────────────

    #[test]
    fn bounds_normalizes_backward_selection() {
        let mut s = SelectionState::new();
        s.start(10, 5);
        s.update(2, 3); // focus before anchor
        let b = s.bounds().unwrap();
        assert_eq!(b.start, Point::new(2, 3));
        assert_eq!(b.end, Point::new(10, 5));
    }

    #[test]
    fn bounds_same_row_backward() {
        let mut s = SelectionState::new();
        s.start(10, 3);
        s.update(2, 3);
        let b = s.bounds().unwrap();
        assert_eq!(b.start, Point::new(2, 3));
        assert_eq!(b.end, Point::new(10, 3));
    }

    // ── Text extraction: ASCII ──────────────────────────────────

    #[test]
    fn get_selected_text_single_line() {
        let (buf, area) = buf_with_lines(20, &["Hello, world!"]);
        let mut s = SelectionState::new();
        s.start(0, 0);
        s.update(19, 0);
        let text = s.get_selected_text(&buf, area);
        assert_eq!(text, "Hello, world!");
    }

    #[test]
    fn get_selected_text_multi_line() {
        let (buf, area) = buf_with_lines(20, &["Line one", "Line two", "Line three"]);
        let mut s = SelectionState::new();
        s.start(0, 0);
        s.update(19, 2);
        let text = s.get_selected_text(&buf, area);
        assert_eq!(text, "Line one\nLine two\nLine three");
    }

    // ── Text extraction: CJK wide characters ────────────────────

    #[test]
    fn get_selected_text_cjk_no_extra_spaces() {
        let (buf, area) = buf_with_lines(20, &["你好世界"]);
        let mut s = SelectionState::new();
        s.start(0, 0);
        s.update(19, 0);
        let text = s.get_selected_text(&buf, area);
        assert_eq!(text, "你好世界");
    }

    #[test]
    fn get_selected_text_mixed_ascii_cjk() {
        let (buf, area) = buf_with_lines(30, &["Hello 你好 World"]);
        let mut s = SelectionState::new();
        s.start(0, 0);
        s.update(29, 0);
        let text = s.get_selected_text(&buf, area);
        assert_eq!(text, "Hello 你好 World");
    }

    #[test]
    fn get_selected_text_emoji() {
        let (buf, area) = buf_with_lines(20, &["A🎉B"]);
        let mut s = SelectionState::new();
        s.start(0, 0);
        s.update(19, 0);
        let text = s.get_selected_text(&buf, area);
        assert_eq!(text, "A🎉B");
    }

    // ── Text extraction: trailing whitespace trimmed ────────────

    #[test]
    fn get_selected_text_trims_trailing_spaces() {
        let (buf, area) = buf_with_lines(40, &["Short"]);
        let mut s = SelectionState::new();
        s.start(0, 0);
        s.update(39, 0);
        let text = s.get_selected_text(&buf, area);
        assert_eq!(text, "Short");
    }

    // ── Snapshot and extract ────────────────────────────────────

    #[test]
    fn snapshot_area_text_captures_all_rows() {
        let (buf, area) = buf_with_lines(20, &["Row A", "Row B", "Row C"]);
        let snap = snapshot_area_text(&buf, area);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0], "Row A");
        assert_eq!(snap[1], "Row B");
        assert_eq!(snap[2], "Row C");
    }

    #[test]
    fn snapshot_cjk_no_extra_spaces() {
        let (buf, area) = buf_with_lines(20, &["你好世界"]);
        let snap = snapshot_area_text(&buf, area);
        assert_eq!(snap[0], "你好世界");
    }

    // ── Scroll compensation: shift_anchor ───────────────────────

    #[test]
    fn shift_anchor_moves_only_anchor() {
        let mut s = SelectionState::new();
        s.start(5, 10);
        s.update(5, 15);
        // Scroll down 2 → content moves up → anchor row decreases.
        s.shift_anchor(-2, 0, 20);
        let b = s.bounds().unwrap();
        assert_eq!(b.start.row, 8); // anchor shifted
        assert_eq!(b.end.row, 15); // focus unchanged
    }

    #[test]
    fn shift_anchor_clamps_to_min() {
        let mut s = SelectionState::new();
        s.start(0, 2);
        s.update(0, 10);
        s.shift_anchor(-5, 0, 20); // would go to -3 → clamped to 0
        let b = s.bounds().unwrap();
        assert_eq!(b.start.row, 0);
    }

    #[test]
    fn shift_anchor_virtual_row_recovers_on_reverse() {
        let mut s = SelectionState::new();
        s.start(0, 5);
        s.update(0, 10);
        // Shift down past min → clamp, then reverse.
        s.shift_anchor(-8, 0, 20); // raw = -3, clamped to 0
        assert_eq!(s.bounds().unwrap().start.row, 0);
        s.shift_anchor(5, 0, 20); // raw = -3 + 5 = 2
        assert_eq!(s.bounds().unwrap().start.row, 2);
    }

    // ── Scroll compensation: shift_for_follow ──────────────────

    #[test]
    fn shift_for_follow_moves_both_endpoints() {
        let mut s = SelectionState::new();
        s.start(0, 5);
        s.update(0, 10);
        s.finish();
        let cleared = s.shift_for_follow(-2, 0, 20);
        assert!(!cleared);
        let b = s.bounds().unwrap();
        assert_eq!(b.start.row, 3);
        assert_eq!(b.end.row, 8);
    }

    #[test]
    fn shift_for_follow_preserves_selection_when_both_off_top() {
        let mut s = SelectionState::new();
        s.start(0, 1);
        s.update(0, 3);
        s.finish();
        let cleared = s.shift_for_follow(-5, 0, 20); // both go negative
                                                     // Selection is preserved (clamped) so Ctrl+C can still copy.
        assert!(!cleared);
        assert!(s.has_selection());
        let b = s.bounds().unwrap();
        assert_eq!(b.start.row, 0); // clamped to min_row
        assert_eq!(b.end.row, 0); // clamped to min_row
    }

    #[test]
    fn shift_for_follow_does_not_clear_if_one_end_in_bounds() {
        let mut s = SelectionState::new();
        s.start(0, 2);
        s.update(0, 10);
        s.finish();
        let cleared = s.shift_for_follow(-5, 0, 20);
        assert!(!cleared);
        assert!(s.has_selection());
        let b = s.bounds().unwrap();
        assert_eq!(b.start.row, 0); // clamped
        assert_eq!(b.end.row, 5);
    }

    // ── capture_from_snapshot ───────────────────────────────────

    #[test]
    fn capture_from_snapshot_above() {
        let prev_area = Rect::new(0, 0, 20, 5);
        let prev_lines = vec![
            "Row 0".into(),
            "Row 1".into(),
            "Row 2".into(),
            "Row 3".into(),
            "Row 4".into(),
        ];
        let mut s = SelectionState::new();
        s.start(0, 0);
        s.update(19, 4);

        // Scrolled down 2 → rows 0-1 leave the top.
        s.capture_from_snapshot(&prev_lines, prev_area, 0, 1, ScrollSide::Above);
        // Shift selection down.
        s.shift_for_follow(-2, 0, 4);

        // Build a "new" buffer that shows rows 2-6 (simulating scroll).
        let (buf, area) = buf_with_lines(20, &["Row 2", "Row 3", "Row 4", "Row 5", "Row 6"]);
        let text = s.get_selected_text(&buf, area);
        // Should include captured rows 0-1 plus visible rows 2-4.
        assert!(text.contains("Row 0"));
        assert!(text.contains("Row 1"));
        assert!(text.contains("Row 2"));
    }

    #[test]
    fn capture_from_snapshot_below() {
        let prev_area = Rect::new(0, 0, 20, 5);
        let prev_lines = vec![
            "Row 0".into(),
            "Row 1".into(),
            "Row 2".into(),
            "Row 3".into(),
            "Row 4".into(),
        ];
        let mut s = SelectionState::new();
        s.start(0, 0);
        s.update(19, 4);

        // Scrolled up 2 → rows 3-4 leave the bottom.
        s.capture_from_snapshot(&prev_lines, prev_area, 3, 4, ScrollSide::Below);
        s.shift_for_follow(2, 0, 4);

        let (buf, area) = buf_with_lines(20, &["Row -2", "Row -1", "Row 0", "Row 1", "Row 2"]);
        let text = s.get_selected_text(&buf, area);
        assert!(text.contains("Row 3"));
        assert!(text.contains("Row 4"));
    }

    #[test]
    fn capture_ignores_rows_outside_selection() {
        let prev_area = Rect::new(0, 0, 20, 5);
        let prev_lines = vec![
            "Row 0".into(),
            "Row 1".into(),
            "Row 2".into(),
            "Row 3".into(),
            "Row 4".into(),
        ];
        // Selection only covers rows 2-3.
        let mut s = SelectionState::new();
        s.start(0, 2);
        s.update(19, 3);

        // Scroll down 2 → rows 0-1 leave. But selection starts at 2,
        // so only row 2 intersects the scrolled-off range.
        s.capture_from_snapshot(&prev_lines, prev_area, 0, 1, ScrollSide::Above);
        // Only row within both [0,1] and [2,3] → none. No capture.
        assert_eq!(s.scrolled_off_above.len(), 0);
    }

    // ── Selection overlay ──────────────────────────────────────

    #[test]
    fn overlay_applies_reversed_modifier() {
        let (mut buf, area) = buf_with_lines(10, &["ABCDEFGHIJ"]);
        let mut s = SelectionState::new();
        s.start(2, 0);
        s.update(5, 0);
        apply_selection_overlay(&s, &mut buf, area);

        // Cells 2-5 should have REVERSED modifier.
        for col in 2..=5 {
            let cell = buf.cell((col, 0)).unwrap();
            assert!(
                cell.modifier.contains(Modifier::REVERSED),
                "cell at col {} should be REVERSED",
                col,
            );
        }
        // Cells outside selection should NOT have REVERSED.
        for col in [0, 1, 6, 7] {
            let cell = buf.cell((col, 0)).unwrap();
            assert!(
                !cell.modifier.contains(Modifier::REVERSED),
                "cell at col {} should NOT be REVERSED",
                col,
            );
        }
    }

    #[test]
    fn overlay_skipped_when_fully_scrolled_off_above() {
        // Selection covers rows 5-10. Viewport is rows 0-19.
        // Scroll down by 15 → both endpoints clamp to 0 with virtual
        // rows tracking the true (negative) positions. `bounds()` returns
        // (0, 0) but no selected content actually lives at row 0 anymore.
        let area = Rect::new(0, 0, 10, 20);
        let mut buf = Buffer::empty(area);
        let mut s = SelectionState::new();
        s.start(0, 5);
        s.update(9, 10);
        s.finish();
        s.shift_for_follow(-15, 0, 19);

        // Bounds is clamped to (0, 0)..(0, 9) but both virtuals are
        // < min_row, so the overlay must NOT paint row 0.
        apply_selection_overlay(&s, &mut buf, area);
        for col in 0..10 {
            let cell = buf.cell((col, 0)).unwrap();
            assert!(
                !cell.modifier.contains(Modifier::REVERSED),
                "row 0 col {} should not be highlighted after full scroll-off above",
                col,
            );
        }
    }

    #[test]
    fn overlay_skipped_when_fully_scrolled_off_below() {
        let area = Rect::new(0, 0, 10, 20);
        let mut buf = Buffer::empty(area);
        let mut s = SelectionState::new();
        s.start(0, 5);
        s.update(9, 10);
        s.finish();
        // Scroll up by 15 → both endpoints go past max_row, clamped to 19.
        s.shift_for_follow(15, 0, 19);

        apply_selection_overlay(&s, &mut buf, area);
        for col in 0..10 {
            let cell = buf.cell((col, 19)).unwrap();
            assert!(
                !cell.modifier.contains(Modifier::REVERSED),
                "row 19 col {} should not be highlighted after full scroll-off below",
                col,
            );
        }
    }

    #[test]
    fn overlay_still_renders_when_only_one_end_offscreen() {
        // Selection rows 5-10, viewport 0-19. Scroll down 7 →
        // anchor (5) virtual at -2, focus (10) at row 3 (visible).
        // Overlay should still paint rows 0-3 (the on-screen part).
        let area = Rect::new(0, 0, 10, 20);
        let mut buf = Buffer::empty(area);
        let mut s = SelectionState::new();
        s.start(0, 5);
        s.update(9, 10);
        s.finish();
        s.shift_for_follow(-7, 0, 19);

        apply_selection_overlay(&s, &mut buf, area);
        // Row 3 (focus, visible) should be highlighted.
        let cell = buf.cell((0, 3)).unwrap();
        assert!(
            cell.modifier.contains(Modifier::REVERSED),
            "visible end of partially-scrolled selection should still highlight",
        );
    }

    #[test]
    fn overlay_clamps_to_area() {
        let area = Rect::new(0, 2, 10, 3); // rows 2-4
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 8));
        let mut s = SelectionState::new();
        s.start(0, 0);
        s.update(9, 7);
        apply_selection_overlay(&s, &mut buf, area);

        // Row 1 is outside area → should not be touched.
        assert!(!buf
            .cell((0, 1))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED));
        // Row 3 is inside area → should be REVERSED.
        assert!(buf
            .cell((0, 3))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED));
        // Row 5 is outside area → should not be touched.
        assert!(!buf
            .cell((0, 5))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED));
    }

    // ── extract_row_text edge cases ────────────────────────────

    #[test]
    fn extract_empty_row() {
        let (buf, area) = buf_with_lines(20, &[""]);
        let text = extract_row_text(&buf, area, 0);
        assert_eq!(text, "");
    }

    #[test]
    fn extract_only_spaces() {
        let (buf, area) = buf_with_lines(20, &["     "]);
        let text = extract_row_text(&buf, area, 0);
        assert_eq!(text, "");
    }

    #[test]
    fn extract_preserves_leading_spaces() {
        let (buf, area) = buf_with_lines(20, &["  indented"]);
        let text = extract_row_text(&buf, area, 0);
        assert_eq!(text, "  indented");
    }

    #[test]
    fn extract_consecutive_cjk() {
        let (buf, area) = buf_with_lines(20, &["一二三四五"]);
        let text = extract_row_text(&buf, area, 0);
        assert_eq!(text, "一二三四五");
    }

    #[test]
    fn extract_mixed_width() {
        let (buf, area) = buf_with_lines(30, &["A你B好C"]);
        let text = extract_row_text(&buf, area, 0);
        assert_eq!(text, "A你B好C");
    }
}
