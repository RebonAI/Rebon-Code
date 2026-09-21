// Text width / wrapping helpers
// ---------------------------------------------------------------------------

/// Terminal-column-width aware wrap calculation.
///
/// Uses `unicode-width` so CJK / emoji / combining-mark cells
/// measure correctly. An earlier cut of this function used
/// `chars().count()` which is ASCII-only.
pub fn wrap_height(text: &str, width: u16) -> u16 {
    use rebon_width::WidthStr;
    if width == 0 {
        return 1;
    }
    let w = width as usize;
    let mut count = 0u16;
    for logical in text.split('\n') {
        let dw = WidthStr::width(logical).max(1);
        let lines = dw.div_ceil(w);
        count = count.saturating_add(lines as u16);
    }
    count.max(1)
}

#[cfg(test)]
mod tests {
    use super::wrap_height;

    #[test]
    fn wrap_height_cjk_display_width() {
        assert_eq!(wrap_height("你好", 4), 1);
        assert_eq!(wrap_height("你好", 3), 2);
    }

    #[test]
    fn wrap_height_emoji_matches_ascii_equivalent() {
        // `🙂🙂` is 4 columns wide; `abcd` is 4 columns wide.
        assert_eq!(wrap_height("🙂🙂", 4), wrap_height("abcd", 4));
    }

    #[test]
    fn wrap_height_combining_marks_are_zero_width() {
        assert_eq!(wrap_height("a\u{0301}", 10), 1);
    }
}
