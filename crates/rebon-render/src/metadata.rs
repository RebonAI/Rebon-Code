//! Visibility gates and label projections for the model and timestamp
//! metadata shown beside an assistant message in transcript mode.

use rebon_width::WidthStr;

use crate::timeline::TimelineMessage;

/// Display text plus the minimum width the renderer should reserve for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataLabelProjection {
    /// Final display text.
    pub text: String,
    /// Minimum render width.
    pub min_width: usize,
}

/// True when transcript metadata applies to this message: transcript mode
/// is on, the assistant message carries a text block, and it has either a
/// timestamp or a model name.
pub fn has_transcript_metadata(message: &TimelineMessage, is_transcript_mode: bool) -> bool {
    let TimelineMessage::Assistant(assistant) = message else {
        return false;
    };
    is_transcript_mode
        && assistant.has_text_block()
        && (assistant.timestamp.is_some() || assistant.model.is_some())
}

/// Model label for an assistant message in transcript mode, reserving the
/// model name's display width plus 8.
pub fn project_message_model(
    message: &TimelineMessage,
    is_transcript_mode: bool,
) -> Option<MetadataLabelProjection> {
    let TimelineMessage::Assistant(assistant) = message else {
        return None;
    };
    let model = assistant.model.as_deref()?;
    if !is_transcript_mode || !assistant.has_text_block() {
        return None;
    }
    Some(MetadataLabelProjection {
        text: model.to_owned(),
        min_width: WidthStr::width(model) + 8,
    })
}

/// True when an assistant message in transcript mode carries both a
/// timestamp and a text block.
pub fn should_show_message_timestamp(message: &TimelineMessage, is_transcript_mode: bool) -> bool {
    let TimelineMessage::Assistant(assistant) = message else {
        return false;
    };
    is_transcript_mode && assistant.timestamp.is_some() && assistant.has_text_block()
}

/// Timestamp display projection with formatting injected as a seam.
pub fn project_message_timestamp(
    message: &TimelineMessage,
    is_transcript_mode: bool,
    formatted_timestamp: &str,
) -> Option<MetadataLabelProjection> {
    should_show_message_timestamp(message, is_transcript_mode).then(|| MetadataLabelProjection {
        text: formatted_timestamp.to_owned(),
        min_width: WidthStr::width(formatted_timestamp),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{AssistantTimelineMessage, TextBlock, TimelineContentBlock};

    fn assistant(
        timestamp: Option<&str>,
        model: Option<&str>,
        content: Vec<TimelineContentBlock>,
    ) -> TimelineMessage {
        TimelineMessage::Assistant(AssistantTimelineMessage {
            uuid: "a-1".into(),
            is_api_error_message: false,
            timestamp: timestamp.map(str::to_owned),
            model: model.map(str::to_owned),
            content,
        })
    }

    #[test]
    fn has_transcript_metadata_requires_assistant_text_and_transcript_mode() {
        let text = vec![TimelineContentBlock::Text(TextBlock {
            text: "hello".into(),
        })];
        let with_model = assistant(None, Some("claude-sonnet"), text.clone());
        assert!(has_transcript_metadata(&with_model, true));
        assert!(!has_transcript_metadata(&with_model, false));

        let thinking_only = assistant(
            Some("2026-04-08T00:00:00Z"),
            None,
            vec![TimelineContentBlock::Thinking],
        );
        assert!(!has_transcript_metadata(&thinking_only, true));
    }

    #[test]
    fn project_message_model_uses_unicode_width_plus_eight() {
        let projection = project_message_model(
            &assistant(
                None,
                Some("wide界"),
                vec![TimelineContentBlock::Text(TextBlock {
                    text: "hello".into(),
                })],
            ),
            true,
        )
        .unwrap();
        assert_eq!(projection.text, "wide界");
        assert_eq!(projection.min_width, WidthStr::width("wide界") + 8);
    }

    #[test]
    fn project_message_timestamp_is_hidden_without_timestamp_or_text() {
        let no_text = assistant(
            Some("2026-04-08T00:00:00Z"),
            None,
            vec![TimelineContentBlock::Thinking],
        );
        assert!(project_message_timestamp(&no_text, true, "01:23 PM").is_none());

        let with_text = assistant(
            Some("2026-04-08T00:00:00Z"),
            None,
            vec![TimelineContentBlock::Text(TextBlock {
                text: "hello".into(),
            })],
        );
        let projection = project_message_timestamp(&with_text, true, "01:23 PM").unwrap();
        assert_eq!(projection.text, "01:23 PM");
        assert_eq!(projection.min_width, WidthStr::width("01:23 PM"));
    }
}
