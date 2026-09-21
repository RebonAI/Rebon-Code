//! Display-width-aware left/right/center padding.
//!
//! ## Behavior
//!
//! `pad_aligned` computes `padding = target_width.saturating_sub(display_width)`
//! and returns `content` unchanged when that is zero. Otherwise it fills
//! `padding` spaces according to `align`: `Alignment::Left` writes the
//! content first and then the padding spaces; `Alignment::Right` writes
//! the padding spaces first and then the content; `Alignment::Center`
//! splits the padding into a left pad of `padding / 2` (rounded down) and
//! a right pad of `padding - left_pad`, so an odd extra column lands on
//! the right.
//!
//! Three load-bearing quirks this implementation preserves:
//!
//! 1. **`display_width` is supplied by the caller**, not computed from
//!    `content`. The function takes both because the caller has
//!    already measured the width before calling, and avoiding the
//!    recompute matters in tight table loops.
//! 2. **`Alignment::Left` is the default.** Only `Alignment::Center`
//!    and `Alignment::Right` select non-default behaviour — left-align
//!    pads on the right.
//! 3. **Center align rounds the LEFT pad down** (integer `padding / 2`), so
//!    odd total padding leaves the extra space on the **right**.
//!    Tests pin this exactly because it's user-visible (off-by-one
//!    in centered headers).

/// Alignment discriminant for [`pad_aligned`]: left (the default, which
/// pads on the right), center (splits the padding, extra column on the
/// right) or right (pads on the left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    Left,
    Center,
    Right,
}

/// Pad `content` to `target_width` columns using `display_width` as
/// the caller-supplied width measurement.
///
/// `display_width` should be the result of a display-width
/// measurement of `content` (e.g. `rebon_width::str_width`)
/// — that is, the number of terminal columns the content occupies,
/// not the number of bytes or chars. Passing a wrong width still
/// produces a defined output (the function will pad to whatever
/// makes `display_width + padding == target_width`), but the
/// result will be visually off.
///
/// Returns `content` unchanged if `display_width >= target_width`
/// (the `saturating_sub` clamp).
pub fn pad_aligned(
    content: &str,
    display_width: usize,
    target_width: usize,
    align: Alignment,
) -> String {
    let padding = target_width.saturating_sub(display_width);
    if padding == 0 {
        return content.to_string();
    }
    match align {
        Alignment::Center => {
            let left_pad = padding / 2;
            let right_pad = padding - left_pad;
            let mut out = String::with_capacity(content.len() + padding);
            for _ in 0..left_pad {
                out.push(' ');
            }
            out.push_str(content);
            for _ in 0..right_pad {
                out.push(' ');
            }
            out
        }
        Alignment::Right => {
            let mut out = String::with_capacity(content.len() + padding);
            for _ in 0..padding {
                out.push(' ');
            }
            out.push_str(content);
            out
        }
        Alignment::Left => {
            let mut out = String::with_capacity(content.len() + padding);
            out.push_str(content);
            for _ in 0..padding {
                out.push(' ');
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Per-alignment basic cases
    // -----------------------------------------------------------------

    #[test]
    fn left_align_pads_on_right() {
        assert_eq!(pad_aligned("hi", 2, 6, Alignment::Left), "hi    ");
    }

    #[test]
    fn right_align_pads_on_left() {
        assert_eq!(pad_aligned("hi", 2, 6, Alignment::Right), "    hi");
    }

    #[test]
    fn center_align_with_even_padding() {
        // padding = 4 → leftPad=2, rightPad=2
        assert_eq!(pad_aligned("hi", 2, 6, Alignment::Center), "  hi  ");
    }

    #[test]
    fn center_align_with_odd_padding_rounds_left_down() {
        // padding = 5 → leftPad=2, rightPad=3
        // Extra column lives on the RIGHT (the integer `padding / 2` truncates down).
        assert_eq!(pad_aligned("hi", 2, 7, Alignment::Center), "  hi   ");
    }

    // -----------------------------------------------------------------
    // No-pad / overflow cases
    // -----------------------------------------------------------------

    #[test]
    fn target_equal_to_display_width_returns_unchanged() {
        assert_eq!(pad_aligned("hello", 5, 5, Alignment::Left), "hello");
        assert_eq!(pad_aligned("hello", 5, 5, Alignment::Right), "hello");
        assert_eq!(pad_aligned("hello", 5, 5, Alignment::Center), "hello");
    }

    #[test]
    fn target_smaller_than_display_returns_unchanged() {
        // The `saturating_sub` clamp. We do not
        // truncate — that's the caller's job.
        assert_eq!(pad_aligned("hello", 5, 3, Alignment::Left), "hello");
        assert_eq!(pad_aligned("hello", 5, 3, Alignment::Right), "hello");
        assert_eq!(pad_aligned("hello", 5, 3, Alignment::Center), "hello");
    }

    #[test]
    fn empty_content_pads_to_full_width() {
        assert_eq!(pad_aligned("", 0, 4, Alignment::Left), "    ");
        assert_eq!(pad_aligned("", 0, 4, Alignment::Right), "    ");
        assert_eq!(pad_aligned("", 0, 4, Alignment::Center), "    ");
    }

    #[test]
    fn zero_target_width_returns_unchanged() {
        assert_eq!(pad_aligned("hi", 2, 0, Alignment::Left), "hi");
    }

    // -----------------------------------------------------------------
    // Caller-supplied display width — wide chars and ANSI escapes
    // -----------------------------------------------------------------

    #[test]
    fn caller_supplied_width_handles_cjk() {
        // CJK content "你好" measures 4 columns but is 6 bytes. The
        // caller passes 4 as display_width; padding lines up to 6
        // columns total.
        // padding = 6 - 4 = 2
        // Left: 2 trailing spaces.
        // Right: 2 leading spaces.
        // Center: leftPad=1, rightPad=1.
        assert_eq!(pad_aligned("你好", 4, 6, Alignment::Left), "你好  ");
        assert_eq!(pad_aligned("你好", 4, 6, Alignment::Right), "  你好");
        assert_eq!(pad_aligned("你好", 4, 6, Alignment::Center), " 你好 ");
    }

    #[test]
    fn ansi_styled_content_passes_through_padding() {
        // `pad_aligned` does not parse ANSI — it just concatenates.
        // Caller passes the visible width; the bold escapes contribute
        // 0 columns and the function pads as if the content were 5 wide.
        let bold_hello = "\x1b[1mhello\x1b[22m";
        let display_width = 5;
        assert_eq!(
            pad_aligned(bold_hello, display_width, 8, Alignment::Right),
            format!("   {bold_hello}")
        );
    }

    // -----------------------------------------------------------------
    // table
    // -----------------------------------------------------------------

    #[test]
    fn pad_aligned_table() {
        let cases: &[(&str, usize, usize, Alignment, &str)] = &[
            ("hi", 2, 6, Alignment::Left, "hi    "),
            ("hi", 2, 6, Alignment::Right, "    hi"),
            ("hi", 2, 6, Alignment::Center, "  hi  "),
            ("hi", 2, 7, Alignment::Center, "  hi   "), // odd: extra on right
            ("hi", 2, 8, Alignment::Center, "   hi   "),
            ("a", 1, 5, Alignment::Center, "  a  "),
            ("hello", 5, 5, Alignment::Left, "hello"),
            ("hello", 5, 3, Alignment::Left, "hello"),
            ("", 0, 3, Alignment::Center, "   "),
            ("hi", 2, 0, Alignment::Left, "hi"),
        ];
        for (content, display_width, target_width, align, expected) in cases {
            let actual = pad_aligned(content, *display_width, *target_width, *align);
            assert_eq!(
                actual, *expected,
                "case ({content:?}, dw={display_width}, tw={target_width}, {align:?}) failed",
            );
        }
    }
}
