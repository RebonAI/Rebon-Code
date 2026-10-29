//! What waits for the session loop between turns.
//!
//! Inputs that arrive while nothing can take them wait here, keyed by where
//! they came from, until a turn carries them to the model. The inbox belongs
//! to the [`crate::session_loop::SessionLoop`], so it lives as long as the
//! session does rather than as long as one screen's state.
//!
//! It holds the context that rides into the next turn (MCP channel
//! notifications, teammate messages, messages from other agents) and when
//! background-task notifications are next worth looking for. More sources
//! move in as the loop takes over deciding what runs next.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a turn that failed to deliver task notifications waits before
/// they are offered again.
pub const TASK_NOTIFICATION_RETRY: Duration = Duration::from_secs(10);

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

/// A kind of input that can start a turn while the session is idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyInput {
    /// Channel, teammate or agent context is waiting.
    Context,
    /// Background tasks may have notifications to deliver.
    TaskNotifications,
    /// Cron tasks fired with no turn to drain them.
    Scheduled,
}

/// The order idle inputs are offered in: what other parties sent this
/// session, then what its own background work reported, then the clock.
const IDLE_PRIORITY: [ReadyInput; 3] = [
    ReadyInput::Context,
    ReadyInput::TaskNotifications,
    ReadyInput::Scheduled,
];

/// When background-task notifications are next worth looking for.
///
/// Looking means asking the task registry what is unnotified, so it happens
/// only when something may have changed: the registry's revision moved, a
/// question escalation is waiting, or a failed delivery's retry came due.
struct TaskNotificationSchedule {
    look_pending: bool,
    retry_after: Option<Instant>,
    revision: Option<tokio::sync::watch::Receiver<u64>>,
}

impl Default for TaskNotificationSchedule {
    fn default() -> Self {
        Self {
            // Notifications can exist before anyone subscribed to changes.
            look_pending: true,
            retry_after: None,
            revision: None,
        }
    }
}

/// Inputs waiting for the session's next turn.
#[derive(Default)]
pub struct SessionInbox {
    context: Mutex<PendingContext>,
    task_notifications: Mutex<TaskNotificationSchedule>,
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

    /// Note what may have made task notifications ready: `revision` is the
    /// task registry's change counter, and `escalations_waiting` whether a
    /// question escalation is undelivered.
    pub fn observe_task_notifications(
        &self,
        revision: tokio::sync::watch::Receiver<u64>,
        escalations_waiting: bool,
    ) {
        let mut schedule = self.task_notifications();
        let changed = match schedule.revision.as_mut() {
            Some(current) if current.same_channel(&revision) => {
                let changed = current.has_changed().unwrap_or(false);
                if changed {
                    let _ = current.borrow_and_update();
                }
                changed
            }
            // A new registry, after a resume or a runtime swap: what it
            // already holds has never been looked at.
            _ => {
                let mut revision = revision;
                let _ = revision.borrow_and_update();
                schedule.revision = Some(revision);
                true
            }
        };
        let retry_due = schedule
            .retry_after
            .is_some_and(|deadline| Instant::now() >= deadline);
        if retry_due {
            schedule.retry_after = None;
        }
        if changed || escalations_waiting || retry_due {
            schedule.look_pending = true;
        }
    }

    /// Whether to look for task notifications now. Saying yes uses up the
    /// reason to look; a failed delivery's retry holds every look off.
    pub fn take_task_notification_look(&self) -> bool {
        let mut schedule = self.task_notifications();
        if schedule.retry_after.is_some() || !schedule.look_pending {
            return false;
        }
        schedule.look_pending = false;
        true
    }

    /// Notifications were delivered, or there is nothing left to retry.
    pub fn task_notifications_settled(&self) {
        self.task_notifications().retry_after = None;
    }

    /// Notifications were not delivered; offer them again after
    /// [`TASK_NOTIFICATION_RETRY`].
    pub fn retry_task_notifications_later(&self) {
        self.task_notifications().retry_after = Some(Instant::now() + TASK_NOTIFICATION_RETRY);
    }

    /// The inputs ready to start a turn, in [`IDLE_PRIORITY`] order.
    ///
    /// Lazy: each input is checked only when the iterator reaches it, so a
    /// caller that stops at the first turn it starts leaves the rest alone.
    /// Reaching [`ReadyInput::TaskNotifications`] uses up the reason to look
    /// for them (see [`Self::take_task_notification_look`]).
    pub fn ready_inputs<'a>(
        &'a self,
        cron: &'a rebon_core::cron::CronPoller,
    ) -> impl Iterator<Item = ReadyInput> + 'a {
        IDLE_PRIORITY.into_iter().filter(move |input| match input {
            ReadyInput::Context => self.has_context(),
            ReadyInput::TaskNotifications => self.take_task_notification_look(),
            ReadyInput::Scheduled => cron.has_pending(),
        })
    }

    /// Whether a failed delivery is waiting out its retry.
    pub fn task_notification_retry_pending(&self) -> bool {
        self.task_notifications().retry_after.is_some()
    }

    fn task_notifications(&self) -> std::sync::MutexGuard<'_, TaskNotificationSchedule> {
        self.task_notifications
            .lock()
            .expect("session inbox poisoned")
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

    #[test]
    fn task_notifications_are_looked_for_when_something_changed() {
        let inbox = SessionInbox::default();
        let (revision_tx, revision_rx) = tokio::sync::watch::channel(0u64);

        // A fresh inbox looks once, whatever has happened.
        assert!(inbox.take_task_notification_look());
        assert!(!inbox.take_task_notification_look());

        // The first registry seen counts as a change; nothing else does.
        inbox.observe_task_notifications(revision_rx.clone(), false);
        assert!(inbox.take_task_notification_look());
        inbox.observe_task_notifications(revision_rx.clone(), false);
        assert!(!inbox.take_task_notification_look());

        revision_tx.send(1).unwrap();
        inbox.observe_task_notifications(revision_rx.clone(), false);
        assert!(inbox.take_task_notification_look());

        inbox.observe_task_notifications(revision_rx.clone(), true);
        assert!(inbox.take_task_notification_look());
    }

    #[test]
    fn ready_inputs_come_in_priority_order_and_only_when_reached() {
        let inbox = SessionInbox::default();
        let cron = rebon_core::cron::CronPoller::default();
        assert!(inbox.take_task_notification_look());
        assert_eq!(inbox.ready_inputs(&cron).count(), 0);

        cron.enqueue("check the build".into());
        inbox.push_context(ContextSource::Teammate, ["hi".to_string()]);
        inbox.retry_task_notifications_later();
        inbox.task_notifications_settled();
        let (_revision_tx, revision_rx) = tokio::sync::watch::channel(0u64);
        inbox.observe_task_notifications(revision_rx, false);

        // Stopping at the first leaves the task-notification look unspent.
        assert_eq!(inbox.ready_inputs(&cron).next(), Some(ReadyInput::Context));
        assert_eq!(
            inbox.ready_inputs(&cron).collect::<Vec<_>>(),
            [
                ReadyInput::Context,
                ReadyInput::TaskNotifications,
                ReadyInput::Scheduled
            ]
        );
        // That pass spent it.
        assert_eq!(
            inbox.ready_inputs(&cron).collect::<Vec<_>>(),
            [ReadyInput::Context, ReadyInput::Scheduled]
        );
    }

    #[test]
    fn a_failed_delivery_holds_looks_off_until_its_retry() {
        let inbox = SessionInbox::default();
        let (revision_tx, revision_rx) = tokio::sync::watch::channel(0u64);
        inbox.observe_task_notifications(revision_rx.clone(), false);
        assert!(inbox.take_task_notification_look());

        inbox.retry_task_notifications_later();
        revision_tx.send(1).unwrap();
        inbox.observe_task_notifications(revision_rx.clone(), false);
        assert!(inbox.task_notification_retry_pending());
        assert!(!inbox.take_task_notification_look());

        inbox.task_notifications_settled();
        assert!(!inbox.task_notification_retry_pending());
        assert!(inbox.take_task_notification_look());
    }
}
