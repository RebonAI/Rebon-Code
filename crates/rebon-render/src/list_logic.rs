//! Pure list-level helpers over a normalized timeline: streaming-thinking
//! visibility, the last visible thinking block, the newest bash-output
//! message, tool-use id collection and filtering, the "new messages"
//! divider index, the cursor index, and the row expand key.

use std::collections::BTreeSet;

use crate::timeline::{TimelineContentBlock, TimelineMessage};

/// Streaming-thinking state consulted when deciding whether a thinking
/// block is still on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingThinkingState {
    /// True while thinking text is still arriving.
    pub is_streaming: bool,
    /// Epoch-ms instant the thinking stream ended, once it has.
    pub streaming_ended_at_ms: Option<i64>,
}

/// Whether a thinking block should still be shown: while thinking is
/// streaming, or for 30 000 ms after it stopped.
pub fn is_streaming_thinking_visible(
    streaming_thinking: Option<&StreamingThinkingState>,
    now_ms: i64,
) -> bool {
    let Some(streaming_thinking) = streaming_thinking else {
        return false;
    };
    if streaming_thinking.is_streaming {
        return true;
    }
    streaming_thinking
        .streaming_ended_at_ms
        .is_some_and(|ended_at_ms| now_ms - ended_at_ms < 30_000)
}

/// Key of the thinking block that should stay expanded. `"streaming"`
/// while thinking is live, `"<uuid>:<index>"` for the newest thinking
/// block behind a user turn, `"no-thinking"` when that turn carries no
/// tool result, and `None` when past thinking is not hidden.
pub fn find_last_thinking_block_id(
    normalized_messages: &[TimelineMessage],
    hide_past_thinking: bool,
    is_streaming_thinking_visible: bool,
) -> Option<String> {
    if !hide_past_thinking {
        return None;
    }
    if is_streaming_thinking_visible {
        return Some("streaming".into());
    }

    for message in normalized_messages.iter().rev() {
        match message {
            TimelineMessage::Assistant(assistant) => {
                for (index, block) in assistant.content.iter().enumerate().rev() {
                    if matches!(block, TimelineContentBlock::Thinking) {
                        return Some(format!("{}:{index}", assistant.uuid));
                    }
                }
            }
            TimelineMessage::User(user) => {
                let has_tool_result = user
                    .content
                    .iter()
                    .any(|block| matches!(block, TimelineContentBlock::ToolResult(_)));
                if !has_tool_result {
                    return Some("no-thinking".into());
                }
            }
            TimelineMessage::Attachment(_)
            | TimelineMessage::System(_)
            | TimelineMessage::GroupedToolUse(_)
            | TimelineMessage::CollapsedReadSearch(_) => {}
        }
    }

    None
}

/// Uuid of the newest user message whose text block starts with
/// `<bash-stdout` or `<bash-stderr`.
pub fn find_latest_bash_output_uuid(normalized_messages: &[TimelineMessage]) -> Option<String> {
    for message in normalized_messages.iter().rev() {
        let TimelineMessage::User(user) = message else {
            continue;
        };
        for block in &user.content {
            if let TimelineContentBlock::Text(block) = block {
                if block.text.starts_with("<bash-stdout") || block.text.starts_with("<bash-stderr")
                {
                    return Some(user.uuid.clone());
                }
            }
        }
    }
    None
}

/// Ids of the tool-use blocks that open each assistant message in the
/// normalized timeline.
pub fn collect_normalized_tool_use_ids(
    normalized_messages: &[TimelineMessage],
) -> BTreeSet<String> {
    normalized_messages
        .iter()
        .filter_map(|message| match message {
            TimelineMessage::Assistant(assistant) => match assistant.content.first() {
                Some(TimelineContentBlock::ToolUse(block)) => Some(block.id.clone()),
                _ => None,
            },
            TimelineMessage::User(_)
            | TimelineMessage::Attachment(_)
            | TimelineMessage::System(_)
            | TimelineMessage::GroupedToolUse(_)
            | TimelineMessage::CollapsedReadSearch(_) => None,
        })
        .collect()
}

/// Streaming tool-use ids that are neither in progress nor already
/// present in the normalized timeline.
pub fn filter_new_streaming_tool_use_ids(
    streaming_tool_use_ids: &[String],
    in_progress_tool_use_ids: &BTreeSet<String>,
    normalized_tool_use_ids: &BTreeSet<String>,
) -> Vec<String> {
    streaming_tool_use_ids
        .iter()
        .filter(|tool_use_id| {
            !in_progress_tool_use_ids.contains(tool_use_id.as_str())
                && !normalized_tool_use_ids.contains(tool_use_id.as_str())
        })
        .cloned()
        .collect()
}

/// Index of the first message whose uuid matches the first-unseen uuid on
/// its leading 24 characters — where the "new messages" divider goes.
pub fn find_divider_before_index(
    renderable_messages: &[TimelineMessage],
    first_unseen_uuid: Option<&str>,
) -> Option<usize> {
    let prefix = first_unseen_uuid?.chars().take(24).collect::<String>();
    renderable_messages
        .iter()
        .position(|message| message.uuid().chars().take(24).collect::<String>() == prefix)
}

/// Index of the message whose uuid equals the cursor uuid.
pub fn find_selected_index(
    renderable_messages: &[TimelineMessage],
    cursor_uuid: Option<&str>,
) -> Option<usize> {
    let cursor_uuid = cursor_uuid?;
    renderable_messages
        .iter()
        .position(|message| message.uuid() == cursor_uuid)
}

/// Key remembering whether a row is expanded: for assistant and user
/// messages the tool-use id when there is one and the uuid otherwise;
/// every other variant uses its uuid.
pub fn expand_key(message: &TimelineMessage) -> String {
    match message {
        TimelineMessage::Assistant(_) | TimelineMessage::User(_) => {
            message.tool_use_id().unwrap_or(message.uuid()).to_owned()
        }
        TimelineMessage::Attachment(_)
        | TimelineMessage::System(_)
        | TimelineMessage::GroupedToolUse(_)
        | TimelineMessage::CollapsedReadSearch(_) => message.uuid().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{
        AssistantTimelineMessage, CollapsedReadSearchTimelineMessage, TextBlock, ToolResultBlock,
        ToolUseBlock, UserTimelineMessage,
    };

    fn assistant(uuid: &str, blocks: Vec<TimelineContentBlock>) -> TimelineMessage {
        TimelineMessage::Assistant(AssistantTimelineMessage {
            uuid: uuid.into(),
            is_api_error_message: false,
            timestamp: None,
            model: None,
            content: blocks,
        })
    }

    fn user(uuid: &str, blocks: Vec<TimelineContentBlock>) -> TimelineMessage {
        TimelineMessage::User(UserTimelineMessage {
            uuid: uuid.into(),
            is_meta: false,
            source_tool_use_id: None,
            content: blocks,
        })
    }

    #[test]
    fn streaming_thinking_visibility_matches_streaming_and_thirty_second_timeout() {
        let active = StreamingThinkingState {
            is_streaming: true,
            streaming_ended_at_ms: None,
        };
        assert!(is_streaming_thinking_visible(Some(&active), 1_000));

        let recent = StreamingThinkingState {
            is_streaming: false,
            streaming_ended_at_ms: Some(80_000),
        };
        assert!(is_streaming_thinking_visible(Some(&recent), 100_000));
        assert!(!is_streaming_thinking_visible(Some(&recent), 111_000));
        assert!(!is_streaming_thinking_visible(None, 100_000));
    }

    #[test]
    fn last_thinking_block_id_uses_streaming_and_user_turn_boundaries() {
        let messages = vec![
            assistant(
                "a-1",
                vec![
                    TimelineContentBlock::Text(TextBlock { text: "hi".into() }),
                    TimelineContentBlock::Thinking,
                ],
            ),
            user(
                "u-1",
                vec![TimelineContentBlock::Text(TextBlock {
                    text: "next".into(),
                })],
            ),
        ];
        assert_eq!(
            find_last_thinking_block_id(&messages, true, true),
            Some("streaming".into())
        );
        assert_eq!(
            find_last_thinking_block_id(&messages, true, false),
            Some("no-thinking".into())
        );

        let tool_result_boundary = vec![
            assistant(
                "a-2",
                vec![
                    TimelineContentBlock::Text(TextBlock { text: "hi".into() }),
                    TimelineContentBlock::Thinking,
                ],
            ),
            user(
                "u-2",
                vec![TimelineContentBlock::ToolResult(ToolResultBlock {
                    tool_use_id: Some("toolu-1".into()),
                })],
            ),
        ];
        assert_eq!(
            find_last_thinking_block_id(&tool_result_boundary, true, false),
            Some("a-2:1".into())
        );
    }

    #[test]
    fn latest_bash_output_uuid_finds_last_stdout_or_stderr_text_block() {
        let messages = vec![
            user(
                "u-1",
                vec![TimelineContentBlock::Text(TextBlock {
                    text: "<bash-stdout>one</bash-stdout>".into(),
                })],
            ),
            user(
                "u-2",
                vec![TimelineContentBlock::Text(TextBlock {
                    text: "plain".into(),
                })],
            ),
            user(
                "u-3",
                vec![TimelineContentBlock::Text(TextBlock {
                    text: "<bash-stderr>two</bash-stderr>".into(),
                })],
            ),
        ];
        assert_eq!(find_latest_bash_output_uuid(&messages), Some("u-3".into()));
    }

    #[test]
    fn collect_and_filter_streaming_tool_use_ids_filters_known_and_in_progress() {
        let normalized_messages = vec![
            assistant(
                "a-1",
                vec![TimelineContentBlock::ToolUse(ToolUseBlock {
                    id: "toolu-1".into(),
                    name: Some("Read".into()),
                    is_collapsible: false,
                })],
            ),
            assistant(
                "a-2",
                vec![TimelineContentBlock::Text(TextBlock {
                    text: "hello".into(),
                })],
            ),
        ];
        let normalized_ids = collect_normalized_tool_use_ids(&normalized_messages);
        assert_eq!(
            normalized_ids
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["toolu-1"]
        );

        let mut in_progress = BTreeSet::new();
        in_progress.insert("toolu-2".into());
        let filtered = filter_new_streaming_tool_use_ids(
            &["toolu-1".into(), "toolu-2".into(), "toolu-3".into()],
            &in_progress,
            &normalized_ids,
        );
        assert_eq!(filtered, vec!["toolu-3"]);
    }

    #[test]
    fn divider_selected_and_expand_key_resolve_expected_values() {
        let grouped = TimelineMessage::CollapsedReadSearch(CollapsedReadSearchTimelineMessage {
            uuid: "123456789012345678901234-tail".into(),
            tool_use_ids: vec!["toolu-9".into()],
        });
        let user_tool_result = TimelineMessage::User(UserTimelineMessage {
            uuid: "u-1".into(),
            is_meta: false,
            source_tool_use_id: None,
            content: vec![TimelineContentBlock::ToolResult(ToolResultBlock {
                tool_use_id: Some("toolu-1".into()),
            })],
        });
        let messages = vec![grouped.clone(), user_tool_result.clone()];

        assert_eq!(
            find_divider_before_index(&messages, Some("123456789012345678901234-rest")),
            Some(0)
        );
        assert_eq!(find_selected_index(&messages, Some("u-1")), Some(1));
        assert_eq!(expand_key(&grouped), "123456789012345678901234-tail");
        assert_eq!(expand_key(&user_tool_result), "toolu-1");

        let assistant_tool_use = assistant(
            "a-1",
            vec![TimelineContentBlock::ToolUse(ToolUseBlock {
                id: "toolu-2".into(),
                name: Some("Bash".into()),
                is_collapsible: false,
            })],
        );
        assert_eq!(expand_key(&assistant_tool_use), "toolu-2");
    }
}
