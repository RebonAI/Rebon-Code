//! Pending model-context prompt drains shared by the runner event loop
//! and prompt lifecycle.

use crate::session::session_inbox::{ContextItem, ContextSource};
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

/// The context a turn is about to carry, with what each item says about it.
pub(super) fn drain_model_context_items(session: &TuiEngineSession) -> Vec<ContextItem> {
    session
        .engine_half
        .session_loop
        .inbox()
        .take_context_items()
}

pub(super) fn drain_model_context_prompts(session: &TuiEngineSession) -> Vec<String> {
    drain_model_context_items(session)
        .into_iter()
        .map(|item| item.text)
        .collect()
}

/// Tell the session's ledger whose message the turn about to start is
/// answering.
///
/// A turn started by the prompt the person typed answers nobody, however much
/// context rides in with it: the reply is the person's, not a link in some other
/// conversation's chain. Only a turn that exists *because* of an agent message
/// inherits that message's depth, which is why only the idle-context path calls
/// this — and why the depth comes from the item the inbox kept rather than from
/// whatever arrived most recently.
pub(super) fn stage_inbound_for_context(session: &TuiEngineSession, items: &[ContextItem]) {
    let Some((from, hops)) = items
        .iter()
        .filter_map(|item| item.inbound.as_ref())
        .next_back()
    else {
        return;
    };
    session
        .engine_half
        .runtime
        .mid_turn_queue
        .reply_ledger()
        .stage_inbound(from, *hops);
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
