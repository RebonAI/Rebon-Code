//! What waits for the session loop between turns.
//!
//! Inputs that arrive while nothing can take them wait here, keyed by where
//! they came from, until a turn carries them to the model. The inbox belongs
//! to the [`crate::session_loop::SessionLoop`], so it lives as long as the
//! session does rather than as long as one screen's state.
//!
//! Today it holds the context that rides into the next turn: MCP channel
//! notifications, teammate messages and messages from other agents. More
//! sources move in as the loop takes over deciding what runs next.

use std::sync::Mutex;

/// Where a piece of pending context came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextSource {
    /// A notification an MCP server pushed on its channel.
    Channel,
    /// A message from a teammate's mailbox.
    Teammate,
    /// A message another agent session sent this one. Never committed to the
    /// transcript: it is another agent's output, not something the person
    /// typed, and the hosted worker delivers it the same way.
    Agent,
}

#[derive(Default)]
struct PendingContext {
    channel: Vec<String>,
    teammate: Vec<String>,
    agent: Vec<String>,
}

impl PendingContext {
    fn queue(&mut self, source: ContextSource) -> &mut Vec<String> {
        match source {
            ContextSource::Channel => &mut self.channel,
            ContextSource::Teammate => &mut self.teammate,
            ContextSource::Agent => &mut self.agent,
        }
    }
}

/// Inputs waiting for the session's next turn.
#[derive(Default)]
pub struct SessionInbox {
    context: Mutex<PendingContext>,
}

impl SessionInbox {
    /// Add context for the next turn to carry.
    pub fn push_context(&self, source: ContextSource, prompts: impl IntoIterator<Item = String>) {
        self.lock().queue(source).extend(prompts);
    }

    /// Whether any context is waiting.
    pub fn has_context(&self) -> bool {
        let context = self.lock();
        !(context.channel.is_empty() && context.teammate.is_empty() && context.agent.is_empty())
    }

    /// Take everything waiting, for the turn about to start: channel
    /// notifications first, then teammate messages, then agent messages.
    pub fn take_context(&self) -> Vec<String> {
        let mut context = self.lock();
        let mut taken = std::mem::take(&mut context.channel);
        taken.append(&mut context.teammate);
        taken.append(&mut context.agent);
        taken
    }

    /// A copy of what is waiting from one source, leaving it in place.
    pub fn pending_context(&self, source: ContextSource) -> Vec<String> {
        self.lock().queue(source).clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PendingContext> {
        self.context.lock().expect("session inbox poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_is_taken_by_source_then_arrival() {
        let inbox = SessionInbox::default();
        assert!(!inbox.has_context());
        inbox.push_context(ContextSource::Agent, ["agent".to_string()]);
        inbox.push_context(ContextSource::Teammate, ["teammate".to_string()]);
        inbox.push_context(
            ContextSource::Channel,
            ["channel 1".to_string(), "channel 2".to_string()],
        );
        assert!(inbox.has_context());
        assert_eq!(inbox.pending_context(ContextSource::Teammate), ["teammate"]);

        assert_eq!(
            inbox.take_context(),
            ["channel 1", "channel 2", "teammate", "agent"]
        );
        assert!(!inbox.has_context());
        assert!(inbox.take_context().is_empty());
    }
}
