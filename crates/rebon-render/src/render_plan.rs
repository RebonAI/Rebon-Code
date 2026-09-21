//! Per-row decisions the transcript render loop makes before drawing a row.

use crate::{has_content_after_index, TimelineMessage};

/// Input bag for [`project_render_message_row`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderMessageRowInput {
    /// Every row the transcript renders.
    pub renderable_messages: Vec<TimelineMessage>,
    /// Current row index.
    pub index: usize,
    /// Whether assistant text is currently streaming.
    pub has_streaming_text: bool,
    /// Whether the row is currently expanded by click-to-expand.
    pub is_item_expanded: bool,
    /// Global `verbose`
    pub verbose: bool,
    /// Whether the cursor points at this row and the row is expanded.
    pub cursor_expanded_and_selected: bool,
    /// Precomputed unseen-divider insertion index.
    pub divider_before_index: Option<usize>,
    /// Conversation id, part of each row's key.
    pub conversation_id: String,
    /// Flattened streaming tool-use IDs.
    pub streaming_tool_use_ids: std::collections::BTreeSet<String>,
}

/// The per-row decisions [`project_render_message_row`] makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderMessageRowPlan {
    /// Row key: `<message uuid>-<conversation id>`.
    pub key: String,
    /// Set when this message and the previous one are both user messages.
    pub is_user_continuation: bool,
    /// Collapsed-read-search scan result.
    pub has_content_after: bool,
    /// Row-local verbose flag.
    pub verbose: bool,
    /// Whether to insert the unseen divider before this row.
    pub insert_unseen_divider_before: bool,
}

/// Assemble one row's key, user-continuation, content-after, verbose and
/// unseen-divider flags.
pub fn project_render_message_row(input: &RenderMessageRowInput) -> RenderMessageRowPlan {
    let message = input
        .renderable_messages
        .get(input.index)
        .expect("render row index must be in bounds");
    let prev_type = input
        .index
        .checked_sub(1)
        .and_then(|index| input.renderable_messages.get(index))
        .map(|message| match message {
            TimelineMessage::User(_) => "user",
            TimelineMessage::Assistant(_) => "assistant",
            TimelineMessage::Attachment(_) => "attachment",
            TimelineMessage::System(_) => "system",
            TimelineMessage::GroupedToolUse(_) => "grouped_tool_use",
            TimelineMessage::CollapsedReadSearch(_) => "collapsed_read_search",
        });
    let is_user_continuation =
        matches!(message, TimelineMessage::User(_)) && prev_type == Some("user");
    let has_content_after = matches!(message, TimelineMessage::CollapsedReadSearch(_))
        && (input.has_streaming_text
            || has_content_after_index(
                &input.renderable_messages,
                input.index,
                &input.streaming_tool_use_ids,
            ));
    let verbose = input.verbose || input.is_item_expanded || input.cursor_expanded_and_selected;

    RenderMessageRowPlan {
        key: format!("{}-{}", message.uuid(), input.conversation_id),
        is_user_continuation,
        has_content_after,
        verbose,
        insert_unseen_divider_before: input.divider_before_index == Some(input.index),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::timeline::{
        AssistantTimelineMessage, CollapsedReadSearchTimelineMessage, TextBlock,
        TimelineContentBlock, UserTimelineMessage,
    };

    fn user(uuid: &str, text: &str) -> TimelineMessage {
        TimelineMessage::User(UserTimelineMessage {
            uuid: uuid.into(),
            is_meta: false,
            source_tool_use_id: None,
            content: vec![TimelineContentBlock::Text(TextBlock { text: text.into() })],
        })
    }

    fn assistant(uuid: &str, text: &str) -> TimelineMessage {
        TimelineMessage::Assistant(AssistantTimelineMessage {
            uuid: uuid.into(),
            is_api_error_message: false,
            timestamp: None,
            model: None,
            content: vec![TimelineContentBlock::Text(TextBlock { text: text.into() })],
        })
    }

    #[test]
    fn render_row_plan_builds_key_and_user_continuation_flag() {
        let input = RenderMessageRowInput {
            renderable_messages: vec![user("u-1", "a"), user("u-2", "b")],
            index: 1,
            has_streaming_text: false,
            is_item_expanded: false,
            verbose: false,
            cursor_expanded_and_selected: false,
            divider_before_index: None,
            conversation_id: "conv-1".into(),
            streaming_tool_use_ids: BTreeSet::new(),
        };
        let plan = project_render_message_row(&input);
        assert_eq!(plan.key, "u-2-conv-1");
        assert!(plan.is_user_continuation);
        assert!(!plan.verbose);
    }

    #[test]
    fn render_row_plan_uses_streaming_text_for_collapsed_group_and_verbose_sources() {
        let input = RenderMessageRowInput {
            renderable_messages: vec![TimelineMessage::CollapsedReadSearch(
                CollapsedReadSearchTimelineMessage {
                    uuid: "c-1".into(),
                    tool_use_ids: vec!["toolu-1".into()],
                },
            )],
            index: 0,
            has_streaming_text: true,
            is_item_expanded: true,
            verbose: false,
            cursor_expanded_and_selected: false,
            divider_before_index: Some(0),
            conversation_id: "conv-1".into(),
            streaming_tool_use_ids: BTreeSet::new(),
        };
        let plan = project_render_message_row(&input);
        assert!(plan.has_content_after);
        assert!(plan.verbose);
        assert!(plan.insert_unseen_divider_before);
    }

    #[test]
    fn render_row_plan_allows_cursor_expansion_to_force_verbose() {
        let input = RenderMessageRowInput {
            renderable_messages: vec![assistant("a-1", "hello")],
            index: 0,
            has_streaming_text: false,
            is_item_expanded: false,
            verbose: false,
            cursor_expanded_and_selected: true,
            divider_before_index: Some(1),
            conversation_id: "conv-2".into(),
            streaming_tool_use_ids: BTreeSet::new(),
        };
        let plan = project_render_message_row(&input);
        assert!(plan.verbose);
        assert!(!plan.insert_unseen_divider_before);
        assert!(!plan.has_content_after);
    }
}
