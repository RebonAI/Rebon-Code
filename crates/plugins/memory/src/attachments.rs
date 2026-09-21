//! The documents a turn walked into, read into the model's history.
//!
//! One attachment, `nested_memory`: when a tool touches a directory that
//! carries its own `REBON.md`, or the project's `REBON.md` changes mid-session,
//! the file's contents are inlined so the model reads the instructions before
//! it acts on the code they govern. It is the read half of this plugin — the
//! write half is [`SaveMemory`](crate::SaveMemoryTool), and the store both work
//! against is [`crate::memory`].
//!
//! It runs fourth of eight, behind the skill listing and ahead of the queued
//! prompts: this producer sits on the kernel's `attachment-producers` seat at
//! [`Order::Context`](rebon_core::attachment_seat::Order::Context), between
//! the listing's rung and the prompts'.
//!
//! **The eager path matters here.** Unlike every other producer this one also
//! answers the eager phase of [`AttachmentPoller::poll`], which the query loop
//! runs *before* the turn's first model request. A `REBON.md` edited between
//! turns has to be read before the model acts, not one tool round later, and
//! that eager phase also keeps message index 0 frozen — the trigger is appended
//! to history rather than rewritten into the system prompt. The seat forwards
//! the complete request unchanged, so this survived the move.
//!
//! **What stayed behind.** Finding the triggers is the engine's:
//! `query::session_prompt` diffs the document snapshot at turn start and the
//! executor hands the result over on the binding, so
//! [`NestedMemoryTrigger`] is `rebon_core::attachment_seat`'s like every
//! other binding payload. This module only renders them.

use std::sync::Arc;

#[cfg(test)]
use rebon_api::ContentBlock as ApiContentBlock;
use rebon_api::{make_meta_user_message, Message as ApiMessage};
use rebon_core::attachment_seat::{
    NestedMemoryTrigger, SeatAttachmentProducer, SessionAttachmentBinding, TurnDocumentTriggers,
};
use rebon_core::query::{AttachmentPollRequest, AttachmentPoller};

/// `nested_memory` attachments. Each trigger becomes one user message, in the
/// order the engine found them.
pub fn nested_memory(triggers: &[NestedMemoryTrigger]) -> Vec<ApiMessage> {
    triggers.iter().map(render_nested_memory_trigger).collect()
}

/// Render one nested-memory trigger as its attachment message.
pub fn render_nested_memory_trigger(trigger: &NestedMemoryTrigger) -> ApiMessage {
    let content = format!(
        "Contents of {}:\n\n{}",
        trigger.display_path, trigger.content
    );
    make_meta_user_message(&content)
}

/// [`AttachmentPoller`] for one turn's document finds.
///
/// It holds the handle, not the session record: a trigger is a fact about this
/// turn, found before it started, and nothing about it is stored on the
/// session. Draining is the delivery receipt — whichever poll asks first gets
/// them, and there is no second copy.
pub struct NestedMemoryAttachmentPoller {
    documents: Arc<dyn TurnDocumentTriggers>,
}

impl NestedMemoryAttachmentPoller {
    pub fn new(documents: Arc<dyn TurnDocumentTriggers>) -> Self {
        Self { documents }
    }
}

impl AttachmentPoller for NestedMemoryAttachmentPoller {
    /// Deliver pre-materialized triggers on the eager iteration-0 poll, while
    /// still accepting triggers raised after it on the next regular poll.
    fn poll(&self, _request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        nested_memory(&self.documents.drain_document_triggers())
    }
}

/// The seat entry.
///
/// A host that binds no document handle declines the turn: there is nothing to
/// drain, and a poller answering empty on every iteration is worth less than
/// not being in the turn at all.
pub struct NestedMemoryProducer;

impl SeatAttachmentProducer for NestedMemoryProducer {
    fn poller_for_session(
        &self,
        binding: &SessionAttachmentBinding,
    ) -> Option<Arc<dyn AttachmentPoller>> {
        let documents = binding.documents.clone()?;
        Some(Arc::new(NestedMemoryAttachmentPoller::new(documents)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::query::AttachmentPollPhase;
    use std::sync::Mutex;

    fn request(next_iteration: u64, phase: AttachmentPollPhase) -> AttachmentPollRequest<'static> {
        AttachmentPollRequest::new("session", "turn", next_iteration, phase)
    }

    fn only_text(messages: &[ApiMessage]) -> String {
        messages
            .iter()
            .flat_map(|m| {
                m.content.iter().filter_map(|b| match b {
                    ApiContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
            })
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    fn trigger(path: &str, content: &str) -> NestedMemoryTrigger {
        NestedMemoryTrigger {
            display_path: path.into(),
            content: content.into(),
        }
    }

    /// Holds one turn's finds and hands them over once, which is what the
    /// executor's handle does.
    struct Finds(Mutex<Vec<NestedMemoryTrigger>>);

    impl Finds {
        fn new(triggers: Vec<NestedMemoryTrigger>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(triggers)))
        }
    }

    impl TurnDocumentTriggers for Finds {
        fn drain_document_triggers(&self) -> Vec<NestedMemoryTrigger> {
            std::mem::take(&mut *self.0.lock().expect("finds mutex"))
        }
    }

    #[test]
    fn nested_memory_emits_one_message_per_trigger() {
        let messages = nested_memory(&[
            trigger("src/REBON.md", "hello"),
            trigger("docs/REBON.md", "world"),
        ]);
        assert_eq!(messages.len(), 2);
        let text = only_text(&messages);
        assert!(text.contains("src/REBON.md"));
        assert!(text.contains("hello"));
        assert!(text.contains("docs/REBON.md"));
        assert!(text.contains("world"));
    }

    #[test]
    fn nested_memory_is_a_noop_without_triggers() {
        assert!(nested_memory(&[]).is_empty());
    }

    /// The eager poll is the one that matters: a document edited between turns
    /// has to be read before the first model request, not a round later.
    #[test]
    fn the_poller_delivers_on_the_eager_poll_and_only_once() {
        let poller =
            NestedMemoryAttachmentPoller::new(Finds::new(vec![trigger("REBON.md", "the rules")]));

        let eager = poller.poll(request(0, AttachmentPollPhase::Eager));
        assert_eq!(eager.len(), 1);
        assert!(only_text(&eager).contains("the rules"));

        assert!(
            poller
                .poll(request(0, AttachmentPollPhase::Eager))
                .is_empty(),
            "a trigger is delivered once"
        );
        assert!(poller
            .poll(request(1, AttachmentPollPhase::Regular))
            .is_empty());
    }

    /// A trigger raised after the eager poll still reaches the model on the
    /// next round.
    #[test]
    fn a_later_find_rides_the_round_poll() {
        let finds = Finds::new(Vec::new());
        let poller = NestedMemoryAttachmentPoller::new(finds.clone());
        assert!(poller
            .poll(request(0, AttachmentPollPhase::Eager))
            .is_empty());

        *finds.0.lock().expect("finds mutex") = vec![trigger("src/REBON.md", "later")];
        let round = poller.poll(request(1, AttachmentPollPhase::Regular));
        assert_eq!(round.len(), 1);
        assert!(only_text(&round).contains("later"));
    }

    /// A host that binds no documents is not in the turn at all.
    #[test]
    fn a_binding_without_documents_declines_the_turn() {
        let binding = SessionAttachmentBinding::new(
            Arc::new(rebon_session_state::ServerState::new()),
            "sess-1",
        );
        assert!(NestedMemoryProducer.poller_for_session(&binding).is_none());
    }
}
