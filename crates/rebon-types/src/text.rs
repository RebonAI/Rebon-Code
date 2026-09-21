//! Small text helpers shared by crates that would otherwise each carry
//! their own copy.

/// Escape the five XML special characters so `input` can be embedded
/// in element text or an attribute value.
pub fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Keep the first `max_chars` characters of `text`, appending `...`
/// when anything was cut off. Counts chars, not bytes, so a multi-byte
/// character is never split.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_replaces_the_five_specials() {
        assert_eq!(
            xml_escape(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn xml_escape_leaves_plain_text_alone() {
        assert_eq!(xml_escape("plain 文本"), "plain 文本");
    }

    #[test]
    fn truncate_chars_keeps_short_text_unchanged() {
        assert_eq!(truncate_chars("abc", 3), "abc");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    #[test]
    fn truncate_chars_appends_ellipsis_when_cut() {
        assert_eq!(truncate_chars("abcdef", 3), "abc...");
    }

    #[test]
    fn truncate_chars_counts_characters_not_bytes() {
        assert_eq!(truncate_chars("你好世界", 2), "你好...");
    }

    #[test]
    fn truncate_chars_zero_limit_keeps_only_the_marker() {
        assert_eq!(truncate_chars("abc", 0), "...");
        assert_eq!(truncate_chars("", 0), "");
    }
}
