//! Row-level decisions for one transcript row: grouping, animation, static
//! rendering and metadata chrome.

use std::collections::BTreeSet;

use crate::{
    has_transcript_metadata, should_render_statically, TimelineLookups, TimelineMessage,
    TimelineScreen,
};

/// Input bag for [`project_message_row`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRowProjectionInput {
    /// The row as it sits in the transcript.
    pub message: TimelineMessage,
    /// The row to display, after grouped/collapsed display-message
    /// substitution.
    pub display_message: TimelineMessage,
    /// Whether later content follows this row.
    pub has_content_after: bool,
    /// Tool-use ids still running.
    pub in_progress_tool_use_ids: BTreeSet<String>,
    /// Tool-use ids whose input is still streaming.
    pub streaming_tool_use_ids: BTreeSet<String>,
    /// Sibling tool-use ids; the row renders statically only once all of
    /// them resolve.
    pub sibling_tool_use_ids: BTreeSet<String>,
    /// Which screen the transcript is drawn on.
    pub screen: TimelineScreen,
    /// Whether rows may animate at all.
    pub can_animate: bool,
    /// Terminal width in columns.
    pub columns: usize,
    /// Whether a turn is in flight.
    pub is_loading: bool,
    /// Precomputed tool-use lookups.
    pub lookups: TimelineLookups,
}

/// The row-level decisions handed to the message renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRowProjection {
    /// True when `screen` is `TimelineScreen::Transcript`.
    pub is_transcript_mode: bool,
    /// True when `message` is `TimelineMessage::GroupedToolUse`.
    pub is_grouped: bool,
    /// True when `message` is `TimelineMessage::CollapsedReadSearch`.
    pub is_collapsed: bool,
    /// Active spinner/present-tense state for collapsed read-search groups.
    pub is_active_collapsed_group: bool,
    /// Result of [`should_render_statically`].
    pub is_static: bool,
    /// Animation gate for in-flight rows.
    pub should_animate: bool,
    /// Whether transcript metadata chrome should be rendered.
    pub has_metadata: bool,
    /// Whether the message renderer adds its top margin.
    pub add_margin: bool,
    /// Width the message renderer lays out in; `None` when metadata chrome
    /// takes the row.
    pub container_width: Option<usize>,
}

/// Decide one row's grouping, active-group, static, animation and metadata
/// flags.
pub fn project_message_row(input: &MessageRowProjectionInput) -> MessageRowProjection {
    let is_transcript_mode = matches!(input.screen, TimelineScreen::Transcript);
    let is_grouped = matches!(input.message, TimelineMessage::GroupedToolUse(_));
    let is_collapsed = matches!(input.message, TimelineMessage::CollapsedReadSearch(_));

    let is_active_collapsed_group = is_collapsed
        && (message_has_any_tool_in_progress(&input.message, &input.in_progress_tool_use_ids)
            || input.is_loading && !input.has_content_after);

    let is_static = should_render_statically(
        &input.message,
        &input.streaming_tool_use_ids,
        &input.in_progress_tool_use_ids,
        &input.sibling_tool_use_ids,
        input.screen,
        &input.lookups,
    );

    let should_animate = if !input.can_animate {
        false
    } else if is_grouped || is_collapsed {
        message_has_any_tool_in_progress(&input.message, &input.in_progress_tool_use_ids)
    } else {
        input.message.tool_use_id().map_or(true, |tool_use_id| {
            input.in_progress_tool_use_ids.contains(tool_use_id)
        })
    };

    let has_metadata = has_transcript_metadata(&input.display_message, is_transcript_mode);
    let add_margin = !has_metadata;
    let container_width = (!has_metadata).then_some(input.columns);

    MessageRowProjection {
        is_transcript_mode,
        is_grouped,
        is_collapsed,
        is_active_collapsed_group,
        is_static,
        should_animate,
        has_metadata,
        add_margin,
        container_width,
    }
}

fn message_has_any_tool_in_progress(
    message: &TimelineMessage,
    in_progress_tool_use_ids: &BTreeSet<String>,
) -> bool {
    match message {
        TimelineMessage::GroupedToolUse(group) => group.messages.iter().any(|message| {
            message
                .content
                .first()
                .and_then(|block| match block {
                    crate::timeline::TimelineContentBlock::ToolUse(block) => {
                        Some(block.id.as_str())
                    }
                    _ => None,
                })
                .is_some_and(|tool_use_id| in_progress_tool_use_ids.contains(tool_use_id))
        }),
        TimelineMessage::CollapsedReadSearch(group) => group
            .tool_use_ids
            .iter()
            .any(|tool_use_id| in_progress_tool_use_ids.contains(tool_use_id.as_str())),
        _ => message
            .tool_use_id()
            .is_some_and(|tool_use_id| in_progress_tool_use_ids.contains(tool_use_id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{
        AssistantTimelineMessage, CollapsedReadSearchTimelineMessage,
        GroupedToolUseTimelineMessage, TextBlock, TimelineContentBlock, ToolUseBlock,
    };

    fn assistant(uuid: &str, blocks: Vec<TimelineContentBlock>) -> TimelineMessage {
        TimelineMessage::Assistant(AssistantTimelineMessage {
            uuid: uuid.into(),
            is_api_error_message: false,
            timestamp: Some("2026-04-08T12:00:00Z".into()),
            model: Some("claude-sonnet".into()),
            content: blocks,
        })
    }

    fn grouped(tool_use_id: &str) -> TimelineMessage {
        TimelineMessage::GroupedToolUse(GroupedToolUseTimelineMessage {
            uuid: "g-1".into(),
            tool_name: "Read".into(),
            messages: vec![AssistantTimelineMessage {
                uuid: "a-1".into(),
                is_api_error_message: false,
                timestamp: None,
                model: None,
                content: vec![TimelineContentBlock::ToolUse(ToolUseBlock {
                    id: tool_use_id.into(),
                    name: Some("Read".into()),
                    is_collapsible: false,
                })],
            }],
        })
    }

    #[test]
    fn grouped_and_collapsed_rows_compute_active_and_animation_flags() {
        let mut in_progress = BTreeSet::new();
        in_progress.insert("toolu-1".into());

        let grouped_message = grouped("toolu-1");
        let projection = project_message_row(&MessageRowProjectionInput {
            message: grouped_message.clone(),
            display_message: grouped_message,
            has_content_after: false,
            in_progress_tool_use_ids: in_progress.clone(),
            streaming_tool_use_ids: BTreeSet::new(),
            sibling_tool_use_ids: BTreeSet::new(),
            screen: TimelineScreen::Prompt,
            can_animate: true,
            columns: 100,
            is_loading: false,
            lookups: TimelineLookups::default(),
        });
        assert!(projection.is_grouped);
        assert!(projection.should_animate);
        assert!(!projection.is_active_collapsed_group);

        let collapsed_message =
            TimelineMessage::CollapsedReadSearch(CollapsedReadSearchTimelineMessage {
                uuid: "c-1".into(),
                tool_use_ids: vec!["toolu-1".into()],
            });
        let projection = project_message_row(&MessageRowProjectionInput {
            message: collapsed_message.clone(),
            display_message: collapsed_message,
            has_content_after: false,
            in_progress_tool_use_ids: in_progress,
            streaming_tool_use_ids: BTreeSet::new(),
            sibling_tool_use_ids: BTreeSet::new(),
            screen: TimelineScreen::Prompt,
            can_animate: true,
            columns: 100,
            is_loading: true,
            lookups: TimelineLookups::default(),
        });
        assert!(projection.is_collapsed);
        assert!(projection.is_active_collapsed_group);
        assert!(projection.should_animate);
    }

    #[test]
    fn transcript_metadata_disables_margin_and_container_width() {
        let message = assistant(
            "a-1",
            vec![TimelineContentBlock::Text(TextBlock {
                text: "hello".into(),
            })],
        );
        let projection = project_message_row(&MessageRowProjectionInput {
            message: message.clone(),
            display_message: message,
            has_content_after: false,
            in_progress_tool_use_ids: BTreeSet::new(),
            streaming_tool_use_ids: BTreeSet::new(),
            sibling_tool_use_ids: BTreeSet::new(),
            screen: TimelineScreen::Transcript,
            can_animate: false,
            columns: 120,
            is_loading: false,
            lookups: TimelineLookups::default(),
        });
        assert!(projection.has_metadata);
        assert!(!projection.add_margin);
        assert_eq!(projection.container_width, None);
        assert!(projection.is_transcript_mode);
    }

    #[test]
    fn plain_tool_use_row_animates_when_unresolved_and_stays_dynamic() {
        let message = assistant(
            "a-2",
            vec![TimelineContentBlock::ToolUse(ToolUseBlock {
                id: "toolu-2".into(),
                name: Some("Bash".into()),
                is_collapsible: false,
            })],
        );
        let mut in_progress = BTreeSet::new();
        in_progress.insert("toolu-2".into());
        let projection = project_message_row(&MessageRowProjectionInput {
            message: message.clone(),
            display_message: message,
            has_content_after: true,
            in_progress_tool_use_ids: in_progress,
            streaming_tool_use_ids: BTreeSet::new(),
            sibling_tool_use_ids: BTreeSet::new(),
            screen: TimelineScreen::Prompt,
            can_animate: true,
            columns: 80,
            is_loading: false,
            lookups: TimelineLookups::default(),
        });
        assert!(projection.should_animate);
        assert!(!projection.is_static);
        assert!(projection.add_margin);
        assert_eq!(projection.container_width, Some(80));
    }
}
