//! Brief-mode timeline filters: keeping only the rows a brief turn needs,
//! and dropping prose from turns that already ran a brief tool.

use std::collections::BTreeSet;

use crate::timeline::{TimelineContentBlock, TimelineMessage, TimelineSystemSubtype};

/// Keeps the rows a brief turn needs: system rows other than API metrics,
/// assistant rows that are API errors or that open a tool use named in
/// `brief_tool_names`, the tool results answering those calls, non-meta user
/// rows, and `queued_command` attachments in `prompt` mode that are neither
/// meta nor originated by a tool. Grouped tool uses and collapsed read/search
/// rows are always dropped.
pub fn filter_for_brief_tool(
    messages: &[TimelineMessage],
    brief_tool_names: &[String],
) -> Vec<TimelineMessage> {
    let name_set: BTreeSet<&str> = brief_tool_names.iter().map(String::as_str).collect();
    let mut brief_tool_use_ids = BTreeSet::new();

    messages
        .iter()
        .filter_map(|message| {
            let keep = match message {
                TimelineMessage::System(system) => {
                    !matches!(system.subtype, TimelineSystemSubtype::ApiMetrics)
                }
                TimelineMessage::Assistant(assistant) => {
                    if assistant.is_api_error_message {
                        true
                    } else if let Some(TimelineContentBlock::ToolUse(block)) =
                        assistant.content.first()
                    {
                        if let Some(name) = block.name.as_deref() {
                            if name_set.contains(name) {
                                brief_tool_use_ids.insert(block.id.clone());
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
                TimelineMessage::User(user) => match user.content.first() {
                    Some(TimelineContentBlock::ToolResult(block)) => block
                        .tool_use_id
                        .as_deref()
                        .is_some_and(|tool_use_id| brief_tool_use_ids.contains(tool_use_id)),
                    _ => !user.is_meta,
                },
                TimelineMessage::Attachment(attachment) => {
                    attachment.attachment_type == "queued_command"
                        && attachment.command_mode.as_deref() == Some("prompt")
                        && !attachment.attachment_is_meta
                        && !attachment.has_origin
                }
                TimelineMessage::GroupedToolUse(_) | TimelineMessage::CollapsedReadSearch(_) => {
                    false
                }
            };

            keep.then(|| message.clone())
        })
        .collect()
}

/// Drops assistant text rows belonging to a turn that ran one of
/// `brief_tool_names`. A new turn starts at each non-meta user message that
/// does not open with a tool result; when no turn ran a brief tool the input
/// is returned unchanged.
pub fn drop_text_in_brief_turns(
    messages: &[TimelineMessage],
    brief_tool_names: &[String],
) -> Vec<TimelineMessage> {
    let name_set: BTreeSet<&str> = brief_tool_names.iter().map(String::as_str).collect();
    let mut turns_with_brief = BTreeSet::new();
    let mut text_index_to_turn = vec![None; messages.len()];
    let mut turn = 0usize;

    for (index, message) in messages.iter().enumerate() {
        match message {
            TimelineMessage::User(user) => {
                let first_block = user.content.first();
                if !matches!(first_block, Some(TimelineContentBlock::ToolResult(_)))
                    && !user.is_meta
                {
                    turn += 1;
                    continue;
                }
            }
            TimelineMessage::Assistant(assistant) => match assistant.content.first() {
                Some(TimelineContentBlock::Text(_)) => text_index_to_turn[index] = Some(turn),
                Some(TimelineContentBlock::ToolUse(block)) => {
                    if block
                        .name
                        .as_deref()
                        .is_some_and(|name| name_set.contains(name))
                    {
                        turns_with_brief.insert(turn);
                    }
                }
                _ => {}
            },
            TimelineMessage::Attachment(_)
            | TimelineMessage::System(_)
            | TimelineMessage::GroupedToolUse(_)
            | TimelineMessage::CollapsedReadSearch(_) => {}
        }
    }

    if turns_with_brief.is_empty() {
        return messages.to_vec();
    }

    messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let keep = match text_index_to_turn[index] {
                Some(turn) => !turns_with_brief.contains(&turn),
                None => true,
            };
            keep.then(|| message.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{
        AssistantTimelineMessage, AttachmentTimelineMessage, SystemTimelineMessage, TextBlock,
        TimelineSystemSubtype, ToolResultBlock, ToolUseBlock, UserTimelineMessage,
    };

    fn brief_names() -> Vec<String> {
        vec!["Brief".into()]
    }

    fn assistant_tool_use(id: &str, name: &str) -> TimelineMessage {
        TimelineMessage::Assistant(AssistantTimelineMessage {
            uuid: format!("a-{id}"),
            is_api_error_message: false,
            timestamp: None,
            model: None,
            content: vec![TimelineContentBlock::ToolUse(ToolUseBlock {
                id: id.into(),
                name: Some(name.into()),
                is_collapsible: false,
            })],
        })
    }

    fn assistant_text(uuid: &str) -> TimelineMessage {
        TimelineMessage::Assistant(AssistantTimelineMessage {
            uuid: uuid.into(),
            is_api_error_message: false,
            timestamp: None,
            model: None,
            content: vec![TimelineContentBlock::Text(TextBlock {
                text: "hello".into(),
            })],
        })
    }

    fn api_error_assistant(uuid: &str) -> TimelineMessage {
        TimelineMessage::Assistant(AssistantTimelineMessage {
            uuid: uuid.into(),
            is_api_error_message: true,
            timestamp: None,
            model: None,
            content: vec![TimelineContentBlock::Text(TextBlock {
                text: "rate limit".into(),
            })],
        })
    }

    fn user_tool_result(uuid: &str, tool_use_id: &str) -> TimelineMessage {
        TimelineMessage::User(UserTimelineMessage {
            uuid: uuid.into(),
            is_meta: false,
            source_tool_use_id: None,
            content: vec![TimelineContentBlock::ToolResult(ToolResultBlock {
                tool_use_id: Some(tool_use_id.into()),
            })],
        })
    }

    fn user_text(uuid: &str, is_meta: bool) -> TimelineMessage {
        TimelineMessage::User(UserTimelineMessage {
            uuid: uuid.into(),
            is_meta,
            source_tool_use_id: None,
            content: vec![TimelineContentBlock::Text(TextBlock {
                text: "user".into(),
            })],
        })
    }

    #[test]
    fn filter_for_brief_tool_keeps_matching_tool_use_results_and_real_input() {
        let messages = vec![
            assistant_text("a-0"),
            assistant_tool_use("toolu-1", "Brief"),
            user_tool_result("u-1", "toolu-1"),
            user_text("u-2", false),
            user_text("u-3", true),
        ];

        let filtered = filter_for_brief_tool(&messages, &brief_names());
        assert_eq!(
            filtered
                .iter()
                .map(TimelineMessage::uuid)
                .collect::<Vec<_>>(),
            vec!["a-toolu-1", "u-1", "u-2"]
        );
    }

    #[test]
    fn filter_for_brief_tool_keeps_api_error_and_drops_api_metrics() {
        let messages = vec![
            TimelineMessage::System(SystemTimelineMessage {
                uuid: "s-1".into(),
                subtype: TimelineSystemSubtype::ApiMetrics,
                tool_use_id: None,
            }),
            TimelineMessage::System(SystemTimelineMessage {
                uuid: "s-2".into(),
                subtype: TimelineSystemSubtype::ApiError,
                tool_use_id: None,
            }),
            api_error_assistant("a-err"),
        ];

        let filtered = filter_for_brief_tool(&messages, &brief_names());
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].uuid(), "s-2");
        assert_eq!(filtered[1].uuid(), "a-err");
    }

    #[test]
    fn filter_for_brief_tool_only_keeps_human_prompt_queued_command_attachments() {
        let keep = TimelineMessage::Attachment(AttachmentTimelineMessage {
            uuid: "att-1".into(),
            attachment_type: "queued_command".into(),
            attachment_is_meta: false,
            command_mode: Some("prompt".into()),
            has_origin: false,
            tool_use_id: None,
        });
        let drop = TimelineMessage::Attachment(AttachmentTimelineMessage {
            uuid: "att-2".into(),
            attachment_type: "queued_command".into(),
            attachment_is_meta: false,
            command_mode: Some("task-notification".into()),
            has_origin: false,
            tool_use_id: None,
        });

        let filtered = filter_for_brief_tool(&[keep.clone(), drop], &brief_names());
        assert_eq!(filtered, vec![keep]);
    }

    #[test]
    fn drop_text_in_brief_turns_only_drops_text_in_turns_that_called_brief() {
        let messages = vec![
            assistant_text("a-0"),
            assistant_tool_use("toolu-1", "Brief"),
            user_tool_result("u-1", "toolu-1"),
            user_text("u-2", false),
            assistant_text("a-1"),
            assistant_tool_use("toolu-2", "OtherTool"),
        ];

        let dropped = drop_text_in_brief_turns(&messages, &brief_names());
        assert_eq!(
            dropped
                .iter()
                .map(TimelineMessage::uuid)
                .collect::<Vec<_>>(),
            vec!["a-toolu-1", "u-1", "u-2", "a-1", "a-toolu-2"]
        );
    }
}
