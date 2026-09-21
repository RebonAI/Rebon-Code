/// The separator drawn dim between byline items: a middot with one space
/// on each side.
///
/// The surrounding spaces are load-bearing — callers join their raw
/// fragments with this literal and must not pad it themselves.
pub const BYLINE_SEPARATOR: &str = " · ";

/// Indices in `children` *after which* a separator belongs.
///
/// An `n`-element slice yields `[1, 2, …, n-1]`: every element but the
/// first is preceded by exactly one separator. An empty slice yields no
/// indices.
pub fn byline_separator_indices(children: &[&str]) -> Vec<usize> {
    if children.is_empty() {
        return Vec::new();
    }
    (1..children.len()).collect()
}

/// Flatten a byline into one string, joining the visible children with
/// [`BYLINE_SEPARATOR`].
///
/// Empty children are invisible and dropped before joining: `""` is how
/// a caller marks a fragment it conditionally suppressed, and a
/// suppressed fragment must not leave a dangling separator. A byline
/// with nothing visible renders as an empty string.
pub fn byline_render(children: &[&str]) -> String {
    let visible: Vec<&str> = children.iter().copied().filter(|s| !s.is_empty()).collect();
    if visible.is_empty() {
        return String::new();
    }
    visible.join(BYLINE_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separator_is_pinned() {
        // Load-bearing: leading and trailing spaces matter.
        assert_eq!(BYLINE_SEPARATOR, " · ");
        assert_eq!(BYLINE_SEPARATOR.chars().count(), 3);
    }

    #[test]
    fn empty_children_yields_empty_string() {
        // No visible children renders nothing.
        assert_eq!(byline_render(&[]), "");
    }

    #[test]
    fn single_child_no_separator() {
        assert_eq!(byline_render(&["esc to cancel"]), "esc to cancel");
    }

    #[test]
    fn two_children_one_separator() {
        let r = byline_render(&["Enter to confirm", "Esc to cancel"]);
        assert_eq!(r, "Enter to confirm · Esc to cancel");
    }

    #[test]
    fn three_children_two_separators() {
        let r = byline_render(&["a", "b", "c"]);
        assert_eq!(r, "a · b · c");
    }

    #[test]
    fn empty_strings_filtered_out() {
        // Empty strings count as invisible and are dropped.
        let r = byline_render(&["a", "", "c"]);
        assert_eq!(r, "a · c");
    }

    #[test]
    fn all_empty_yields_empty_string() {
        let r = byline_render(&["", "", ""]);
        assert_eq!(r, "");
    }

    #[test]
    fn separator_indices_empty() {
        assert!(byline_separator_indices(&[]).is_empty());
    }

    #[test]
    fn separator_indices_single() {
        // No separators for a single element.
        assert!(byline_separator_indices(&["a"]).is_empty());
    }

    #[test]
    fn separator_indices_three() {
        assert_eq!(byline_separator_indices(&["a", "b", "c"]), vec![1, 2]);
    }
}
