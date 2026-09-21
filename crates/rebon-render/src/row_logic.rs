//! Pure per-row timeline predicates: the thinking-content gate, whether
//! content follows a row, streaming and resolution checks, the
//! static-render decision, and the row memo comparator.

use std::collections::BTreeSet;

use crate::timeline::{
    AssistantTimelineMessage, GroupedToolUseTimelineMessage, TimelineContentBlock, TimelineLookups,
    TimelineMessage, TimelineScreen, TimelineSystemSubtype, ToolUseBlock,
};

/// Minimal set of inputs for the [`MessageRow`](crate::MessageRow) memo comparator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRowMemoInput {
    /// Identity token for the message: a change in it is on its own enough
    /// to report the row as changed.
    pub message_identity: usize,
    /// Current message row.
    pub message: TimelineMessage,
    /// `screen`
    pub screen: TimelineScreen,
    /// `verbose`
    pub verbose: bool,
    /// `columns`
    pub columns: usize,
    /// Key of the thinking block that should stay expanded.
    pub last_thinking_block_id: Option<String>,
    /// Uuid of the newest user message carrying bash stdout/stderr text.
    pub latest_bash_output_uuid: Option<String>,
    /// Tool-use ids whose output is still streaming.
    pub streaming_tool_use_ids: BTreeSet<String>,
    /// `lookups`
    pub lookups: TimelineLookups,
}

/// True when an assistant message holds a thinking or redacted-thinking
/// block.
pub fn has_timeline_thinking_content(message: &TimelineMessage) -> bool {
    let TimelineMessage::Assistant(assistant) = message else {
        return false;
    };
    assistant.content.iter().any(|block| {
        matches!(
            block,
            TimelineContentBlock::Thinking | TimelineContentBlock::RedactedThinking
        )
    })
}

/// True when something after `index` still has to be drawn. Walks forward
/// skipping thinking blocks, collapsible tool uses, streaming tool uses,
/// system and attachment rows, user rows that open with a tool result, and
/// fully collapsible grouped tool uses; any other row counts as content.
pub fn has_content_after_index(
    messages: &[TimelineMessage],
    index: usize,
    streaming_tool_use_ids: &BTreeSet<String>,
) -> bool {
    for message in messages.iter().skip(index + 1) {
        match message {
            TimelineMessage::Assistant(assistant) => {
                let Some(block) = assistant.content.first() else {
                    return true;
                };
                match block {
                    TimelineContentBlock::Thinking | TimelineContentBlock::RedactedThinking => {
                        continue;
                    }
                    TimelineContentBlock::ToolUse(block) if block.is_collapsible => continue,
                    TimelineContentBlock::ToolUse(block)
                        if streaming_tool_use_ids.contains(block.id.as_str()) =>
                    {
                        continue;
                    }
                    _ => return true,
                }
            }
            TimelineMessage::System(_) | TimelineMessage::Attachment(_) => continue,
            TimelineMessage::User(user) => match user.content.first() {
                Some(TimelineContentBlock::ToolResult(_)) => continue,
                _ => return true,
            },
            TimelineMessage::GroupedToolUse(group) => {
                if grouped_tool_use_is_collapsible(group) {
                    continue;
                }
                return true;
            }
            TimelineMessage::CollapsedReadSearch(_) => return true,
        }
    }
    false
}

/// True when the message carries a tool-use id that is still streaming,
/// including inside grouped and collapsed read/search rows.
pub fn is_message_streaming(
    message: &TimelineMessage,
    streaming_tool_use_ids: &BTreeSet<String>,
) -> bool {
    match message {
        TimelineMessage::GroupedToolUse(group) => group.messages.iter().any(|message| {
            matches!(
                message.content.first(),
                Some(TimelineContentBlock::ToolUse(block))
                    if streaming_tool_use_ids.contains(block.id.as_str())
            )
        }),
        TimelineMessage::CollapsedReadSearch(group) => group
            .tool_use_ids
            .iter()
            .any(|tool_use_id| streaming_tool_use_ids.contains(tool_use_id.as_str())),
        _ => message
            .tool_use_id()
            .is_some_and(|tool_use_id| streaming_tool_use_ids.contains(tool_use_id)),
    }
}

/// True when every tool-use id on the message is resolved: grouped and
/// collapsed read/search rows check all of their ids, a server tool use
/// checks its own id, and a message without a tool-use id counts as
/// resolved.
pub fn all_tools_resolved(
    message: &TimelineMessage,
    resolved_tool_use_ids: &BTreeSet<String>,
) -> bool {
    match message {
        TimelineMessage::GroupedToolUse(group) => group.messages.iter().all(|message| {
            matches!(
                message.content.first(),
                Some(TimelineContentBlock::ToolUse(block))
                    if resolved_tool_use_ids.contains(block.id.as_str())
            )
        }),
        TimelineMessage::CollapsedReadSearch(group) => group
            .tool_use_ids
            .iter()
            .all(|tool_use_id| resolved_tool_use_ids.contains(tool_use_id.as_str())),
        TimelineMessage::Assistant(assistant) => match assistant.content.first() {
            Some(TimelineContentBlock::ServerToolUse(block)) => {
                resolved_tool_use_ids.contains(block.id.as_str())
            }
            _ => message.tool_use_id().map_or(true, |tool_use_id| {
                resolved_tool_use_ids.contains(tool_use_id)
            }),
        },
        _ => message.tool_use_id().map_or(true, |tool_use_id| {
            resolved_tool_use_ids.contains(tool_use_id)
        }),
    }
}

/// True when the row can be painted once and left alone. The transcript
/// screen always qualifies. Assistant, user, and attachment rows qualify
/// when a leading server tool use is resolved, or when the message has no
/// tool-use id or every sibling tool-use id is resolved; they fail if their
/// own id is streaming, in progress, or still awaiting a post-tool-use
/// result. System rows qualify except for API errors, grouped tool uses
/// qualify when every member is resolved, and collapsed read/search rows
/// never qualify.
pub fn should_render_statically(
    message: &TimelineMessage,
    streaming_tool_use_ids: &BTreeSet<String>,
    in_progress_tool_use_ids: &BTreeSet<String>,
    sibling_tool_use_ids: &BTreeSet<String>,
    screen: TimelineScreen,
    lookups: &TimelineLookups,
) -> bool {
    if matches!(screen, TimelineScreen::Transcript) {
        return true;
    }

    match message {
        TimelineMessage::Attachment(_)
        | TimelineMessage::User(_)
        | TimelineMessage::Assistant(_) => {
            if let TimelineMessage::Assistant(assistant) = message {
                if let Some(TimelineContentBlock::ServerToolUse(block)) = assistant.content.first()
                {
                    return lookups.resolved_tool_use_ids.contains(block.id.as_str());
                }
            }

            let Some(tool_use_id) = message.tool_use_id() else {
                return true;
            };

            if streaming_tool_use_ids.contains(tool_use_id)
                || in_progress_tool_use_ids.contains(tool_use_id)
                || lookups.unresolved_post_tool_use_ids.contains(tool_use_id)
            {
                return false;
            }

            sibling_tool_use_ids
                .iter()
                .all(|tool_use_id| lookups.resolved_tool_use_ids.contains(tool_use_id))
        }
        TimelineMessage::System(system) => {
            !matches!(system.subtype, TimelineSystemSubtype::ApiError)
        }
        TimelineMessage::GroupedToolUse(group) => group.messages.iter().all(|message| {
            matches!(
                message.content.first(),
                Some(TimelineContentBlock::ToolUse(block))
                    if lookups.resolved_tool_use_ids.contains(block.id.as_str())
            )
        }),
        TimelineMessage::CollapsedReadSearch(_) => false,
    }
}

/// Memo comparison for a message row. Differing identity, screen, verbosity
/// or column count report a change; so does a flipped latest-bash flag, a
/// changed thinking-block key when the next message has thinking content, a
/// collapsed read/search row whose next screen is not the transcript, and
/// any row that is still streaming or has unresolved tools.
pub fn are_message_row_memo_inputs_equal(
    prev: &MessageRowMemoInput,
    next: &MessageRowMemoInput,
) -> bool {
    if prev.message_identity != next.message_identity {
        return false;
    }
    if prev.screen != next.screen {
        return false;
    }
    if prev.verbose != next.verbose {
        return false;
    }
    if matches!(prev.message, TimelineMessage::CollapsedReadSearch(_))
        && !matches!(next.screen, TimelineScreen::Transcript)
    {
        return false;
    }
    if prev.columns != next.columns {
        return false;
    }

    let prev_is_latest_bash = prev.latest_bash_output_uuid.as_deref() == Some(prev.message.uuid());
    let next_is_latest_bash = next.latest_bash_output_uuid.as_deref() == Some(next.message.uuid());
    if prev_is_latest_bash != next_is_latest_bash {
        return false;
    }

    if prev.last_thinking_block_id != next.last_thinking_block_id
        && has_timeline_thinking_content(&next.message)
    {
        return false;
    }

    let is_streaming = is_message_streaming(&prev.message, &prev.streaming_tool_use_ids);
    let is_resolved = all_tools_resolved(&prev.message, &prev.lookups.resolved_tool_use_ids);
    if is_streaming || !is_resolved {
        return false;
    }

    true
}

fn grouped_tool_use_is_collapsible(message: &GroupedToolUseTimelineMessage) -> bool {
    message
        .messages
        .first()
        .and_then(first_tool_use_block)
        .is_some_and(|block| block.is_collapsible)
}

fn first_tool_use_block(message: &AssistantTimelineMessage) -> Option<&ToolUseBlock> {
    match message.content.first() {
        Some(TimelineContentBlock::ToolUse(block)) => Some(block),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{
        AssistantTimelineMessage, AttachmentTimelineMessage, CollapsedReadSearchTimelineMessage,
        GroupedToolUseTimelineMessage, ServerToolUseBlock, SystemTimelineMessage, TextBlock,
        TimelineLookups, TimelineMessage, TimelineSystemSubtype, ToolResultBlock,
        UserTimelineMessage,
    };

    fn assistant_with_block(uuid: &str, block: TimelineContentBlock) -> TimelineMessage {
        TimelineMessage::Assistant(AssistantTimelineMessage {
            uuid: uuid.into(),
            is_api_error_message: false,
            timestamp: None,
            model: None,
            content: vec![block],
        })
    }

    fn user_with_block(uuid: &str, block: TimelineContentBlock) -> TimelineMessage {
        TimelineMessage::User(UserTimelineMessage {
            uuid: uuid.into(),
            is_meta: false,
            source_tool_use_id: None,
            content: vec![block],
        })
    }

    fn grouped(ids: &[(&str, bool)]) -> TimelineMessage {
        TimelineMessage::GroupedToolUse(GroupedToolUseTimelineMessage {
            uuid: "g-1".into(),
            tool_name: "Read".into(),
            messages: ids
                .iter()
                .map(|(id, is_collapsible)| AssistantTimelineMessage {
                    uuid: format!("a-{id}"),
                    is_api_error_message: false,
                    timestamp: None,
                    model: None,
                    content: vec![TimelineContentBlock::ToolUse(ToolUseBlock {
                        id: (*id).into(),
                        name: Some("Read".into()),
                        is_collapsible: *is_collapsible,
                    })],
                })
                .collect(),
        })
    }

    #[test]
    fn has_content_after_index_skips_transient_items_but_stops_on_real_content() {
        let messages = vec![
            assistant_with_block(
                "a-0",
                TimelineContentBlock::Text(TextBlock {
                    text: "seed".into(),
                }),
            ),
            assistant_with_block("a-1", TimelineContentBlock::Thinking),
            assistant_with_block(
                "a-2",
                TimelineContentBlock::ToolUse(ToolUseBlock {
                    id: "toolu-1".into(),
                    name: Some("Read".into()),
                    is_collapsible: true,
                }),
            ),
            user_with_block(
                "u-1",
                TimelineContentBlock::ToolResult(ToolResultBlock {
                    tool_use_id: Some("toolu-1".into()),
                }),
            ),
            TimelineMessage::Attachment(AttachmentTimelineMessage {
                uuid: "att-1".into(),
                attachment_type: "hook_success".into(),
                attachment_is_meta: false,
                command_mode: None,
                has_origin: false,
                tool_use_id: None,
            }),
            grouped(&[("toolu-2", true)]),
            assistant_with_block(
                "a-3",
                TimelineContentBlock::Text(TextBlock {
                    text: "real".into(),
                }),
            ),
        ];
        assert!(has_content_after_index(&messages, 0, &BTreeSet::new()));
        assert!(!has_content_after_index(
            &messages[..6],
            0,
            &BTreeSet::new()
        ));
    }

    #[test]
    fn is_message_streaming_and_all_tools_resolved_match_grouped_collapsed_and_server_cases() {
        let mut streaming = BTreeSet::new();
        streaming.insert("toolu-1".into());
        let mut resolved = BTreeSet::new();
        resolved.insert("toolu-2".into());
        resolved.insert("srv-1".into());

        let grouped_message = grouped(&[("toolu-1", false)]);
        assert!(is_message_streaming(&grouped_message, &streaming));
        assert!(!all_tools_resolved(&grouped_message, &resolved));

        let collapsed = TimelineMessage::CollapsedReadSearch(CollapsedReadSearchTimelineMessage {
            uuid: "c-1".into(),
            tool_use_ids: vec!["toolu-2".into()],
        });
        assert!(!is_message_streaming(&collapsed, &streaming));
        assert!(all_tools_resolved(&collapsed, &resolved));

        let server = assistant_with_block(
            "a-4",
            TimelineContentBlock::ServerToolUse(ServerToolUseBlock { id: "srv-1".into() }),
        );
        assert!(all_tools_resolved(&server, &resolved));
    }

    #[test]
    fn should_render_statically_respects_prompt_runtime_gates() {
        let message = assistant_with_block(
            "a-1",
            TimelineContentBlock::ToolUse(ToolUseBlock {
                id: "toolu-1".into(),
                name: Some("Bash".into()),
                is_collapsible: false,
            }),
        );
        let mut sibling_ids = BTreeSet::new();
        sibling_ids.insert("toolu-1".into());

        let mut lookups = TimelineLookups::default();
        lookups.resolved_tool_use_ids.insert("toolu-1".into());
        assert!(should_render_statically(
            &message,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &sibling_ids,
            TimelineScreen::Prompt,
            &lookups,
        ));

        let mut in_progress = BTreeSet::new();
        in_progress.insert("toolu-1".into());
        assert!(!should_render_statically(
            &message,
            &BTreeSet::new(),
            &in_progress,
            &sibling_ids,
            TimelineScreen::Prompt,
            &lookups,
        ));

        lookups
            .unresolved_post_tool_use_ids
            .insert("toolu-1".into());
        assert!(!should_render_statically(
            &message,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &sibling_ids,
            TimelineScreen::Prompt,
            &lookups,
        ));
    }

    #[test]
    fn should_render_statically_handles_system_grouped_and_collapsed_rows() {
        let system = TimelineMessage::System(SystemTimelineMessage {
            uuid: "s-1".into(),
            subtype: TimelineSystemSubtype::ApiError,
            tool_use_id: None,
        });
        assert!(!should_render_statically(
            &system,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            TimelineScreen::Prompt,
            &TimelineLookups::default(),
        ));

        let grouped_message = grouped(&[("toolu-1", false), ("toolu-2", false)]);
        let mut lookups = TimelineLookups::default();
        lookups.resolved_tool_use_ids.insert("toolu-1".into());
        lookups.resolved_tool_use_ids.insert("toolu-2".into());
        assert!(should_render_statically(
            &grouped_message,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            TimelineScreen::Prompt,
            &lookups,
        ));

        let collapsed = TimelineMessage::CollapsedReadSearch(CollapsedReadSearchTimelineMessage {
            uuid: "c-1".into(),
            tool_use_ids: vec!["toolu-1".into()],
        });
        assert!(!should_render_statically(
            &collapsed,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            TimelineScreen::Prompt,
            &lookups,
        ));
        assert!(should_render_statically(
            &collapsed,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            TimelineScreen::Transcript,
            &lookups,
        ));
    }

    #[test]
    fn are_message_row_props_equal_only_bails_for_static_and_stable_rows() {
        let mut resolved = TimelineLookups::default();
        resolved.resolved_tool_use_ids.insert("toolu-1".into());
        let base = MessageRowMemoInput {
            message_identity: 1,
            message: assistant_with_block(
                "a-1",
                TimelineContentBlock::ToolUse(ToolUseBlock {
                    id: "toolu-1".into(),
                    name: Some("Bash".into()),
                    is_collapsible: false,
                }),
            ),
            screen: TimelineScreen::Prompt,
            verbose: false,
            columns: 120,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            streaming_tool_use_ids: BTreeSet::new(),
            lookups: resolved,
        };
        let same = base.clone();
        assert!(are_message_row_memo_inputs_equal(&base, &same));

        let mut changed = same.clone();
        changed.message_identity = 2;
        assert!(!are_message_row_memo_inputs_equal(&base, &changed));

        let thinking_message = assistant_with_block("a-2", TimelineContentBlock::Thinking);
        let thinking_prev = MessageRowMemoInput {
            message_identity: 1,
            message: thinking_message.clone(),
            screen: TimelineScreen::Prompt,
            verbose: false,
            columns: 120,
            last_thinking_block_id: None,
            latest_bash_output_uuid: None,
            streaming_tool_use_ids: BTreeSet::new(),
            lookups: TimelineLookups::default(),
        };
        let mut thinking_next = thinking_prev.clone();
        thinking_next.last_thinking_block_id = Some("a-2:0".into());
        assert!(!are_message_row_memo_inputs_equal(
            &thinking_prev,
            &thinking_next
        ));
    }
}
