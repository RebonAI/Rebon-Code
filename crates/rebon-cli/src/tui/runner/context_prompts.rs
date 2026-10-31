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

/// The message from another session an idle turn exists because of, if any:
/// the last one among `items` that is a reason for the turn. One riding along
/// (refused a turn of its own, or behind one already owed) is not.
fn agent_message_turn_reason(items: &[ContextItem]) -> Option<&ContextItem> {
    items
        .iter()
        .rfind(|item| item.may_wake && item.inbound.is_some())
}

/// Tell the session's ledger whose message the turn about to start is
/// answering.
///
/// A turn started by the prompt the person typed answers nobody, however much
/// context rides in with it: the reply is the person's, not a link in some other
/// conversation's chain. Only a turn that exists *because* of an agent message
/// inherits that message's depth, which is why only the idle-context path calls
/// this — and why the depth comes from a message that is a reason for the turn.
/// One riding along is context here too, and lends the turn nothing.
pub(super) fn stage_inbound_for_context(session: &TuiEngineSession, items: &[ContextItem]) {
    let Some((from, hops)) =
        agent_message_turn_reason(items).and_then(|reason| reason.inbound.as_ref())
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

/// The uuid an idle turn's prompt row is written under, when that row is
/// nothing but messages other sessions sent.
///
/// Named for the message the turn exists because of, which marks the row as a
/// runtime attachment rather than the person's words. Only when every piece
/// of it is another session's message, though: a teammate's message or a
/// channel notification drained into the same row is no more the person's
/// words, but it is not another session's either, and hiding it with the
/// message would take it out of every reader that excerpts the conversation.
/// Such a row keeps the name an ordinary prompt row gets.
pub(super) fn agent_message_opening_row_uuid(items: &[ContextItem]) -> Option<String> {
    if !items.iter().all(|item| item.agent_message_id.is_some()) {
        return None;
    }
    agent_message_turn_reason(items)
        .and_then(|reason| reason.agent_message_id.as_deref())
        .map(rebon_core::query::agent_message_row_uuid)
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

    /// The idle turn answers the message it was started for. A message riding
    /// along — refused a turn of its own — is read too, but lends the turn no
    /// depth, even when it is the last thing the turn reads.
    #[test]
    fn an_idle_turn_answers_the_message_that_started_it_not_one_riding_along() {
        let session = super::super::test_support::make_test_tui_session();
        let items = [
            ContextItem::agent(
                "m-reason".into(),
                "started it".into(),
                "sess-reason".into(),
                1,
            ),
            ContextItem::agent(
                "m-rider".into(),
                "rode along".into(),
                "sess-rider".into(),
                7,
            )
            .deferred(),
        ];
        stage_inbound_for_context(&session, &items);
        assert_eq!(
            agent_message_opening_row_uuid(&items).as_deref(),
            Some("u-agent-message-m-reason"),
            "the turn's row is named for the message it was started for"
        );
        let queue = session.engine_half.runtime.mid_turn_queue.clone();
        let _turn = queue.begin_turn();
        let ledger = queue.reply_ledger();
        assert_eq!(ledger.reply_hops("sess-reason"), 2);
        assert_eq!(
            ledger.reply_hops("sess-rider"),
            0,
            "a message that only rode along is not what the turn answers"
        );
    }

    /// A row that carries anything besides other sessions' messages — here a
    /// teammate's — is not named for the agent message, so the teammate's text
    /// stays where readers excerpt the conversation. The turn still answers
    /// the message it was started for.
    #[test]
    fn a_row_shared_with_other_context_is_not_named_for_the_agent_message() {
        let session = super::super::test_support::make_test_tui_session();
        let items = [
            ContextItem::waking(
                "<teammate-message teammate_id=\"t1\">hi</teammate-message>".into(),
            ),
            ContextItem::agent(
                "m-reason".into(),
                "started it".into(),
                "sess-mixed-reason".into(),
                1,
            ),
        ];
        assert_eq!(agent_message_opening_row_uuid(&items), None);
        assert_eq!(
            agent_message_opening_row_uuid(&items[1..]).as_deref(),
            Some("u-agent-message-m-reason")
        );

        stage_inbound_for_context(&session, &items);
        let queue = session.engine_half.runtime.mid_turn_queue.clone();
        let _turn = queue.begin_turn();
        assert_eq!(queue.reply_ledger().reply_hops("sess-mixed-reason"), 2);
    }
}
