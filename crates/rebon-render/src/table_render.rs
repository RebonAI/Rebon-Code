//! Markdown-table border drawing, row vertical centering, and the
//! horizontal-vs-vertical layout switch.
//!
//! ## What this module covers
//!
//! * **Row rendering** — [`render_row_lines`], which centres pre-wrapped
//!   cell lines vertically inside a row.
//! * **Border drawing** — [`render_border_line`] for the top, middle and
//!   bottom rules.
//! * **Vertical (key-value) format** — [`render_vertical_format`], which
//!   turns each source row into a block of label/value pairs.
//! * **Horizontal-vs-vertical decision** — [`use_vertical_format`] (the
//!   `max_row_lines > MAX_ROW_LINES` gate) plus the post-render
//!   `max_line_width > terminal_width - SAFETY_MARGIN` safety check in
//!   [`render_horizontal_table`].
//!
//! ## Why pre-wrapped lines instead of inline wrapping
//!
//! ANSI-aware wrapping is non-trivial (style carry-over, hyperlink
//! escapes, hard-wrap semantics) and is out of scope for this crate.
//! The renderer here takes already-wrapped `Vec<String>` per cell so the
//! layout and border logic can be tested independently of whichever
//! wrapper the production caller uses; that caller composes the wrapped
//! lines before calling in.

use crate::pad_aligned::{pad_aligned, Alignment};
use crate::table_layout::{compute_available_width, ColumnLayout, MAX_ROW_LINES, SAFETY_MARGIN};
use rebon_width::WidthStr;

/// Which border line is being drawn: top, middle or bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderKind {
    Top,
    Middle,
    Bottom,
}

/// One pre-wrapped, pre-formatted cell. The caller has already done
/// the work of:
///
/// * Choosing the column width.
/// * Wrapping the cell content (ANSI-aware) to that width.
/// * Stripping or preserving ANSI as needed for `display_width`.
///
/// `lines` may be empty: the renderer treats an empty `lines` vector as
/// one empty line.
#[derive(Debug, Clone)]
pub struct CellLines {
    /// Pre-wrapped visible content for this cell.
    pub lines: Vec<String>,
}

impl CellLines {
    /// Convenience for tests and simple callers — wrap a single
    /// already-wrapped string into a one-line cell.
    pub fn single(line: impl Into<String>) -> Self {
        Self {
            lines: vec![line.into()],
        }
    }
}

/// Caller-supplied input for table rendering: header, rows, alignments
/// and per-column widths, with every cell already pre-wrapped to its
/// chosen column width.
#[derive(Debug, Clone)]
pub struct TableInput {
    /// Header row, one cell per column.
    pub header: Vec<CellLines>,
    /// Data rows. Every row must have `header.len()` cells.
    pub rows: Vec<Vec<CellLines>>,
    /// One alignment per column. `Alignment::Left` is the default
    /// when the markdown source omits the alignment marker.
    pub align: Vec<Alignment>,
    /// Per-column display widths, as computed by
    /// [`compute_column_widths`](crate::table_layout::compute_column_widths).
    pub column_widths: Vec<usize>,
    /// Whether the layout had to hard-wrap. The render module does not
    /// consume this flag for table drawing — wrapping happens before it
    /// is called — but it is part of the input contract so the production
    /// caller can pass the full
    /// [`ColumnLayout`] without
    /// splitting it.
    pub needs_hard_wrap: bool,
}

impl TableInput {
    /// Construct from a [`ColumnLayout`] and the cell/header inputs.
    /// Convenience for production callers and tests.
    pub fn new(
        header: Vec<CellLines>,
        rows: Vec<Vec<CellLines>>,
        align: Vec<Alignment>,
        layout: ColumnLayout,
    ) -> Self {
        Self {
            header,
            rows,
            align,
            column_widths: layout.widths,
            needs_hard_wrap: layout.needs_hard_wrap,
        }
    }
}

/// Output of [`render_horizontal_table`] / [`render_vertical_format`].
/// `lines` is one entry per visual row of the rendered output. The
/// caller is responsible for joining with `\n` (or for routing each
/// line into a separate render target).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedTable {
    pub lines: Vec<String>,
}

/// Render a single horizontal border line for the given column widths.
///
/// Output for `Top` with widths `[3, 5]` is:
///
/// ```text
/// ┌─────┬───────┐
/// ```
///
/// (Each `column_width + 2` cells of the bar character, joined by
/// the appropriate cross / corner character.)
pub fn render_border_line(kind: BorderKind, column_widths: &[usize]) -> String {
    let (left, mid, cross, right) = match kind {
        BorderKind::Top => ("┌", "─", "┬", "┐"),
        BorderKind::Middle => ("├", "─", "┼", "┤"),
        BorderKind::Bottom => ("└", "─", "┴", "┘"),
    };
    let mut out = String::new();
    out.push_str(left);
    for (i, &width) in column_widths.iter().enumerate() {
        for _ in 0..(width + 2) {
            out.push_str(mid);
        }
        if i < column_widths.len() - 1 {
            out.push_str(cross);
        } else {
            out.push_str(right);
        }
    }
    out
}

/// Render the lines for one row, performing per-cell vertical centering.
///
/// `cells` are the pre-wrapped cell contents (one entry per column).
/// `is_header` selects the header alignment override (header cells
/// are forced to centered regardless of column alignment).
///
/// Each output line is one **complete** physical row. Multi-line
/// cells are vertically centered against the tallest cell in the
/// row by leaving blank lines above shorter cells. The top offset is
/// `(max_lines - cell_lines) / 2`, rounded down, so an odd total
/// padding leaves the extra blank line BELOW the cell.
pub fn render_row_lines(
    cells: &[CellLines],
    align: &[Alignment],
    column_widths: &[usize],
    is_header: bool,
) -> Vec<String> {
    assert_eq!(
        cells.len(),
        column_widths.len(),
        "row cells and column_widths must have the same length",
    );
    assert_eq!(
        align.len(),
        column_widths.len(),
        "align and column_widths must have the same length",
    );

    // Default an empty `lines` to a single empty line.
    let cell_lines: Vec<&[String]> = cells
        .iter()
        .map(|c| {
            if c.lines.is_empty() {
                &[][..]
            } else {
                c.lines.as_slice()
            }
        })
        .collect();

    let max_lines = cell_lines
        .iter()
        .map(|lines| lines.len())
        .max()
        .unwrap_or(0)
        .max(1);

    // Vertical offset per column — `(max_lines - n) / 2`, rounded down.
    let offsets: Vec<usize> = cell_lines
        .iter()
        .map(|lines| (max_lines - lines.len()) / 2)
        .collect();

    let mut out = Vec::with_capacity(max_lines);
    for line_idx in 0..max_lines {
        let mut line = String::from("│");
        for (col_idx, &width) in column_widths.iter().enumerate() {
            let lines = cell_lines[col_idx];
            let offset = offsets[col_idx];
            let line_text: &str = if line_idx >= offset && line_idx - offset < lines.len() {
                &lines[line_idx - offset]
            } else {
                ""
            };
            // Header alignment is forced to Center; data uses
            // per-column alignment.
            let cell_align = if is_header {
                Alignment::Center
            } else {
                align[col_idx]
            };
            // Display width = unicode-width of the visible string.
            // ANSI-styled inputs need their width counted by the
            // caller before reaching this function (it does
            // not parse escape sequences) — but plain unicode width
            // is the correct measurement for the no-ANSI case.
            let display_width = WidthStr::width(line_text);
            line.push(' ');
            line.push_str(&pad_aligned(line_text, display_width, width, cell_align));
            line.push_str(" │");
        }
        out.push(line);
    }
    out
}

/// Render the table in horizontal (boxed) format: top rule, header row, a
/// rule per body row, bottom rule.
///
/// Returns `RenderedTable` whose `lines` are the boxed rows in order.
/// The caller joins them into one block or routes each line into a
/// separate render target.
///
/// **Safety check**: if any output line exceeds
/// `terminal_width - SAFETY_MARGIN`, the function returns
/// [`render_vertical_format`]'s output instead.
pub fn render_horizontal_table(table: &TableInput, terminal_width: usize) -> RenderedTable {
    // Pre-flight: if any row would wrap to more than MAX_ROW_LINES,
    // vertical format is used instead. Callers usually check this
    // first, but the check is repeated here.
    let header_max = max_row_lines(&table.header);
    let body_max = table
        .rows
        .iter()
        .map(|row| max_row_lines(row))
        .max()
        .unwrap_or(0);
    let max_row = header_max.max(body_max).max(1);
    if max_row > MAX_ROW_LINES {
        return render_vertical_format(table, terminal_width);
    }

    let mut lines = Vec::new();
    lines.push(render_border_line(BorderKind::Top, &table.column_widths));
    lines.extend(render_row_lines(
        &table.header,
        &table.align,
        &table.column_widths,
        true,
    ));
    lines.push(render_border_line(BorderKind::Middle, &table.column_widths));
    let row_count = table.rows.len();
    for (i, row) in table.rows.iter().enumerate() {
        lines.extend(render_row_lines(
            row,
            &table.align,
            &table.column_widths,
            false,
        ));
        if i + 1 < row_count {
            lines.push(render_border_line(BorderKind::Middle, &table.column_widths));
        }
    }
    lines.push(render_border_line(BorderKind::Bottom, &table.column_widths));

    // Post-render safety check: fall back to vertical format if any line
    // overflows the terminal.
    let max_line_width = lines
        .iter()
        .map(|line| WidthStr::width(line.as_str()))
        .max()
        .unwrap_or(0);
    if max_line_width > terminal_width.saturating_sub(SAFETY_MARGIN) {
        return render_vertical_format(table, terminal_width);
    }

    RenderedTable { lines }
}

/// Render the table in vertical (key-value) format.
///
/// Each row of the source becomes a block of label/value pairs:
///
/// ```text
/// Header1: value
/// Header2: value
///   continuation
/// ──────────
/// Header1: value
/// ...
/// ```
///
/// Two-pass wrapping (a narrower first line because the label takes space,
/// then wider continuation lines) is not done here: this crate does not
/// own ANSI-aware wrapping. The renderer treats each cell's first
/// pre-wrapped line as the value after the label and emits the remaining
/// pre-wrapped lines unchanged, indented. Callers that want exact
/// two-pass wrapping pre-compute the cell lines before calling in.
pub fn render_vertical_format(table: &TableInput, terminal_width: usize) -> RenderedTable {
    const ANSI_BOLD_START: &str = "\x1b[1m";
    const ANSI_BOLD_END: &str = "\x1b[22m";
    const WRAP_INDENT: &str = "  ";

    let mut lines = Vec::new();
    let separator_width = terminal_width.saturating_sub(1).min(40);
    let separator: String = (0..separator_width).map(|_| '─').collect();

    // Headers: extract the first line of each cell as the label.
    let labels: Vec<String> = table
        .header
        .iter()
        .map(|cell| cell.lines.first().cloned().unwrap_or_default())
        .collect();

    for (row_idx, row) in table.rows.iter().enumerate() {
        if row_idx > 0 {
            lines.push(separator.clone());
        }
        for (col_idx, cell) in row.iter().enumerate() {
            // Label fallback uses a non-empty header label, otherwise `Column {index}`.
            let label = if labels.get(col_idx).map(|l| !l.is_empty()).unwrap_or(false) {
                labels[col_idx].clone()
            } else {
                format!("Column {}", col_idx + 1)
            };

            let mut cell_lines = cell.lines.iter().filter(|l| !l.trim().is_empty());
            let first = cell_lines.next().cloned().unwrap_or_default();
            lines.push(format!("{ANSI_BOLD_START}{label}:{ANSI_BOLD_END} {first}"));
            for cont in cell_lines {
                lines.push(format!("{WRAP_INDENT}{cont}"));
            }
        }
    }

    RenderedTable { lines }
}

/// Return the maximum number of pre-wrapped lines across the cells in
/// `row`, treating an empty cell as one line. The horizontal-vs-vertical
/// switch is computed from this same value.
pub fn max_row_lines(row: &[CellLines]) -> usize {
    row.iter()
        .map(|cell| cell.lines.len().max(1))
        .max()
        .unwrap_or(0)
}

/// Whether the table should be drawn vertically: true when any row —
/// header or body — spans more than [`MAX_ROW_LINES`] pre-wrapped lines,
/// with the row count floored at one so an empty table stays horizontal.
///
/// Exposed so callers can make the layout decision before calling
/// [`render_horizontal_table`], for example to pre-allocate or to route
/// the output to a different widget.
pub fn use_vertical_format(table: &TableInput) -> bool {
    let header_max = max_row_lines(&table.header);
    let body_max = table
        .rows
        .iter()
        .map(|row| max_row_lines(row))
        .max()
        .unwrap_or(0);
    header_max.max(body_max).max(1) > MAX_ROW_LINES
}

/// Forwarder to [`compute_available_width`] for renderer-side callers that
/// want the border-overhead arithmetic without importing `table_layout`
/// separately.
pub fn available_width(num_cols: usize, terminal_width: usize) -> usize {
    compute_available_width(num_cols, terminal_width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table_layout::compute_column_widths;

    fn cell(s: &str) -> CellLines {
        CellLines::single(s)
    }

    fn cell_lines(lines: &[&str]) -> CellLines {
        CellLines {
            lines: lines.iter().map(|s| s.to_string()).collect(),
        }
    }

    // -----------------------------------------------------------------
    // render_border_line — exhaustive
    // -----------------------------------------------------------------

    #[test]
    fn border_top_two_columns() {
        // widths [3, 5] → ┌─────┬───────┐
        // (3+2 = 5 bars, then ┬, then 5+2 = 7 bars, then ┐)
        assert_eq!(
            render_border_line(BorderKind::Top, &[3, 5]),
            "┌─────┬───────┐"
        );
    }

    #[test]
    fn border_middle_two_columns() {
        assert_eq!(
            render_border_line(BorderKind::Middle, &[3, 5]),
            "├─────┼───────┤"
        );
    }

    #[test]
    fn border_bottom_two_columns() {
        assert_eq!(
            render_border_line(BorderKind::Bottom, &[3, 5]),
            "└─────┴───────┘"
        );
    }

    #[test]
    fn border_single_column() {
        assert_eq!(render_border_line(BorderKind::Top, &[4]), "┌──────┐");
        assert_eq!(render_border_line(BorderKind::Middle, &[4]), "├──────┤");
        assert_eq!(render_border_line(BorderKind::Bottom, &[4]), "└──────┘");
    }

    #[test]
    fn border_three_columns() {
        // widths [2, 3, 4]
        // ┌────┬─────┬──────┐
        let expected = "┌────┬─────┬──────┐";
        assert_eq!(render_border_line(BorderKind::Top, &[2, 3, 4]), expected);
    }

    // -----------------------------------------------------------------
    // render_row_lines — single-line cells
    // -----------------------------------------------------------------

    #[test]
    fn single_line_row_with_left_alignment() {
        let cells = vec![cell("hi"), cell("world")];
        let align = vec![Alignment::Left, Alignment::Left];
        let widths = vec![5, 7];
        let out = render_row_lines(&cells, &align, &widths, false);
        // "│ hi    │ world   │"
        assert_eq!(out, vec!["│ hi    │ world   │".to_string()]);
    }

    #[test]
    fn single_line_row_with_right_alignment() {
        let cells = vec![cell("hi"), cell("world")];
        let align = vec![Alignment::Right, Alignment::Right];
        let widths = vec![5, 7];
        let out = render_row_lines(&cells, &align, &widths, false);
        assert_eq!(out, vec!["│    hi │   world │".to_string()]);
    }

    #[test]
    fn header_row_forces_center_alignment() {
        // Even with align=Left configured, header row centers cells.
        let cells = vec![cell("A"), cell("B")];
        let align = vec![Alignment::Left, Alignment::Left];
        let widths = vec![5, 5];
        let out = render_row_lines(&cells, &align, &widths, true);
        // padding = 5-1 = 4 → left pad 2, right pad 2 → "  A  "
        assert_eq!(out, vec!["│   A   │   B   │".to_string()]);
        // Wait — the visible cell content is " " + padded + " " on each side.
        // Padded "A" with width 5 center: "  A  " (2 left, 2 right)
        // → " " + "  A  " + " │" = " " + "  A  " + " │" → "│   A   │   B   │"
    }

    // -----------------------------------------------------------------
    // render_row_lines — multi-line cells & vertical centering
    // -----------------------------------------------------------------

    #[test]
    fn multiline_cell_pads_taller_neighbor_with_blanks() {
        // Cell A has 1 line, cell B has 3, so the row's tallest cell is 3
        // lines.
        // Cell A's offset = floor((3-1)/2) = 1 → A appears on row 1.
        // Cell B's offset = 0.
        let cells = vec![cell("A"), cell_lines(&["B1", "B2", "B3"])];
        let align = vec![Alignment::Left, Alignment::Left];
        let widths = vec![3, 3];
        let out = render_row_lines(&cells, &align, &widths, false);
        assert_eq!(
            out,
            vec![
                "│     │ B1  │".to_string(),
                "│ A   │ B2  │".to_string(),
                "│     │ B3  │".to_string(),
            ]
        );
    }

    #[test]
    fn multiline_centering_rounds_left_pad_down() {
        // Cell with 1 line vs cell with 4 lines.
        // offset = floor((4-1)/2) = 1 → A on row 1, blanks on rows 0, 2, 3.
        let cells = vec![cell("A"), cell_lines(&["B1", "B2", "B3", "B4"])];
        let align = vec![Alignment::Left, Alignment::Left];
        let widths = vec![3, 3];
        let out = render_row_lines(&cells, &align, &widths, false);
        assert_eq!(
            out,
            vec![
                "│     │ B1  │".to_string(),
                "│ A   │ B2  │".to_string(),
                "│     │ B3  │".to_string(),
                "│     │ B4  │".to_string(),
            ]
        );
    }

    #[test]
    fn empty_cell_renders_as_single_blank_line() {
        let cells = vec![CellLines { lines: vec![] }, cell("X")];
        let align = vec![Alignment::Left, Alignment::Left];
        let widths = vec![3, 3];
        let out = render_row_lines(&cells, &align, &widths, false);
        assert_eq!(out, vec!["│     │ X   │".to_string()]);
    }

    // -----------------------------------------------------------------
    // CJK / display-width handling
    // -----------------------------------------------------------------

    #[test]
    fn cjk_cell_uses_display_width_not_byte_count() {
        // "你好" has display width 4, not 6 bytes. Padding should
        // line up to 4-column total: width 4, content "你好" → no
        // padding (display=4, target=4).
        let cells = vec![cell("你好")];
        let align = vec![Alignment::Left];
        let widths = vec![4];
        let out = render_row_lines(&cells, &align, &widths, false);
        assert_eq!(out, vec!["│ 你好 │".to_string()]);
    }

    // -----------------------------------------------------------------
    // use_vertical_format / max_row_lines
    // -----------------------------------------------------------------

    #[test]
    fn use_vertical_format_false_for_short_table() {
        let table = TableInput::new(
            vec![cell("A"), cell("B")],
            vec![vec![cell("a"), cell("b")]],
            vec![Alignment::Left, Alignment::Left],
            ColumnLayout {
                widths: vec![3, 3],
                needs_hard_wrap: false,
            },
        );
        assert!(!use_vertical_format(&table));
    }

    #[test]
    fn use_vertical_format_true_when_row_exceeds_max_row_lines() {
        // 5 lines in a single cell > MAX_ROW_LINES (4)
        let table = TableInput::new(
            vec![cell("A"), cell("B")],
            vec![vec![cell_lines(&["1", "2", "3", "4", "5"]), cell("b")]],
            vec![Alignment::Left, Alignment::Left],
            ColumnLayout {
                widths: vec![3, 3],
                needs_hard_wrap: false,
            },
        );
        assert!(use_vertical_format(&table));
    }

    #[test]
    fn max_row_lines_floors_at_one_for_empty_cells() {
        let row = vec![CellLines { lines: vec![] }, cell("x")];
        assert_eq!(max_row_lines(&row), 1);
    }

    // -----------------------------------------------------------------
    // render_horizontal_table — full integration
    // -----------------------------------------------------------------

    #[test]
    fn horizontal_table_assembles_borders_and_rows() {
        let layout = compute_column_widths(&[3, 3], &[3, 3], 80);
        let table = TableInput::new(
            vec![cell("A"), cell("B")],
            vec![vec![cell("a1"), cell("b1")], vec![cell("a2"), cell("b2")]],
            vec![Alignment::Left, Alignment::Left],
            layout,
        );
        let rendered = render_horizontal_table(&table, 80);
        assert_eq!(
            rendered.lines,
            vec![
                "┌─────┬─────┐".to_string(),
                "│  A  │  B  │".to_string(),
                "├─────┼─────┤".to_string(),
                "│ a1  │ b1  │".to_string(),
                "├─────┼─────┤".to_string(),
                "│ a2  │ b2  │".to_string(),
                "└─────┴─────┘".to_string(),
            ]
        );
    }

    #[test]
    fn horizontal_table_no_middle_border_after_last_row() {
        // Single-row table — no `├` line at all.
        let layout = compute_column_widths(&[3], &[3], 80);
        let table = TableInput::new(
            vec![cell("H")],
            vec![vec![cell("v")]],
            vec![Alignment::Left],
            layout,
        );
        let rendered = render_horizontal_table(&table, 80);
        assert_eq!(
            rendered.lines,
            vec![
                "┌─────┐".to_string(),
                "│  H  │".to_string(),
                "├─────┤".to_string(),
                "│ v   │".to_string(),
                "└─────┘".to_string(),
            ]
        );
    }

    #[test]
    fn horizontal_table_falls_back_to_vertical_when_row_too_tall() {
        // 5-line cell > MAX_ROW_LINES → render_horizontal_table
        // routes to render_vertical_format upfront.
        let table = TableInput::new(
            vec![cell("A"), cell("B")],
            vec![vec![cell_lines(&["1", "2", "3", "4", "5"]), cell("v")]],
            vec![Alignment::Left, Alignment::Left],
            ColumnLayout {
                widths: vec![3, 3],
                needs_hard_wrap: false,
            },
        );
        let rendered = render_horizontal_table(&table, 80);
        // Vertical format starts with the first row's first label,
        // not with a `┌` border.
        assert!(
            !rendered.lines[0].starts_with("┌"),
            "expected vertical fallback, got: {:?}",
            rendered.lines
        );
    }

    #[test]
    fn horizontal_table_falls_back_to_vertical_when_too_wide_for_terminal() {
        // Width exceeds terminal-SAFETY_MARGIN → safety-check fallback.
        let layout = ColumnLayout {
            widths: vec![20, 20],
            needs_hard_wrap: false,
        };
        let table = TableInput::new(
            vec![cell("A"), cell("B")],
            vec![vec![cell("aaaa"), cell("bbbb")]],
            vec![Alignment::Left, Alignment::Left],
            layout,
        );
        // terminal_width = 30, SAFETY_MARGIN = 4 → max allowed = 26.
        // The horizontal output is roughly 47 columns wide
        // (20 + 20 + borders + spaces), which exceeds 26.
        let rendered = render_horizontal_table(&table, 30);
        assert!(
            !rendered.lines[0].starts_with("┌"),
            "expected vertical fallback, got: {:?}",
            rendered.lines
        );
    }

    // -----------------------------------------------------------------
    // render_vertical_format
    // -----------------------------------------------------------------

    #[test]
    fn vertical_format_emits_label_value_pairs() {
        let table = TableInput::new(
            vec![cell("Name"), cell("Value")],
            vec![
                vec![cell("Alice"), cell("42")],
                vec![cell("Bob"), cell("17")],
            ],
            vec![Alignment::Left, Alignment::Left],
            ColumnLayout {
                widths: vec![10, 10],
                needs_hard_wrap: false,
            },
        );
        let rendered = render_vertical_format(&table, 80);
        // Each row produces 2 label/value lines, separated by ── line
        // between rows.
        // Lines:
        //   "\x1b[1mName:\x1b[22m Alice"
        //   "\x1b[1mValue:\x1b[22m 42"
        //   "──────...──────" (40 chars)
        //   "\x1b[1mName:\x1b[22m Bob"
        //   "\x1b[1mValue:\x1b[22m 17"
        assert_eq!(rendered.lines.len(), 5);
        assert!(rendered.lines[0].contains("Name:"));
        assert!(rendered.lines[0].contains("Alice"));
        assert!(rendered.lines[1].contains("Value:"));
        assert!(rendered.lines[1].contains("42"));
        assert!(rendered.lines[2].chars().all(|c| c == '─'));
        assert_eq!(rendered.lines[2].chars().count(), 40);
        assert!(rendered.lines[3].contains("Name:"));
        assert!(rendered.lines[3].contains("Bob"));
        assert!(rendered.lines[4].contains("Value:"));
        assert!(rendered.lines[4].contains("17"));
    }

    #[test]
    fn vertical_format_uses_column_n_when_header_empty() {
        let table = TableInput::new(
            vec![CellLines { lines: vec![] }, CellLines { lines: vec![] }],
            vec![vec![cell("v1"), cell("v2")]],
            vec![Alignment::Left, Alignment::Left],
            ColumnLayout {
                widths: vec![5, 5],
                needs_hard_wrap: false,
            },
        );
        let rendered = render_vertical_format(&table, 80);
        assert!(rendered.lines[0].contains("Column 1:"));
        assert!(rendered.lines[1].contains("Column 2:"));
    }

    #[test]
    fn vertical_format_separator_caps_at_40_columns() {
        // terminal=200 but separator width capped at 40.
        let table = TableInput::new(
            vec![cell("A")],
            vec![vec![cell("v1")], vec![cell("v2")]],
            vec![Alignment::Left],
            ColumnLayout {
                widths: vec![5],
                needs_hard_wrap: false,
            },
        );
        let rendered = render_vertical_format(&table, 200);
        // Separator is at index 1 (after the first row's 1 line).
        let sep = &rendered.lines[1];
        assert_eq!(sep.chars().count(), 40);
        assert!(sep.chars().all(|c| c == '─'));
    }

    // -----------------------------------------------------------------
    // Cross-module sanity — layout + render together
    // -----------------------------------------------------------------

    #[test]
    fn layout_and_render_compose_for_realistic_table() {
        // Three columns, mixed alignment, widths chosen by layout.
        let min = vec![3, 3, 3];
        let ideal = vec![6, 4, 8];
        let avail = compute_available_width(3, 80);
        let layout = compute_column_widths(&min, &ideal, avail);
        let table = TableInput::new(
            vec![cell("Col1"), cell("Col2"), cell("Col3")],
            vec![vec![cell("aaa"), cell("bb"), cell("cccccc")]],
            vec![Alignment::Left, Alignment::Center, Alignment::Right],
            layout,
        );
        let rendered = render_horizontal_table(&table, 80);

        // Top + header + middle + 1 row + bottom = 5 lines for a
        // single-data-row table.
        assert_eq!(rendered.lines.len(), 5);
        assert!(rendered.lines[0].starts_with("┌"));
        assert!(rendered.lines.last().unwrap().starts_with("└"));
    }
}
