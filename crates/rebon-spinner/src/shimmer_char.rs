//! Shimmer-character predicate.
//!
//! The rule is a single boolean: a character is shimmered when its column index
//! is within one cell of the moving glimmer index, i.e.
//! `(index - glimmer_index).abs() <= 1`.

/// Whether the character at `index` is shimmered.
///
/// Returns `true` when `index` is within 1 cell of `glimmer_index`.
/// Both are signed, so negative `glimmer_index` values are handled too
/// (the `STALLED_GLIMMER_INDEX = -100` sentinel and the off-screen
/// sweeps).
pub fn should_use_shimmer(index: i64, glimmer_index: i64) -> bool {
    (index - glimmer_index).abs() <= 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_shimmers() {
        assert!(should_use_shimmer(5, 5));
    }

    #[test]
    fn one_left_shimmers() {
        assert!(should_use_shimmer(4, 5));
    }

    #[test]
    fn one_right_shimmers() {
        assert!(should_use_shimmer(6, 5));
    }

    #[test]
    fn two_left_does_not_shimmer() {
        assert!(!should_use_shimmer(3, 5));
    }

    #[test]
    fn two_right_does_not_shimmer() {
        assert!(!should_use_shimmer(7, 5));
    }

    #[test]
    fn off_screen_negative_glimmer_no_match() {
        // Stalled sentinel — no on-screen index can match.
        assert!(!should_use_shimmer(0, -100));
        assert!(!should_use_shimmer(50, -100));
    }

    #[test]
    fn glimmer_index_zero_matches_zero_one_negone() {
        assert!(should_use_shimmer(0, 0));
        assert!(should_use_shimmer(1, 0));
        assert!(should_use_shimmer(-1, 0));
        assert!(!should_use_shimmer(2, 0));
    }

    #[test]
    fn negative_index_negative_glimmer_works() {
        assert!(should_use_shimmer(-100, -100));
        assert!(should_use_shimmer(-99, -100));
        assert!(!should_use_shimmer(-98, -100));
    }

    #[test]
    fn shimmer_char_table() {
        // Pinned table from running should_use_shimmer by hand for
        // glimmer_index = 5 across the visible window.
        for (idx, expected) in [
            (-1, false),
            (0, false),
            (1, false),
            (2, false),
            (3, false),
            (4, true),
            (5, true),
            (6, true),
            (7, false),
            (8, false),
            (9, false),
            (10, false),
        ] {
            assert_eq!(
                should_use_shimmer(idx, 5),
                expected,
                "index {idx}, glimmer 5: expected {expected}"
            );
        }
    }
}
