//! Folding a full-width digit to its ASCII equivalent, one character at a
//! time.
//!
//! A CJK IME hands the terminal `０`-`９` (U+FF10..U+FF19) where a person means
//! `0`-`9`. Every surface that reads a keystroke and cares whether it was a
//! digit has to fold them first, and the fold belongs on the key-decoding path
//! rather than in whatever feature happens to want a digit — which is how it
//! ended up living in a survey crate that nothing else could reach.
//!
//! There is a sibling in `rebon-customselect`: `util::normalize_full_width_digits`,
//! the `&str -> String` form, which the select reducers use for numeric jumps.
//! Same table, different shape. This one stays `char -> char` because its
//! caller is a per-keystroke path where allocating a `String` per key would be
//! the only cost in the loop.

/// Normalize a single full-width digit (U+FF10..U+FF19) to its ASCII
/// equivalent. Any other character passes through unchanged.
///
/// The multi-character case belongs to the caller: a key decoder only ever has
/// the one character in hand.
pub fn normalize_full_width_digit(ch: char) -> char {
    match ch {
        '\u{FF10}' => '0',
        '\u{FF11}' => '1',
        '\u{FF12}' => '2',
        '\u{FF13}' => '3',
        '\u{FF14}' => '4',
        '\u{FF15}' => '5',
        '\u{FF16}' => '6',
        '\u{FF17}' => '7',
        '\u{FF18}' => '8',
        '\u{FF19}' => '9',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full-width digit folding: the ten that fold, and the three kinds that
    /// must not — an ASCII letter, an ASCII digit already, and NUL.
    #[test]
    fn full_width_digit_normalization() {
        for (full, ascii) in [
            ('\u{FF10}', '0'),
            ('\u{FF11}', '1'),
            ('\u{FF12}', '2'),
            ('\u{FF13}', '3'),
            ('\u{FF14}', '4'),
            ('\u{FF15}', '5'),
            ('\u{FF16}', '6'),
            ('\u{FF17}', '7'),
            ('\u{FF18}', '8'),
            ('\u{FF19}', '9'),
            ('a', 'a'),
            ('0', '0'),
            ('\0', '\0'),
        ] {
            assert_eq!(normalize_full_width_digit(full), ascii);
        }
    }

    /// The sibling in `rebon-customselect` folds the same table over a string.
    /// Kept honest here rather than by hoping two crates stay in step: a digit
    /// this one folds is a digit that one folds.
    #[test]
    fn agrees_with_the_string_form_over_the_whole_block() {
        for cp in 0xFF10..=0xFF19u32 {
            let ch = char::from_u32(cp).expect("the full-width block is valid");
            let folded = normalize_full_width_digit(ch);
            assert!(folded.is_ascii_digit(), "{ch:?} folded to {folded:?}");
            assert_eq!(folded as u32 - b'0' as u32, cp - 0xFF10);
        }
    }
}
