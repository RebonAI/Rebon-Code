//! Memo comparison for a message row: the flags it depends on, the
//! thinking-content probe, and the equality check itself.

use crate::types::{AssistantContentBlock, MessageRow};

/// Minimal set of inputs the row equality check reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageMemoInput {
    /// Current row.
    pub message: MessageRow,
    /// Key of the thinking block that should stay expanded.
    pub last_thinking_block_id: Option<String>,
    /// Verbose render mode.
    pub verbose: bool,
    /// Uuid of the newest user message carrying bash stdout/stderr text.
    pub latest_bash_output_uuid: Option<String>,
    /// Whether the transcript screen is active.
    pub is_transcript_mode: bool,
    /// Width available to the row, when known.
    pub container_width: Option<u16>,
    /// Whether the row has been marked static and can skip re-rendering.
    pub is_static: bool,
}

/// True when an assistant row holds a thinking or redacted-thinking block.
pub fn has_thinking_content(message: &MessageRow) -> bool {
    match message {
        MessageRow::Assistant(assistant) => assistant.content.iter().any(|block| {
            matches!(
                block,
                AssistantContentBlock::Thinking { .. }
                    | AssistantContentBlock::RedactedThinking { .. }
            )
        }),
        _ => false,
    }
}

/// Row equality: a differing uuid, verbose flag, transcript mode or
/// container width reports a change, as do a flipped latest-bash flag and a
/// changed thinking-block key when the next row has thinking content. A row
/// that survives all of that still compares equal only when both sides are
/// static.
pub fn are_message_memo_inputs_equal(prev: &MessageMemoInput, next: &MessageMemoInput) -> bool {
    if prev.message.uuid() != next.message.uuid() {
        return false;
    }
    if prev.last_thinking_block_id != next.last_thinking_block_id
        && has_thinking_content(&next.message)
    {
        return false;
    }
    if prev.verbose != next.verbose {
        return false;
    }
    let prev_is_latest = prev.latest_bash_output_uuid.as_deref() == Some(prev.message.uuid());
    let next_is_latest = next.latest_bash_output_uuid.as_deref() == Some(next.message.uuid());
    if prev_is_latest != next_is_latest {
        return false;
    }
    if prev.is_transcript_mode != next.is_transcript_mode {
        return false;
    }
    if prev.container_width != next.container_width {
        return false;
    }
    if prev.is_static && next.is_static {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, SystemMessage, SystemSubtype};

    fn assistant_with_thinking() -> MessageRow {
        MessageRow::Assistant(AssistantMessage {
            uuid: "a1".into(),
            content: vec![AssistantContentBlock::Thinking {
                thinking: Some("thought".into()),
            }],
            advisor_model: None,
            is_stream_continuation: false,
        })
    }

    fn assistant_without_thinking() -> MessageRow {
        MessageRow::Assistant(AssistantMessage {
            uuid: "a1".into(),
            content: vec![AssistantContentBlock::Text { text: "x".into() }],
            advisor_model: None,
            is_stream_continuation: false,
        })
    }

    fn system(uuid: &str) -> MessageRow {
        MessageRow::System(SystemMessage {
            uuid: uuid.into(),
            subtype: SystemSubtype::Other,
            raw_subtype: Some("informational".into()),
            level: Some("info".into()),
            content: "x".into(),
            stop_hook_summary: None,
        })
    }

    fn input(message: MessageRow) -> MessageMemoInput {
        MessageMemoInput {
            message,
            last_thinking_block_id: None,
            verbose: false,
            latest_bash_output_uuid: None,
            is_transcript_mode: false,
            container_width: Some(80),
            is_static: false,
        }
    }

    #[test]
    fn has_thinking_content_true_for_thinking_and_redacted() {
        assert!(has_thinking_content(&assistant_with_thinking()));
        assert!(has_thinking_content(&MessageRow::Assistant(
            AssistantMessage {
                uuid: "a2".into(),
                content: vec![AssistantContentBlock::RedactedThinking {
                    data: Some("opaque".into()),
                }],
                advisor_model: None,
                is_stream_continuation: false,
            }
        )));
    }

    #[test]
    fn has_thinking_content_false_for_non_assistant() {
        assert!(!has_thinking_content(&system("s1")));
        assert!(!has_thinking_content(&assistant_without_thinking()));
    }

    #[test]
    fn equality_false_when_uuid_changes() {
        assert!(!are_message_memo_inputs_equal(
            &input(system("s1")),
            &input(system("s2"))
        ));
    }

    #[test]
    fn equality_false_when_last_thinking_changes_and_message_has_thinking() {
        let prev = input(assistant_with_thinking());
        let mut next = prev.clone();
        next.last_thinking_block_id = Some("a1:0".into());
        assert!(!are_message_memo_inputs_equal(&prev, &next));
    }

    #[test]
    fn equality_requires_static_even_without_thinking_change() {
        let prev = input(assistant_without_thinking());
        let mut next = prev.clone();
        next.last_thinking_block_id = Some("a1:0".into());
        assert!(!are_message_memo_inputs_equal(&prev, &next));

        let mut prev_static = prev.clone();
        let mut next_static = next.clone();
        prev_static.is_static = true;
        next_static.is_static = true;
        assert!(are_message_memo_inputs_equal(&prev_static, &next_static));
    }

    #[test]
    fn equality_false_on_verbose_change() {
        let prev = input(system("s1"));
        let mut next = prev.clone();
        next.verbose = true;
        assert!(!are_message_memo_inputs_equal(&prev, &next));
    }

    #[test]
    fn equality_false_when_latest_bash_status_flips() {
        let prev = input(system("s1"));
        let mut next = prev.clone();
        next.latest_bash_output_uuid = Some("s1".into());
        assert!(!are_message_memo_inputs_equal(&prev, &next));
    }

    #[test]
    fn equality_false_on_transcript_or_container_width_change() {
        let prev = input(system("s1"));
        let mut next = prev.clone();
        next.is_transcript_mode = true;
        assert!(!are_message_memo_inputs_equal(&prev, &next));

        let mut next2 = prev.clone();
        next2.container_width = Some(100);
        assert!(!are_message_memo_inputs_equal(&prev, &next2));
    }

    #[test]
    fn equality_true_only_for_static_messages_after_all_other_checks() {
        let mut prev = input(system("s1"));
        let mut next = prev.clone();
        prev.is_static = true;
        next.is_static = true;
        assert!(are_message_memo_inputs_equal(&prev, &next));
    }
}
