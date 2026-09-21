//! Interaction helpers for transcript rows: whether a row can be clicked to
//! expand, and the text a transcript search matches against.

use crate::{TimelineContentBlock, TimelineMessage};

/// Input bag for [`is_item_clickable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemClickabilityInput {
    /// Current renderable row.
    pub message: TimelineMessage,
    /// Whether the assistant row has already been identified as an advisor
    /// result block of type `advisor_tool_result` carrying an
    /// `advisor_result` content type.
    pub assistant_is_advisor_result: bool,
    /// Whether the user tool-result row is flagged `is_error`.
    pub user_tool_result_is_error: bool,
    /// Whether the user row carries a structured tool-use result.
    pub has_user_tool_use_result: bool,
    /// Whether the linked tool reports its result as truncated; `None` when
    /// the tool gives no answer.
    pub linked_tool_result_is_truncated: Option<bool>,
}

/// Whether a row can be clicked to expand: collapsed read/search groups and
/// advisor results always, a user tool-result row only when it carries a
/// structured result that is not an error and the tool reports truncated.
pub fn is_item_clickable(input: &ItemClickabilityInput) -> bool {
    match &input.message {
        TimelineMessage::CollapsedReadSearch(_) => true,
        TimelineMessage::Assistant(_) => input.assistant_is_advisor_result,
        TimelineMessage::User(user) => {
            let has_tool_result = user
                .content
                .iter()
                .any(|block| matches!(block, TimelineContentBlock::ToolResult(_)));
            has_tool_result
                && !input.user_tool_result_is_error
                && input.has_user_tool_use_result
                && input.linked_tool_result_is_truncated.unwrap_or(false)
        }
        TimelineMessage::Attachment(_)
        | TimelineMessage::System(_)
        | TimelineMessage::GroupedToolUse(_) => false,
    }
}

/// Input bag for [`extract_search_text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTextExtractionInput {
    /// Current renderable row.
    pub message: TimelineMessage,
    /// The row's rendered text, used when no tool override applies.
    pub fallback_text: String,
    /// Whether the user row carries a structured tool-use result.
    pub has_user_tool_use_result: bool,
    /// Tool-provided override text.
    ///
    /// `None` means the tool implemented no override and returned
    /// nothing, so the fallback text stands. An explicit `Some("")` is
    /// respected and replaces the fallback text with an empty string.
    pub tool_extracted_text: Option<String>,
}

/// The lowercased text a transcript search matches against: the tool's
/// override for a user tool-result row when it supplies one, otherwise the
/// fallback text.
pub fn extract_search_text(input: &SearchTextExtractionInput) -> String {
    let mut text = input.fallback_text.clone();
    if matches!(&input.message, TimelineMessage::User(_)) && input.has_user_tool_use_result {
        let has_tool_result = matches!(
            input.message.first_block(),
            Some(TimelineContentBlock::ToolResult(_))
        );
        if has_tool_result {
            if let Some(extracted) = input.tool_extracted_text.as_ref() {
                text = extracted.clone();
            }
        }
    }
    text.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{
        AssistantTimelineMessage, CollapsedReadSearchTimelineMessage, TextBlock, ToolResultBlock,
        UserTimelineMessage,
    };

    fn assistant(uuid: &str) -> TimelineMessage {
        TimelineMessage::Assistant(AssistantTimelineMessage {
            uuid: uuid.into(),
            is_api_error_message: false,
            timestamp: None,
            model: None,
            content: vec![TimelineContentBlock::Text(TextBlock {
                text: "assistant".into(),
            })],
        })
    }

    fn user_tool_result(uuid: &str) -> TimelineMessage {
        TimelineMessage::User(UserTimelineMessage {
            uuid: uuid.into(),
            is_meta: false,
            source_tool_use_id: None,
            content: vec![TimelineContentBlock::ToolResult(ToolResultBlock {
                tool_use_id: Some("toolu-1".into()),
            })],
        })
    }

    #[test]
    fn collapsed_and_advisor_rows_are_clickable() {
        let collapsed = TimelineMessage::CollapsedReadSearch(CollapsedReadSearchTimelineMessage {
            uuid: "c-1".into(),
            tool_use_ids: vec!["toolu-1".into()],
        });
        assert!(is_item_clickable(&ItemClickabilityInput {
            message: collapsed,
            assistant_is_advisor_result: false,
            user_tool_result_is_error: false,
            has_user_tool_use_result: false,
            linked_tool_result_is_truncated: None,
        }));

        assert!(is_item_clickable(&ItemClickabilityInput {
            message: assistant("a-1"),
            assistant_is_advisor_result: true,
            user_tool_result_is_error: false,
            has_user_tool_use_result: false,
            linked_tool_result_is_truncated: None,
        }));
    }

    #[test]
    fn user_tool_result_clickability_requires_non_error_payload_and_truncated_tool_output() {
        let base = ItemClickabilityInput {
            message: user_tool_result("u-1"),
            assistant_is_advisor_result: false,
            user_tool_result_is_error: false,
            has_user_tool_use_result: true,
            linked_tool_result_is_truncated: Some(true),
        };
        assert!(is_item_clickable(&base));

        let mut rejected = base.clone();
        rejected.user_tool_result_is_error = true;
        assert!(!is_item_clickable(&rejected));

        let mut not_truncated = base.clone();
        not_truncated.linked_tool_result_is_truncated = Some(false);
        assert!(!is_item_clickable(&not_truncated));
    }

    #[test]
    fn plain_rows_are_not_clickable() {
        assert!(!is_item_clickable(&ItemClickabilityInput {
            message: assistant("a-2"),
            assistant_is_advisor_result: false,
            user_tool_result_is_error: false,
            has_user_tool_use_result: false,
            linked_tool_result_is_truncated: None,
        }));
    }

    #[test]
    fn extract_search_text_uses_tool_override_and_lowercases_result() {
        let extracted = extract_search_text(&SearchTextExtractionInput {
            message: user_tool_result("u-1"),
            fallback_text: "Fallback".into(),
            has_user_tool_use_result: true,
            tool_extracted_text: Some("MiXeD".into()),
        });
        assert_eq!(extracted, "mixed");

        let empty_override = extract_search_text(&SearchTextExtractionInput {
            message: user_tool_result("u-1"),
            fallback_text: "Fallback".into(),
            has_user_tool_use_result: true,
            tool_extracted_text: Some(String::new()),
        });
        assert_eq!(empty_override, "");
    }

    #[test]
    fn extract_search_text_keeps_fallback_when_tool_override_is_unavailable() {
        let fallback = extract_search_text(&SearchTextExtractionInput {
            message: user_tool_result("u-1"),
            fallback_text: "Fallback TEXT".into(),
            has_user_tool_use_result: true,
            tool_extracted_text: None,
        });
        assert_eq!(fallback, "fallback text");

        let assistant_fallback = extract_search_text(&SearchTextExtractionInput {
            message: assistant("a-3"),
            fallback_text: "Assistant TEXT".into(),
            has_user_tool_use_result: true,
            tool_extracted_text: Some("ignored".into()),
        });
        assert_eq!(assistant_fallback, "assistant text");
    }
}
