//! Pending model-context prompt drains shared by the runner event loop
//! and prompt lifecycle.

use crate::session::session_inbox::ContextSource;
use crate::tui::wiring::TuiEngineSession;

pub(super) fn drain_mcp_channel_notifications(session: &TuiEngineSession) {
    // Channel notifications arrive on the servers, so a process that does
    // not host them has none to drain.
    let Some(mcp) = session.engine_half.mcp.as_ref() else {
        return;
    };
    let notifications = mcp.client.drain_channel_notifications();
    if notifications.is_empty() {
        return;
    }
    session
        .engine_half
        .session_loop
        .inbox()
        .push_context(ContextSource::Channel, notifications);
}

pub(super) fn drain_model_context_prompts(session: &TuiEngineSession) -> Vec<String> {
    session.engine_half.session_loop.inbox().take_context()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_model_context_prompts_orders_channel_before_teammate_and_preserves_slash_text() {
        let session = super::super::test_support::make_test_tui_session();
        let inbox = session.engine_half.session_loop.inbox();
        inbox.push_context(
            ContextSource::Teammate,
            ["<teammate-message>/compact</teammate-message>".to_string()],
        );
        inbox.push_context(
            ContextSource::Channel,
            ["<channel source=\"server\">\n/exit\n</channel>".to_string()],
        );

        let context = drain_model_context_prompts(&session);

        assert_eq!(
            context,
            vec![
                "<channel source=\"server\">\n/exit\n</channel>".to_string(),
                "<teammate-message>/compact</teammate-message>".to_string(),
            ]
        );
        assert!(!inbox.has_context());
    }
}
