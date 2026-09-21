use rebon_render::UserContentBlock as RmUserContentBlock;

use crate::message::UserToolResultBlock;

pub(super) fn user_tool_result_content_block(
    tool_result: &UserToolResultBlock,
) -> RmUserContentBlock {
    RmUserContentBlock::ToolResult {
        tool_use_id: Some(tool_result.tool_use_id.clone()),
        content: Some(tool_result.content.as_display_string()),
        is_error: tool_result.is_error.unwrap_or(false),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use ratatui::layout::Rect;

    use super::super::{render_message, RenderTheme, ToolOutputVerbosity};
    use crate::message::{
        Message, ToolResultContent, UserContentBlock, UserMessage, UserMessageInner, UserRole,
        UserToolResultBlock,
    };
    use crate::render::tests::{all_text, new_buf};

    fn user_tool_result_message(
        uuid: &str,
        tool_use_id: &str,
        content: &str,
        is_error: Option<bool>,
    ) -> Message {
        Message::User(UserMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: UserMessageInner {
                role: UserRole::User,
                content: vec![UserContentBlock::ToolResult(UserToolResultBlock {
                    tool_use_id: tool_use_id.into(),
                    content: ToolResultContent::Text(content.into()),
                    is_error,
                })],
            },
            is_compact_summary: None,
            is_meta: None,
            is_visible_in_transcript_only: None,
            image_paste_ids: None,
            plan_content: None,
        })
    }

    #[test]
    fn user_tool_result_header_plus_body() {
        let msg = user_tool_result_message("u1", "toolu_42", "line one\nline two", Some(false));
        let mut buf = new_buf(40, 8);
        let used = render_message(
            &msg,
            Rect::new(0, 0, 40, 8),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Verbose,
        );
        assert!(used >= 2);
        let snap = all_text(&buf);
        assert!(snap.contains("toolu_42"), "expected tool_use_id: {snap:?}");
    }

    #[test]
    fn user_tool_result_error_uses_error_label() {
        let msg = user_tool_result_message("u1", "t", "boom", Some(true));
        let mut buf = new_buf(40, 6);
        render_message(
            &msg,
            Rect::new(0, 0, 40, 6),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Verbose,
        );
        let snap = all_text(&buf);
        assert!(
            snap.contains("error") || snap.contains("Error"),
            "expected error indicator: {snap:?}"
        );
    }
}
