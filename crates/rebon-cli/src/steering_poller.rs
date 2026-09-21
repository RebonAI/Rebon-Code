//! `_session/steering` mailbox for the ACP **server** leg.
//!
//! When an editor drives Rebon over ACP, its steering requests land in the
//! [`rebon_acp::SteeringSink`] half of this type via the serve loop's bypass
//! worker. The [`AttachmentPoller`] half hands the queued messages to the
//! engine between tool rounds of the session's active prompt turn — the same
//! per-iteration injection contract the local TUI's mid-turn queue uses, so
//! the engine persists each message with `queuedCommand: true` and echoes a
//! `QueuedUserMessage` update without any code here touching transcripts.
//!
//! Delivery contract (enforced by the handler, relied on here):
//! - a message removed by [`AttachmentPoller::poll`] is *delivered* — the engine owns
//!   it from that point, even if the turn later fails;
//! - a message still queued when the turn ends is taken back through
//!   [`rebon_acp::SteeringSink::drain_pending`] and settled onto a fresh
//!   turn, so nothing is stranded.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use rebon_acp::{SteeringMessage, SteeringSink};
use rebon_api::{
    ContentBlock as ApiContentBlock, ImageBlock, Message as ApiMessage, Role, TextBlock,
};
#[cfg(test)]
use rebon_core::query::AttachmentPollPhase;
use rebon_core::query::{AttachmentPollRequest, AttachmentPoller};
use rebon_types::ContentBlock as AcpContentBlock;

/// Per-session queues of steering messages awaiting injection.
#[derive(Debug, Default)]
pub struct AcpSteeringPoller {
    queues: Mutex<HashMap<String, VecDeque<SteeringMessage>>>,
}

impl AcpSteeringPoller {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn take_messages(&self, session_id: &str) -> Vec<ApiMessage> {
        let drained = match self.queues.lock() {
            Ok(mut queues) => queues.remove(session_id),
            Err(_) => None,
        };
        drained
            .map(|queue| queue.iter().map(steering_message_to_attachment).collect())
            .unwrap_or_default()
    }
}

impl SteeringSink for AcpSteeringPoller {
    fn enqueue(&self, session_id: &str, messages: Vec<SteeringMessage>) {
        if messages.is_empty() {
            return;
        }
        {
            let mut queues = self
                .queues
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queues
                .entry(session_id.to_string())
                .or_default()
                .extend(messages);
        }
    }

    fn drain_pending(&self, session_id: &str) -> Vec<SteeringMessage> {
        match self.queues.lock() {
            Ok(mut queues) => queues.remove(session_id).map(Vec::from).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn requeue_front(&self, session_id: &str, messages: Vec<SteeringMessage>) {
        if messages.is_empty() {
            return;
        }
        {
            let mut queues = self
                .queues
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let queue = queues.entry(session_id.to_string()).or_default();
            for message in messages.into_iter().rev() {
                queue.push_front(message);
            }
        }
    }
}

/// Build the injectable user message: the converted content blocks plus the
/// trailing `<rebon-queued-user-input uuid>` marker. The marker routes the
/// message through the engine's queued-user branch — transcript line under
/// this uuid, model-side `<system-reminder>` wrapping, `QueuedUserMessage`
/// echo — exactly like a local mid-turn submit.
fn steering_message_to_attachment(message: &SteeringMessage) -> ApiMessage {
    let mut content = acp_blocks_to_api_content_blocks(&message.prompt);
    content.push(ApiContentBlock::Text(TextBlock {
        text: format!(
            "<rebon-queued-user-input uuid=\"{}\" />",
            message.user_message_uuid
        ),
    }));
    ApiMessage {
        role: Role::User,
        content,
    }
}

/// Text and image blocks convert; other ACP block kinds are dropped. This
/// mirrors the conversion `session/prompt` applies to its prompt blocks in
/// `rebon-core`, so a steered message can never carry *more* content
/// kinds than a prompted one.
fn acp_blocks_to_api_content_blocks(blocks: &[AcpContentBlock]) -> Vec<ApiContentBlock> {
    blocks
        .iter()
        .filter_map(|block| match block {
            AcpContentBlock::Text(text) => Some(ApiContentBlock::Text(TextBlock {
                text: text.text.clone(),
            })),
            AcpContentBlock::Image(image) => Some(ApiContentBlock::Image(ImageBlock::base64(
                image.mime_type.clone(),
                image.data.clone(),
            ))),
            _ => None,
        })
        .collect()
}

impl AttachmentPoller for AcpSteeringPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        self.take_messages(request.session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_types::{AudioContent, TextContent};

    fn request<'a>(
        session_id: &'a str,
        turn_id: &'a str,
        next_iteration: u64,
        phase: AttachmentPollPhase,
    ) -> AttachmentPollRequest<'a> {
        AttachmentPollRequest::new(session_id, turn_id, next_iteration, phase)
    }

    fn regular_request<'a>(
        session_id: &'a str,
        turn_id: &'a str,
        next_iteration: u64,
    ) -> AttachmentPollRequest<'a> {
        request(
            session_id,
            turn_id,
            next_iteration,
            AttachmentPollPhase::Regular,
        )
    }

    fn text_message(uuid: &str, text: &str) -> SteeringMessage {
        SteeringMessage {
            prompt: vec![AcpContentBlock::Text(TextContent {
                text: text.into(),
                annotations: None,
            })],
            user_message_uuid: uuid.into(),
        }
    }

    fn text_of(block: &ApiContentBlock) -> &str {
        match block {
            ApiContentBlock::Text(text) => &text.text,
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn contextual_poll_converts_with_trailing_marker_and_consumes() {
        let poller = AcpSteeringPoller::new();
        poller.enqueue("sess-1", vec![text_message("u-steer-1", "use tabs")]);

        let messages = poller.poll(regular_request("sess-1", "turn-1", 3));

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(text_of(&messages[0].content[0]), "use tabs");
        assert_eq!(
            text_of(&messages[0].content[1]),
            "<rebon-queued-user-input uuid=\"u-steer-1\" />"
        );
        assert!(poller
            .poll(regular_request("sess-1", "turn-1", 4))
            .is_empty());
        assert!(poller.drain_pending("sess-1").is_empty());
    }

    #[test]
    fn queues_are_isolated_per_session() {
        let poller = AcpSteeringPoller::new();
        poller.enqueue("sess-a", vec![text_message("u-a", "for a")]);
        poller.enqueue("sess-b", vec![text_message("u-b", "for b")]);

        assert!(poller
            .poll(regular_request("sess-other", "turn", 1))
            .is_empty());
        let for_a = poller.poll(regular_request("sess-a", "turn", 1));
        assert_eq!(for_a.len(), 1);
        assert_eq!(text_of(&for_a[0].content[0]), "for a");
        assert_eq!(poller.drain_pending("sess-b").len(), 1);
    }

    #[test]
    fn polling_wrong_sessions_never_leaks_queued_messages() {
        let poller = AcpSteeringPoller::new();
        poller.enqueue("sess-1", vec![text_message("u-1", "queued")]);

        assert!(poller
            .poll(regular_request("sess-other", "turn-1", 1))
            .is_empty());
        assert!(poller
            .poll(request(
                "sess-other",
                "turn-2",
                1,
                AttachmentPollPhase::Eager,
            ))
            .is_empty());
        // Still available to an eager poll carrying the exact session.
        assert_eq!(
            poller
                .poll(request("sess-1", "turn-1", 1, AttachmentPollPhase::Eager,))
                .len(),
            1
        );
    }

    #[test]
    fn drain_pending_returns_undelivered_in_fifo_order() {
        let poller = AcpSteeringPoller::new();
        poller.enqueue("sess-1", vec![text_message("u-1", "first")]);
        poller.enqueue("sess-1", vec![text_message("u-2", "second")]);

        let drained = poller.drain_pending("sess-1");

        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].user_message_uuid, "u-1");
        assert_eq!(drained[1].user_message_uuid, "u-2");
        assert!(poller.poll(regular_request("sess-1", "turn", 1)).is_empty());
    }

    #[test]
    fn requeue_front_restores_order_ahead_of_new_arrivals() {
        let poller = AcpSteeringPoller::new();
        poller.enqueue("sess-1", vec![text_message("u-1", "first")]);
        poller.enqueue("sess-1", vec![text_message("u-2", "second")]);
        let drained = poller.drain_pending("sess-1");
        poller.enqueue("sess-1", vec![text_message("u-3", "newer")]);

        poller.requeue_front("sess-1", drained);

        let order: Vec<String> = poller
            .drain_pending("sess-1")
            .into_iter()
            .map(|m| m.user_message_uuid)
            .collect();
        assert_eq!(order, vec!["u-1", "u-2", "u-3"]);
    }

    #[test]
    fn image_blocks_survive_conversion() {
        let poller = AcpSteeringPoller::new();
        poller.enqueue(
            "sess-1",
            vec![SteeringMessage {
                prompt: vec![
                    AcpContentBlock::Text(TextContent {
                        text: "look at this".into(),
                        annotations: None,
                    }),
                    AcpContentBlock::Image(rebon_types::ImageContent {
                        mime_type: "image/png".into(),
                        data: "aGk=".into(),
                        uri: None,
                        annotations: None,
                    }),
                ],
                user_message_uuid: "u-img".into(),
            }],
        );

        let messages = poller.poll(regular_request("sess-1", "turn", 1));

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content.len(), 3);
        assert!(matches!(
            &messages[0].content[1],
            ApiContentBlock::Image(image)
                if image.source.media_type == "image/png" && image.source.data == "aGk="
        ));
    }

    #[test]
    fn unsupported_blocks_are_dropped_but_marker_remains() {
        let poller = AcpSteeringPoller::new();
        poller.enqueue(
            "sess-1",
            vec![SteeringMessage {
                prompt: vec![AcpContentBlock::Audio(AudioContent {
                    mime_type: "audio/wav".into(),
                    data: "aGk=".into(),
                    annotations: None,
                })],
                user_message_uuid: "u-x".into(),
            }],
        );

        let messages = poller.poll(regular_request("sess-1", "turn", 1));

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content.len(), 1);
        assert!(text_of(&messages[0].content[0]).starts_with("<rebon-queued-user-input"));
    }
}
