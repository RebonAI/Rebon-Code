//! Change detection for the whole transcript list: the inputs that decide
//! whether the list has to be redrawn, in canonical form.

use std::collections::BTreeSet;

use crate::TimelineScreen;

/// Canonical unseen-divider state compared by [`are_messages_memo_inputs_equal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagesMemoUnseenDivider {
    /// Uuid of the first unseen message.
    pub first_unseen_uuid: String,
    /// Number of unseen messages.
    pub count: usize,
}

/// Canonical transcript-list inputs for [`are_messages_memo_inputs_equal`].
///
/// Callbacks and imperative handles are not part of the comparison and are
/// intentionally absent from this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagesMemoInput {
    /// Stable token for the message list.
    pub messages_identity: usize,
    /// Tool names only; other tool fields do not affect the comparison.
    pub tool_names: Vec<String>,
    /// Stable token for the command list.
    pub commands_identity: usize,
    /// Stable token for the UI a running tool shows in place of the prompt.
    pub tool_ui_identity: usize,
    /// Stable token for the tool-use confirmation queue.
    pub tool_use_confirm_queue_identity: usize,
    /// Tool-use ids still running, canonicalized as a set.
    pub in_progress_tool_use_ids: BTreeSet<String>,
    /// Whether the message selector is open.
    pub is_message_selector_visible: bool,
    /// Conversation id.
    pub conversation_id: String,
    /// Which screen the transcript is drawn on.
    pub screen: TimelineScreen,
    /// Streaming tool uses, canonicalized to their content-block ids.
    pub streaming_tool_use_ids: Vec<String>,
    /// Whether the transcript screen shows every message.
    pub show_all_in_transcript: bool,
    /// Stable token for the agent definitions.
    pub agent_definitions_identity: usize,
    /// Global verbose flag.
    pub verbose: bool,
    /// Whether the logo header is hidden.
    pub hide_logo: bool,
    /// Whether a turn is in flight.
    pub is_loading: bool,
    /// Whether thinking from earlier turns is hidden.
    pub hide_past_thinking: bool,
    /// Stable token for the streaming thinking block.
    pub streaming_thinking_identity: usize,
    /// Assistant text currently streaming, if any.
    pub streaming_text: Option<String>,
    /// Whether the brief-only view is on.
    pub is_brief_only: bool,
    /// Canonical unseen-divider payload.
    pub unseen_divider: Option<MessagesMemoUnseenDivider>,
    /// Whether the cap on rendered rows is lifted.
    pub disable_render_cap: bool,
    /// Stable token for the selection cursor.
    pub cursor_identity: usize,
    /// Row range being rendered, if restricted.
    pub render_range: Option<(usize, usize)>,
}

/// Whether the transcript list inputs are unchanged.
///
/// The struct already reduces tools to their names, streaming tool uses to
/// their content-block ids, in-progress ids to a set and the unseen divider
/// to uuid + count, so plain value equality is the comparison.
pub fn are_messages_memo_inputs_equal(prev: &MessagesMemoInput, next: &MessagesMemoInput) -> bool {
    prev == next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> MessagesMemoInput {
        MessagesMemoInput {
            messages_identity: 1,
            tool_names: vec!["Bash".into(), "Read".into()],
            commands_identity: 1,
            tool_ui_identity: 1,
            tool_use_confirm_queue_identity: 1,
            in_progress_tool_use_ids: BTreeSet::from(["toolu-1".into()]),
            is_message_selector_visible: false,
            conversation_id: "conv-1".into(),
            screen: TimelineScreen::Prompt,
            streaming_tool_use_ids: vec!["toolu-2".into()],
            show_all_in_transcript: false,
            agent_definitions_identity: 1,
            verbose: false,
            hide_logo: false,
            is_loading: false,
            hide_past_thinking: false,
            streaming_thinking_identity: 1,
            streaming_text: None,
            is_brief_only: false,
            unseen_divider: Some(MessagesMemoUnseenDivider {
                first_unseen_uuid: "u-1".into(),
                count: 3,
            }),
            disable_render_cap: false,
            cursor_identity: 1,
            render_range: Some((0, 20)),
        }
    }

    #[test]
    fn messages_memo_props_equal_when_canonical_values_match() {
        let prev = input();
        let next = input();
        assert!(are_messages_memo_inputs_equal(&prev, &next));
    }

    #[test]
    fn messages_memo_props_detect_message_or_streaming_changes() {
        let prev = input();
        let mut next = input();
        next.messages_identity = 2;
        assert!(!are_messages_memo_inputs_equal(&prev, &next));

        let mut next = input();
        next.streaming_tool_use_ids = vec!["toolu-3".into()];
        assert!(!are_messages_memo_inputs_equal(&prev, &next));
    }

    #[test]
    fn messages_memo_props_detect_tool_name_and_unseen_divider_changes() {
        let prev = input();
        let mut next = input();
        next.tool_names = vec!["Bash".into(), "Edit".into()];
        assert!(!are_messages_memo_inputs_equal(&prev, &next));

        let mut next = input();
        next.unseen_divider = Some(MessagesMemoUnseenDivider {
            first_unseen_uuid: "u-1".into(),
            count: 4,
        });
        assert!(!are_messages_memo_inputs_equal(&prev, &next));
    }

    #[test]
    fn messages_memo_props_use_set_equality_for_in_progress_ids() {
        let prev = input();
        let next = MessagesMemoInput {
            in_progress_tool_use_ids: BTreeSet::from(["toolu-1".into()]),
            ..input()
        };
        assert!(are_messages_memo_inputs_equal(&prev, &next));
    }
}
