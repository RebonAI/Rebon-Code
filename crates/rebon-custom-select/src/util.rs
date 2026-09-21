//! Full-width digit / space normalisation helpers.
//!
//! [`normalize_full_width_digits`] and [`normalize_full_width_space`]
//! back the numeric-jump key and the space-toggle key in both the
//! single- and multi-select reducers, so an IME that emits full-width
//! characters behaves identically to an ASCII keyboard.
//!
//! Full-width digits are the contiguous block U+FF10..U+FF19 (０-９),
//! which makes the mapping a simple offset from `b'0'`. Full-width
//! space is U+3000.

/// Replace every full-width digit (U+FF10..U+FF19) with its
/// half-width ASCII equivalent. All other characters pass through.
pub fn normalize_full_width_digits(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            let cp = c as u32;
            if (0xFF10..=0xFF19).contains(&cp) {
                char::from_u32(cp - 0xFF10 + b'0' as u32).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

/// Replace U+3000 (ideographic space) with ASCII space. All other
/// characters pass through.
pub fn normalize_full_width_space(input: &str) -> String {
    input
        .chars()
        .map(|c| if c == '\u{3000}' { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_digits_pass_through() {
        assert_eq!(normalize_full_width_digits("0123456789"), "0123456789");
    }

    #[test]
    fn full_width_digits_normalize() {
        let fw = "\u{FF10}\u{FF11}\u{FF12}\u{FF13}\u{FF14}\u{FF15}\u{FF16}\u{FF17}\u{FF18}\u{FF19}";
        assert_eq!(normalize_full_width_digits(fw), "0123456789");
    }

    #[test]
    fn mixed_full_and_half_width_digits() {
        let mixed = "\u{FF11}2\u{FF13}";
        assert_eq!(normalize_full_width_digits(mixed), "123");
    }

    #[test]
    fn non_digit_characters_unchanged() {
        assert_eq!(normalize_full_width_digits("abc"), "abc");
        assert_eq!(normalize_full_width_digits("\u{4E2D}"), "\u{4E2D}");
    }

    #[test]
    fn ascii_space_passes_through() {
        assert_eq!(normalize_full_width_space("a b"), "a b");
    }

    #[test]
    fn full_width_space_normalizes() {
        assert_eq!(normalize_full_width_space("a\u{3000}b"), "a b");
    }

    #[test]
    fn empty_string() {
        assert_eq!(normalize_full_width_digits(""), "");
        assert_eq!(normalize_full_width_space(""), "");
    }
}
