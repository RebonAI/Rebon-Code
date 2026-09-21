//! The display-width policy has to reach the cell buffer, not just
//! Rebon's own layout code.
//!
//! Rebon measures a string in one place (`rebon-width`) and ratatui lays
//! it into cells in another; if the two disagree about an East Asian
//! ambiguous character, the terminal's cursor ends up a column away from
//! where the next partial redraw thinks it is, and the redraw writes into
//! the wrong cell and cannot erase what it left behind. That is the
//! `· ingers 9 min      ed` defect from RFC-0004's Agent View rows.
//!
//! This lives in its own test binary, as one test, because the policy is
//! process-wide: flipping it beside another test — in this file or in the
//! rest of the suite — would change what that one measures.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

/// `·` (U+00B7) — ambiguous, and one of the two the defect was found
/// with. `x` after it is what a partial redraw would go on to overwrite.
const AMBIGUOUS_THEN_ASCII: &str = "·x";

fn row(text: &str) -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
    buffer.set_string(0, 0, text, Style::default());
    buffer
}

fn symbols(buffer: &Buffer) -> Vec<&str> {
    (0..4).map(|x| buffer[(x, 0)].symbol()).collect()
}

#[test]
fn the_policy_decides_how_many_cells_an_ambiguous_character_takes() {
    rebon_width::set_ambiguous_wide(false);
    let narrow = row(AMBIGUOUS_THEN_ASCII);
    assert_eq!(symbols(&narrow), ["·", "x", " ", " "]);
    assert_eq!(Line::from(AMBIGUOUS_THEN_ASCII).width(), 2);

    rebon_width::set_ambiguous_wide(true);
    let wide = row(AMBIGUOUS_THEN_ASCII);
    // The cell the wide glyph covers is left blank and `x` moves over,
    // which is what the terminal actually paints.
    assert_eq!(symbols(&wide), ["·", " ", "x", " "]);
    assert_eq!(Line::from(AMBIGUOUS_THEN_ASCII).width(), 3);

    // Nothing else moves: ASCII stays one cell and an ideograph two,
    // under either policy.
    for wide_policy in [false, true] {
        rebon_width::set_ambiguous_wide(wide_policy);
        assert_eq!(symbols(&row("a漢")), ["a", "漢", " ", " "]);
    }

    // A redraw that changes what follows an ambiguous character has to
    // leave the cell the glyph covers alone and start from the one after
    // it. `Buffer::diff` decides that, from the same policy.
    rebon_width::set_ambiguous_wide(true);
    let before = row("·ed");
    let after = row("·9 min");

    let updated: Vec<u16> = before.diff(&after).into_iter().map(|(x, ..)| x).collect();
    // Column 1 is the half the `·` covers — nothing is ever written
    // there — and the first real difference is at column 2.
    assert!(
        !updated.contains(&1),
        "the cell under the wide glyph must not be written: {updated:?}"
    );
    assert!(updated.contains(&2), "columns updated: {updated:?}");

    rebon_width::set_ambiguous_wide(false);
    let narrow_before = row("·ed");
    let narrow_after = row("·9 min");
    let narrow_updated: Vec<u16> = narrow_before
        .diff(&narrow_after)
        .into_iter()
        .map(|(x, ..)| x)
        .collect();
    // Narrow, the same text starts differing one column earlier.
    assert!(narrow_updated.contains(&1), "columns: {narrow_updated:?}");
}
