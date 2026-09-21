pub(super) fn last_non_empty_line(text: &str) -> String {
    for line in text.lines().rev() {
        let trimmed = line.trim_end();
        if !trimmed.trim().is_empty() {
            return trimmed.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::last_non_empty_line;

    #[test]
    fn last_non_empty_line_picks_newest_substantive_line() {
        assert_eq!(last_non_empty_line(""), "");
        assert_eq!(last_non_empty_line("\n\n"), "");
        assert_eq!(last_non_empty_line("one line"), "one line");
        assert_eq!(last_non_empty_line("first\nsecond\nthird"), "third");
        // Trailing blank lines must be skipped.
        assert_eq!(last_non_empty_line("first\nsecond\n\n  \n"), "second");
        // Trailing whitespace on the picked line is trimmed.
        assert_eq!(last_non_empty_line("alpha   "), "alpha");
    }
}
