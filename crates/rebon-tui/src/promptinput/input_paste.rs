//! Truncation of very long pasted text into a `[...Truncated text #N]`
//! placeholder backed by the pasted-content store.

use std::collections::BTreeMap;

/// Input length, in bytes, above which the middle is truncated.
pub const TRUNCATION_THRESHOLD: usize = 10_000;
/// Bytes kept from the head and tail combined (half each).
pub const PREVIEW_LENGTH: usize = 1_000;

/// Line count shown in a pasted-text reference.
///
/// Counts newline separators rather than the number of
/// logical lines, so a single-line string yields `0`.
pub fn pasted_text_ref_num_lines(text: &str) -> usize {
    let mut count = 0;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\n' => count += 1,
            '\r' => {
                count += 1;
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            _ => {}
        }
    }

    count
}

/// Minimal pasted-content shape this helper writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedContent {
    /// Placeholder id.
    pub id: u32,
    /// Only `"text"` is produced by this helper.
    pub content_type: &'static str,
    /// Stored placeholder payload.
    pub content: String,
}

/// Output of [`maybe_truncate_message_for_input`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedMessage {
    /// Truncated text inserted into the prompt.
    pub truncated_text: String,
    /// Removed middle portion stored separately.
    pub placeholder_content: String,
}

/// Output of [`maybe_truncate_input`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncateInputResult {
    /// New prompt input.
    pub new_input: String,
    /// New pasted-content store.
    pub new_pasted_contents: BTreeMap<u32, PastedContent>,
}

/// Keep the head and tail of text longer than [`TRUNCATION_THRESHOLD`] and
/// replace the middle with a truncated-text reference numbered `next_paste_id`.
pub fn maybe_truncate_message_for_input(
    text: &str,
    next_paste_id: u32,
    count_lines: impl Fn(&str) -> usize,
) -> TruncatedMessage {
    if text.len() <= TRUNCATION_THRESHOLD {
        return TruncatedMessage {
            truncated_text: text.to_string(),
            placeholder_content: String::new(),
        };
    }

    let start_length = PREVIEW_LENGTH / 2;
    let end_length = PREVIEW_LENGTH / 2;
    let start_text = &text[..start_length];
    let end_text = &text[text.len() - end_length..];
    let placeholder_content = text[start_length..text.len() - end_length].to_string();
    let truncated_lines = count_lines(&placeholder_content);
    let placeholder_ref = format_truncated_text_ref(next_paste_id, truncated_lines);

    TruncatedMessage {
        truncated_text: format!("{start_text}{placeholder_ref}{end_text}"),
        placeholder_content,
    }
}

/// Truncate the input if needed and store the removed middle under the next
/// free paste id.
pub fn maybe_truncate_input(
    input: &str,
    pasted_contents: &BTreeMap<u32, PastedContent>,
    count_lines: impl Fn(&str) -> usize,
) -> TruncateInputResult {
    let next_paste_id = pasted_contents.keys().max().copied().unwrap_or(0) + 1;
    let truncated = maybe_truncate_message_for_input(input, next_paste_id, count_lines);

    if truncated.placeholder_content.is_empty() {
        return TruncateInputResult {
            new_input: input.to_string(),
            new_pasted_contents: pasted_contents.clone(),
        };
    }

    let mut next_store = pasted_contents.clone();
    next_store.insert(
        next_paste_id,
        PastedContent {
            id: next_paste_id,
            content_type: "text",
            content: truncated.placeholder_content.clone(),
        },
    );
    TruncateInputResult {
        new_input: truncated.truncated_text,
        new_pasted_contents: next_store,
    }
}

fn format_truncated_text_ref(id: u32, num_lines: usize) -> String {
    format!("[...Truncated text #{id} +{num_lines} lines...]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_not_truncated() {
        let result = maybe_truncate_message_for_input("hello", 1, pasted_text_ref_num_lines);
        assert_eq!(
            result,
            TruncatedMessage {
                truncated_text: "hello".into(),
                placeholder_content: String::new(),
            }
        );
    }

    #[test]
    fn long_text_keeps_head_and_tail_and_inserts_placeholder() {
        let text = format!(
            "{}{}{}",
            "a".repeat(500),
            "b".repeat(10050),
            "c".repeat(500)
        );
        let result = maybe_truncate_message_for_input(&text, 7, pasted_text_ref_num_lines);
        assert!(result.truncated_text.starts_with(&"a".repeat(500)));
        assert!(result.truncated_text.ends_with(&"c".repeat(500)));
        assert!(result
            .truncated_text
            .contains("[...Truncated text #7 +0 lines...]"));
        assert_eq!(result.placeholder_content.len(), 10050);
    }

    #[test]
    fn maybe_truncate_input_preserves_store_when_not_truncated() {
        let mut store = BTreeMap::new();
        store.insert(
            1,
            PastedContent {
                id: 1,
                content_type: "text",
                content: "existing".into(),
            },
        );
        let result = maybe_truncate_input("hello", &store, pasted_text_ref_num_lines);
        assert_eq!(result.new_input, "hello");
        assert_eq!(result.new_pasted_contents, store);
    }

    #[test]
    fn maybe_truncate_input_appends_next_id() {
        let mut store = BTreeMap::new();
        store.insert(
            2,
            PastedContent {
                id: 2,
                content_type: "text",
                content: "existing".into(),
            },
        );
        let text = "x".repeat(TRUNCATION_THRESHOLD + 50);
        let result = maybe_truncate_input(&text, &store, pasted_text_ref_num_lines);
        assert!(result.new_input.contains("#3"));
        assert!(result.new_pasted_contents.contains_key(&3));
    }

    #[test]
    fn pasted_text_ref_num_lines_matches_history_helper_semantics() {
        assert_eq!(pasted_text_ref_num_lines("single line"), 0);
        assert_eq!(pasted_text_ref_num_lines("a\nb\nc"), 2);
        assert_eq!(pasted_text_ref_num_lines("a\r\nb\rc"), 2);
    }
}
