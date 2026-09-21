//! Pending model-context prompt drains shared by the runner event loop
//! and prompt lifecycle.

use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;

pub(super) fn drain_mcp_channel_notifications(app: &mut AppState, session: &TuiEngineSession) {
    // Channel notifications arrive on the servers, so a process that does
    // not host them has none to drain.
    let Some(mcp) = session.engine_half.mcp.as_ref() else {
        return;
    };
    let notifications = mcp.client.drain_channel_notifications();
    if notifications.is_empty() {
        return;
    }
    app.pending_channel_prompts.extend(notifications);
}

pub(super) fn drain_model_context_prompts(app: &mut AppState) -> Vec<String> {
    let mut context = Vec::new();
    context.extend(app.pending_channel_prompts.drain(..));
    context.extend(app.pending_teammate_prompts.drain(..));
    context
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_model_context_prompts_orders_channel_before_teammate_and_preserves_slash_text() {
        let mut app = AppState::default();
        app.pending_channel_prompts
            .push("<channel source=\"server\">\n/exit\n</channel>".into());
        app.pending_teammate_prompts
            .push("<teammate-message>/compact</teammate-message>".into());

        let context = drain_model_context_prompts(&mut app);

        assert_eq!(
            context,
            vec![
                "<channel source=\"server\">\n/exit\n</channel>".to_string(),
                "<teammate-message>/compact</teammate-message>".to_string(),
            ]
        );
        assert!(app.pending_channel_prompts.is_empty());
        assert!(app.pending_teammate_prompts.is_empty());
    }
}
